//! Child process plumbing: spawn, JSONL stdin, byte-level stdout reader,
//! stderr ring, and an exit monitor.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::AgentError;
use super::AgentEvent;
use super::config::LaunchPlan;
use super::job::JobHandle;
use super::parse::ParserState;
use super::shared::{Shared, lock};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// How long stop() waits for Pi to exit after stdin EOF before killing the job.
pub(crate) const STOP_GRACE: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const MONITOR_INTERVAL: Duration = Duration::from_millis(25);

/// Shared exit flags for one child. The monitor and the reader both use them.
#[derive(Debug, Default)]
pub(crate) struct ProcessFlags {
    /// stop()/cleanup() started: suppress error reporting and EOF turn flush.
    pub(crate) stopping: AtomicBool,
    /// try_wait observed an exit.
    pub(crate) exited: AtomicBool,
}

/// One live Pi child and its helper threads.
pub(crate) struct Process {
    child: Arc<Mutex<Child>>,
    stdin: Mutex<Option<ChildStdin>>,
    flags: Arc<ProcessFlags>,
    job: Option<JobHandle>,
    reader: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<()>>,
    monitor: Option<JoinHandle<()>>,
}

impl Process {
    /// Spawn Pi (or a test fixture) and start reading its pipes.
    pub(crate) fn spawn(
        plan: &LaunchPlan,
        cwd: &Path,
        shared: Arc<Shared>,
        generation: u64,
    ) -> Result<Self, AgentError> {
        let mut command = Command::new(&plan.program);
        command
            .args(&plan.args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(windows)]
        {
            // Do not flash a console window from the GUI.
            command.creation_flags(0x0800_0000);
            if let Some(raw) = &plan.raw_command_line {
                command.raw_arg(raw);
            }
        }
        #[cfg(not(windows))]
        {
            if let Some(raw) = &plan.raw_command_line {
                command.arg(raw);
            }
        }

        let mut child = command.spawn().map_err(|error| AgentError::Spawn {
            program: plan.program.clone(),
            message: error.to_string(),
        })?;

        let job = match JobHandle::create() {
            Ok(job) => match job.assign(&child) {
                Ok(()) => job,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(AgentError::JobObject(error));
                }
            },
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(AgentError::JobObject(error));
            }
        };

        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AgentError::Protocol(
                "pi child has no stdin/stdout/stderr pipes".to_owned(),
            ));
        };

        let child = Arc::new(Mutex::new(child));
        let flags = Arc::new(ProcessFlags::default());
        let mut job = Some(job);
        let mut threads: Vec<JoinHandle<()>> = Vec::new();

        let reader = {
            let shared = Arc::clone(&shared);
            let reader_flags = Arc::clone(&flags);
            match thread::Builder::new()
                .name("rustboard-pi-stdout".to_owned())
                .spawn(move || read_stdout(stdout, shared, reader_flags, generation))
            {
                Ok(handle) => handle,
                Err(error) => {
                    abort_spawn(&mut job, &child, &flags, threads);
                    return Err(AgentError::Io(error));
                }
            }
        };

        let stderr_reader = {
            let shared = Arc::clone(&shared);
            match thread::Builder::new()
                .name("rustboard-pi-stderr".to_owned())
                .spawn(move || read_stderr(stderr, shared))
            {
                Ok(handle) => handle,
                Err(error) => {
                    threads.push(reader);
                    abort_spawn(&mut job, &child, &flags, threads);
                    return Err(AgentError::Io(error));
                }
            }
        };

        let monitor = {
            let monitor_child = Arc::clone(&child);
            let monitor_flags = Arc::clone(&flags);
            let shared = Arc::clone(&shared);
            match thread::Builder::new()
                .name("rustboard-pi-monitor".to_owned())
                .spawn(move || monitor_exit(monitor_child, monitor_flags, shared, generation))
            {
                Ok(handle) => handle,
                Err(error) => {
                    threads.push(reader);
                    threads.push(stderr_reader);
                    abort_spawn(&mut job, &child, &flags, threads);
                    return Err(AgentError::Io(error));
                }
            }
        };

        Ok(Self {
            child,
            stdin: Mutex::new(Some(stdin)),
            flags,
            job,
            reader: Some(reader),
            stderr_reader: Some(stderr_reader),
            monitor: Some(monitor),
        })
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.flags.exited.load(Ordering::Acquire) || self.flags.stopping.load(Ordering::Acquire)
    }

    /// Write one JSONL record and flush it.
    pub(crate) fn write_record(&self, record: &Value) -> Result<(), AgentError> {
        let mut bytes = serde_json::to_vec(record)?;
        bytes.push(b'\n');
        let mut stdin = lock(&self.stdin);
        let stdin = stdin.as_mut().ok_or(AgentError::NotRunning)?;
        stdin.write_all(&bytes)?;
        stdin.flush()?;
        Ok(())
    }

    /// Graceful stop: close stdin, wait, then terminate the job and reaped the
    /// child. Always joins the helper threads.
    pub(crate) fn shutdown(mut self, grace: Duration) {
        self.close(grace);
    }

    fn close(&mut self, grace: Duration) {
        if self.job.is_none()
            && self.reader.is_none()
            && self.stderr_reader.is_none()
            && self.monitor.is_none()
        {
            return;
        }

        self.flags.stopping.store(true, Ordering::Release);
        let stdin = lock(&self.stdin).take();
        drop(stdin);

        let exited = wait_for_exit(&self.child, grace, &self.flags);

        if let Some(job) = self.job.take() {
            // Kill the whole tree even when the root exited cleanly: a tool
            // or MCP grandchild must not outlive the harness.
            job.terminate(1);
            drop(job);
        }

        if !exited {
            let _ = lock(&self.child).kill();
            let _ = wait_for_exit(&self.child, STOP_GRACE, &self.flags);
        }

        let _ = lock(&self.child).wait();
        join_thread(&mut self.reader);
        join_thread(&mut self.stderr_reader);
        join_thread(&mut self.monitor);
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // Any Process that escapes without shutdown() (panic, early return)
        // still closes the job handle and kills the tree.
        self.close(Duration::ZERO);
    }
}

fn abort_spawn(
    job: &mut Option<JobHandle>,
    child: &Arc<Mutex<Child>>,
    flags: &ProcessFlags,
    threads: Vec<JoinHandle<()>>,
) {
    flags.stopping.store(true, Ordering::Release);
    if let Some(job) = job.take() {
        job.terminate(1);
        drop(job);
    }
    {
        let mut child = lock(child);
        let _ = child.kill();
        let _ = child.wait();
    }
    for thread in threads {
        let _ = thread.join();
    }
}

fn read_stdout(
    stdout: ChildStdout,
    shared: Arc<Shared>,
    flags: Arc<ProcessFlags>,
    generation: u64,
) {
    let mut reader = BufReader::new(stdout);
    let mut parser = ParserState::default();
    let mut buffer = Vec::new();

    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => {
                shared.handle_event(
                    generation,
                    AgentEvent::Error(format!("pi stdout read failed: {error}")),
                );
                break;
            }
        }
        if flags.stopping.load(Ordering::Acquire) {
            break;
        }
        for event in parser.parse_line(&buffer, &shared) {
            shared.handle_event(generation, event);
        }
    }

    if !flags.stopping.load(Ordering::Acquire) {
        for event in parser.finish() {
            shared.handle_event(generation, event);
        }
    }
}

fn read_stderr(stderr: ChildStderr, shared: Arc<Shared>) {
    let mut reader = BufReader::new(stderr);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let line = String::from_utf8_lossy(&buffer);
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            continue;
        }
        let line: String = line.chars().take(1000).collect();
        shared.push_stderr(line);
    }
}

fn monitor_exit(
    child: Arc<Mutex<Child>>,
    flags: Arc<ProcessFlags>,
    shared: Arc<Shared>,
    generation: u64,
) {
    loop {
        if flags.stopping.load(Ordering::Acquire) {
            return;
        }
        let waited = {
            let mut child = lock(&child);
            child.try_wait()
        };
        match waited {
            Ok(Some(status)) => {
                flags.exited.store(true, Ordering::Release);
                if !flags.stopping.load(Ordering::Acquire) {
                    shared.report_exit(generation, &status);
                }
                return;
            }
            Ok(None) => thread::sleep(MONITOR_INTERVAL),
            Err(_) => {
                flags.exited.store(true, Ordering::Release);
                return;
            }
        }
    }
}

fn wait_for_exit(child: &Arc<Mutex<Child>>, timeout: Duration, flags: &ProcessFlags) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let waited = {
            let mut child = lock(child);
            child.try_wait()
        };
        match waited {
            Ok(Some(_)) => {
                flags.exited.store(true, Ordering::Release);
                return true;
            }
            Ok(None) => {}
            Err(_) => {
                flags.exited.store(true, Ordering::Release);
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn join_thread(handle: &mut Option<JoinHandle<()>>) {
    if let Some(handle) = handle.take() {
        let _ = handle.join();
    }
}

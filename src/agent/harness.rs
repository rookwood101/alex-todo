//! The `PiHarness` implementation and its end-to-end tests.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, unbounded};
use serde_json::{Value, json};
use uuid::Uuid;

use super::config::{PiConfig, PiProfile};
use super::process::{Process, STOP_GRACE};
use super::shared::{Shared, lock};
use super::{AgentError, AgentEvent, AgentHarness, AgentStatus};

/// Long-lived Pi process with a JSONL event stream.
///
/// The harness starts lazily on the first `send`, stays alive between turns,
/// restarts after an unexpected exit (up to three consecutive failures), and
/// kills the entire process tree on `stop` or drop.
pub struct PiHarness {
    config: PiConfig,
    shared: Arc<Shared>,
    receiver: Mutex<Receiver<AgentEvent>>,
    inner: Mutex<Inner>,
    lifecycle: Mutex<()>,
}

struct Inner {
    profile: PiProfile,
    process: Option<Process>,
}

impl PiHarness {
    /// Build a harness and its event channel. No process is started yet.
    pub fn new(config: PiConfig) -> Self {
        let (sender, receiver) = unbounded();
        let profile = config.profile;
        Self {
            config,
            shared: Arc::new(Shared::new(sender)),
            receiver: Mutex::new(receiver),
            inner: Mutex::new(Inner {
                profile,
                process: None,
            }),
            lifecycle: Mutex::new(()),
        }
    }

    /// The last lines Pi wrote to stderr (diagnostics only, never protocol).
    pub fn stderr_tail(&self) -> Vec<String> {
        self.shared.stderr_tail(40)
    }

    /// Configuration used for the next spawn.
    pub fn config(&self) -> &PiConfig {
        &self.config
    }

    /// Take the event receiver. Call once; cloning splits events between
    /// consumers, so the chat UI should own the only clone.
    pub fn events(&self) -> Receiver<AgentEvent> {
        lock(&self.receiver).clone()
    }

    /// Current lifecycle state.
    pub fn status(&self) -> AgentStatus {
        self.shared.status()
    }

    /// Explicitly (re)start Pi with the given profile, resetting failures.
    pub fn start(&self, profile: PiProfile) -> Result<(), AgentError> {
        let _lifecycle = lock(&self.lifecycle);
        self.start_locked(profile)
    }

    /// Send a prompt. Starts the child lazily and restarts a dead child.
    pub fn send(&self, prompt: &str) -> Result<(), AgentError> {
        let _lifecycle = lock(&self.lifecycle);
        self.ensure_running()?;

        let id = command_id();
        let record = json!({
            "id": id,
            "type": "prompt",
            "message": prompt,
        });
        self.shared.register_pending(&id, "prompt");
        // Set Busy before writing: a very fast child could otherwise settle
        // before this method resumes, leaving the harness stuck in Busy.
        self.shared.set_status(AgentStatus::Busy);

        match self.write_running(&record) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = self.shared.take_pending(&id);
                self.abandon_process();
                let _ = self
                    .shared
                    .note_failure(&format!("failed to write to pi: {error}"));
                Err(error)
            }
        }
    }

    /// Queue a steering message for the active run. Never starts a child.
    pub fn steer(&self, text: &str) -> Result<(), AgentError> {
        let _lifecycle = lock(&self.lifecycle);
        let id = command_id();
        let record = json!({
            "id": id,
            "type": "steer",
            "message": text,
        });
        self.shared.register_pending(&id, "steer");
        match self.write_running(&record) {
            Ok(()) => Ok(()),
            Err(AgentError::NotRunning) => {
                let _ = self.shared.take_pending(&id);
                Err(AgentError::NotRunning)
            }
            Err(error) => {
                let _ = self.shared.take_pending(&id);
                self.abandon_process();
                let _ = self
                    .shared
                    .note_failure(&format!("failed to steer pi: {error}"));
                Err(error)
            }
        }
    }

    /// Abort the active run. A no-op when no child is running.
    pub fn abort(&self) -> Result<(), AgentError> {
        let _lifecycle = lock(&self.lifecycle);
        let id = command_id();
        let record = json!({
            "id": id,
            "type": "abort",
        });
        self.shared.register_pending(&id, "abort");

        match self.write_running(&record) {
            Ok(()) => {
                self.shared.set_status(AgentStatus::Ready);
                Ok(())
            }
            Err(AgentError::NotRunning) => {
                let _ = self.shared.take_pending(&id);
                Ok(())
            }
            Err(error) => {
                let _ = self.shared.take_pending(&id);
                self.abandon_process();
                let _ = self
                    .shared
                    .note_failure(&format!("failed to abort pi: {error}"));
                Err(error)
            }
        }
    }

    /// Stop the child, close its stdin, and kill the process tree.
    pub fn stop(&self) -> Result<(), AgentError> {
        let _lifecycle = lock(&self.lifecycle);
        self.stop_locked();
        Ok(())
    }

    fn start_locked(&self, profile: PiProfile) -> Result<(), AgentError> {
        self.shared.reset_for_start();
        let previous = {
            let mut inner = lock(&self.inner);
            inner.profile = profile;
            inner.process.take()
        };
        if let Some(process) = previous {
            process.shutdown(STOP_GRACE);
        }
        self.spawn_process()
    }

    fn stop_locked(&self) {
        let process = lock(&self.inner).process.take();
        if let Some(process) = process {
            process.shutdown(STOP_GRACE);
        }
        self.shared.reset_after_stop();
    }

    fn ensure_running(&self) -> Result<(), AgentError> {
        let dead = {
            let mut inner = lock(&self.inner);
            let finished = inner
                .process
                .as_ref()
                .map(Process::is_finished)
                .unwrap_or(true);
            if !finished {
                return Ok(());
            }
            inner.process.take()
        };

        if let Some(process) = dead {
            // Unexpected exit already reported by the monitor; just join the
            // threads and close the job.
            process.shutdown(Duration::ZERO);
        }

        if self.shared.give_up() {
            return Err(AgentError::RestartExhausted);
        }
        let attempts = self.shared.attempts();
        if attempts > 0 {
            thread::sleep(backoff(attempts));
        }
        self.spawn_process()
    }

    fn spawn_process(&self) -> Result<(), AgentError> {
        self.shared.set_status(AgentStatus::Starting);
        let (profile, generation) = {
            let inner = lock(&self.inner);
            (inner.profile, self.shared.next_generation())
        };
        let plan = self.config.launch_plan_for(profile);
        self.shared.clear_pending();

        match Process::spawn(
            &plan,
            &self.config.board_home,
            Arc::clone(&self.shared),
            generation,
        ) {
            Ok(process) => {
                lock(&self.inner).process = Some(process);
                self.shared.set_status(AgentStatus::Ready);
                self.shared.emit(AgentEvent::Ready);
                Ok(())
            }
            Err(error) => {
                let _ = self.shared.note_failure(&error.to_string());
                Err(error)
            }
        }
    }

    fn write_running(&self, record: &Value) -> Result<(), AgentError> {
        let inner = lock(&self.inner);
        match inner.process.as_ref() {
            Some(process) if !process.is_finished() => process.write_record(record),
            _ => Err(AgentError::NotRunning),
        }
    }

    fn abandon_process(&self) {
        let process = lock(&self.inner).process.take();
        if let Some(process) = process {
            process.shutdown(Duration::ZERO);
        }
    }
}

impl AgentHarness for PiHarness {
    fn start(&self, profile: PiProfile) -> Result<(), AgentError> {
        PiHarness::start(self, profile)
    }

    fn send(&self, prompt: &str) -> Result<(), AgentError> {
        PiHarness::send(self, prompt)
    }

    fn steer(&self, text: &str) -> Result<(), AgentError> {
        PiHarness::steer(self, text)
    }

    fn abort(&self) -> Result<(), AgentError> {
        PiHarness::abort(self)
    }

    fn stop(&self) -> Result<(), AgentError> {
        PiHarness::stop(self)
    }

    fn status(&self) -> AgentStatus {
        PiHarness::status(self)
    }

    fn events(&self) -> Receiver<AgentEvent> {
        PiHarness::events(self)
    }
}

impl Drop for PiHarness {
    fn drop(&mut self) {
        let _lifecycle = lock(&self.lifecycle);
        let process = lock(&self.inner).process.take();
        if let Some(process) = process {
            process.shutdown(STOP_GRACE);
        }
        self.shared.reset_after_stop();
    }
}

fn command_id() -> String {
    format!("rustboard-{}", Uuid::new_v4())
}

fn backoff(attempts: u32) -> Duration {
    let shift = attempts.saturating_sub(1).min(4);
    Duration::from_millis(100 * (1u64 << shift))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::test_support::{
        aborting_child, looping_child, one_shot_child, scratch_dir, tree_child,
    };
    use std::path::Path;
    use std::time::Instant;

    const NORMAL: &str = r#"{"type":"agent_start"}
{"type":"message_update","assistantMessageEvent":{"type":"text_start","contentIndex":0}}
{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"Hello"}}
{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":" world"}}
{"type":"message_update","assistantMessageEvent":{"type":"text_end","contentIndex":0,"content":"Hello world"}}
{"type":"message_end","message":{"role":"assistant","stopReason":"stop"}}
{"type":"agent_end","willRetry":false}
{"type":"agent_settled","aborted":false}"#;

    const TOOLS: &str = r#"{"type":"agent_start"}
{"type":"tool_execution_start","toolCallId":"c1","toolName":"mcp__rustboard__add_note","args":{}}
{"type":"tool_execution_end","toolCallId":"c1","toolName":"mcp__rustboard__add_note","isError":false}
{"type":"tool_execution_start","toolCallId":"c2","toolName":"mcp__rustboard__board_sql","args":{}}
{"type":"tool_execution_end","toolCallId":"c2","toolName":"mcp__rustboard__board_sql","isError":true}
{"type":"agent_end","willRetry":false}
{"type":"agent_settled","aborted":false}"#;

    const ERROR: &str = r#"{"type":"error","message":"No API key found for deepseek."}
{"type":"agent_settled","aborted":false}"#;

    #[cfg(windows)]
    fn fixture_config(bat: &Path, home: &Path) -> PiConfig {
        let mut config = PiConfig::new(home, PiProfile::Board);
        config.executable = bat.display().to_string();
        config.command = Some("cmd.exe".to_owned());
        config.base_args = Some(vec!["/d".to_owned(), "/s".to_owned(), "/c".to_owned()]);
        config
    }

    fn recv(rx: &Receiver<AgentEvent>) -> AgentEvent {
        rx.recv_timeout(Duration::from_secs(10))
            .expect("agent event arrives")
    }

    fn collect_until_turn_end(rx: &Receiver<AgentEvent>) -> Vec<AgentEvent> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut events = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let event = match rx.recv_timeout(remaining) {
                Ok(event) => event,
                Err(error) => {
                    panic!("turn did not settle ({error}); collected so far: {events:?}")
                }
            };
            let done = matches!(event, AgentEvent::TurnEnd { .. });
            events.push(event);
            if done {
                return events;
            }
        }
    }

    fn wait_until(mut condition: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if condition() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(windows)]
    #[test]
    fn normal_turn_streams_events_in_order_and_stays_alive_between_turns() {
        let dir = scratch_dir("normal");
        let bat = looping_child(&dir, "normal", NORMAL);
        let harness = PiHarness::new(fixture_config(&bat, &dir));
        let events = harness.events();

        harness.start(PiProfile::Board).expect("start");
        assert_eq!(recv(&events), AgentEvent::Ready);
        assert_eq!(harness.status(), AgentStatus::Ready);

        harness.send("first").expect("send first prompt");
        assert_eq!(
            collect_until_turn_end(&events),
            vec![
                AgentEvent::TextDelta("Hello".to_owned()),
                AgentEvent::TextDelta(" world".to_owned()),
                AgentEvent::TurnEnd {
                    reason: "completed".to_owned()
                },
            ]
        );
        assert_eq!(harness.status(), AgentStatus::Ready);

        // The fake child is still waiting on stdin: no restart, same process.
        harness.send("second").expect("send second prompt");
        assert_eq!(
            collect_until_turn_end(&events),
            vec![
                AgentEvent::TextDelta("Hello".to_owned()),
                AgentEvent::TextDelta(" world".to_owned()),
                AgentEvent::TurnEnd {
                    reason: "completed".to_owned()
                },
            ]
        );

        harness.stop().expect("stop");
        assert_eq!(harness.status(), AgentStatus::Stopped);
    }

    #[cfg(windows)]
    #[test]
    fn tool_turn_maps_tool_chips() {
        let dir = scratch_dir("tools");
        let bat = looping_child(&dir, "tools", TOOLS);
        let harness = PiHarness::new(fixture_config(&bat, &dir));
        let events = harness.events();
        harness.start(PiProfile::Board).expect("start");
        assert_eq!(recv(&events), AgentEvent::Ready);
        harness.send("use a tool").expect("send");
        assert_eq!(
            collect_until_turn_end(&events),
            vec![
                AgentEvent::ToolStart {
                    name: "mcp__rustboard__add_note".to_owned()
                },
                AgentEvent::ToolEnd {
                    name: "mcp__rustboard__add_note".to_owned(),
                    ok: true
                },
                AgentEvent::ToolStart {
                    name: "mcp__rustboard__board_sql".to_owned()
                },
                AgentEvent::ToolEnd {
                    name: "mcp__rustboard__board_sql".to_owned(),
                    ok: false
                },
                AgentEvent::TurnEnd {
                    reason: "completed".to_owned()
                },
            ]
        );
        harness.stop().expect("stop");
    }

    #[cfg(windows)]
    #[test]
    fn auth_error_surfaces_as_an_event() {
        let dir = scratch_dir("error");
        let bat = looping_child(&dir, "error", ERROR);
        let harness = PiHarness::new(fixture_config(&bat, &dir));
        let events = harness.events();
        harness.start(PiProfile::Board).expect("start");
        assert_eq!(recv(&events), AgentEvent::Ready);
        harness.send("anything").expect("send");

        let collected = collect_until_turn_end(&events);
        assert!(collected.iter().any(|event| matches!(
            event,
            AgentEvent::Error(message) if message.contains("No API key")
        )));
        assert!(matches!(collected.last(), Some(AgentEvent::TurnEnd { .. })));
        harness.stop().expect("stop");
    }

    #[cfg(windows)]
    #[test]
    fn abort_writes_the_command_and_turn_ends_aborted() {
        let dir = scratch_dir("abort");
        let bat = aborting_child(&dir);
        let harness = PiHarness::new(fixture_config(&bat, &dir));
        let events = harness.events();
        harness.start(PiProfile::Board).expect("start");
        assert_eq!(recv(&events), AgentEvent::Ready);

        harness.send("count to 100").expect("send");
        assert_eq!(recv(&events), AgentEvent::TextDelta("Working".to_owned()));
        assert_eq!(harness.status(), AgentStatus::Busy);

        harness.abort().expect("abort");
        assert_eq!(harness.status(), AgentStatus::Ready);
        let settled = collect_until_turn_end(&events);
        assert_eq!(
            settled,
            vec![AgentEvent::TurnEnd {
                reason: "aborted".to_owned()
            }]
        );
        harness.stop().expect("stop");
    }

    #[cfg(windows)]
    #[test]
    fn failed_spawn_surfaces_as_an_error_event_not_a_panic() {
        let dir = scratch_dir("failed-spawn");
        let mut config = PiConfig::new(&dir, PiProfile::Board);
        config.command = Some("rustboard-definitely-missing-pi.exe".to_owned());
        config.base_args = Some(Vec::new());
        let harness = PiHarness::new(config);
        let events = harness.events();

        let error = harness
            .start(PiProfile::Board)
            .expect_err("spawn must fail");
        assert!(matches!(error, AgentError::Spawn { .. }));
        assert!(matches!(recv(&events), AgentEvent::Error(_)));
        assert_eq!(harness.status(), AgentStatus::Error);
    }

    #[cfg(windows)]
    #[test]
    fn clean_exit_between_turns_is_stopped_and_restarts_lazily() {
        let dir = scratch_dir("clean-exit");
        let bat = one_shot_child(&dir, "once", NORMAL);
        let harness = PiHarness::new(fixture_config(&bat, &dir));
        let events = harness.events();

        harness.start(PiProfile::Board).expect("start");
        assert_eq!(recv(&events), AgentEvent::Ready);
        harness.send("first").expect("send");
        assert!(matches!(
            collect_until_turn_end(&events).last(),
            Some(AgentEvent::TurnEnd { .. })
        ));
        assert!(
            wait_until(
                || harness.status() == AgentStatus::Stopped,
                Duration::from_secs(5)
            ),
            "a clean child exit becomes Stopped"
        );

        harness.send("second").expect("lazy restart");
        let restarted = collect_until_turn_end(&events);
        assert_eq!(restarted.first(), Some(&AgentEvent::Ready));
        assert!(matches!(restarted.last(), Some(AgentEvent::TurnEnd { .. })));
        harness.stop().expect("stop");
    }

    #[cfg(windows)]
    #[test]
    fn dropping_the_harness_kills_the_grandchild_tree() {
        let dir = scratch_dir("tree");
        let bat = tree_child(&dir, NORMAL);
        let harness = PiHarness::new(fixture_config(&bat, &dir));
        let events = harness.events();
        harness.start(PiProfile::Board).expect("start");
        assert_eq!(recv(&events), AgentEvent::Ready);
        harness.send("start a child").expect("send");
        assert!(matches!(
            collect_until_turn_end(&events).last(),
            Some(AgentEvent::TurnEnd { .. })
        ));

        let pid_file = dir.join("grandchild.pid");
        assert!(
            wait_until(|| pid_file.exists(), Duration::from_secs(10)),
            "grandchild writes its pid"
        );
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("pid file reads")
            .trim()
            .parse()
            .expect("pid is a number");
        assert!(process_alive(pid), "grandchild is initially alive");

        drop(harness);
        assert!(
            wait_until(|| !process_alive(pid), Duration::from_secs(10)),
            "Job Object killed the grandchild on drop"
        );
    }

    #[cfg(windows)]
    fn process_alive(pid: u32) -> bool {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

        // SAFETY: OpenProcess only queries the process; the returned handle is
        // closed immediately.
        unsafe {
            match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                Ok(handle) => {
                    let _ = CloseHandle(handle);
                    true
                }
                Err(_) => false,
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn events_are_delivered_in_order_by_the_receiver() {
        // The channel is unbounded and the reader is a single thread, so FIFO
        // is guaranteed; this asserts the public ordering contract end to end.
        let dir = scratch_dir("order");
        let bat = looping_child(&dir, "order", NORMAL);
        let harness = PiHarness::new(fixture_config(&bat, &dir));
        let events = harness.events();
        harness.start(PiProfile::Board).expect("start");
        assert_eq!(recv(&events), AgentEvent::Ready);
        harness.send("order").expect("send");
        let collected = collect_until_turn_end(&events);
        let mut text = String::new();
        for event in &collected {
            if let AgentEvent::TextDelta(delta) = event {
                text.push_str(delta);
            }
        }
        assert_eq!(text, "Hello world");
        harness.stop().expect("stop");
    }
}

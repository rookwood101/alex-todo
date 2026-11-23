//! State shared by the harness owner, the stdout reader, and the exit monitor.
//!
//! The reader thread is the only producer of transcript events. The monitor
//! thread reports unexpected exits. A generation counter makes late records
//! from a dead process harmless after a restart.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::ExitStatus;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use crossbeam_channel::Sender;

use super::{AgentEvent, AgentStatus};

/// Maximum consecutive unexpected exits or spawn failures before giving up.
pub(crate) const MAX_RESTARTS: u32 = 3;
/// Lines kept from stderr for error tails.
pub(crate) const STDERR_LINES: usize = 40;

pub(crate) struct Shared {
    status: AtomicU8,
    generation: AtomicU64,
    sender: Sender<AgentEvent>,
    stderr: Mutex<VecDeque<String>>,
    pending: Mutex<HashMap<String, String>>,
    restart_attempts: AtomicU32,
    give_up: AtomicBool,
    /// A prompt was sent and its TurnEnd has not been published yet.
    turn_active: AtomicBool,
    /// Optional diagnostics log (stderr lines and events).
    log: Mutex<Option<File>>,
}

impl Shared {
    pub(crate) fn new(sender: Sender<AgentEvent>) -> Self {
        Self {
            status: AtomicU8::new(encode_status(AgentStatus::Stopped)),
            generation: AtomicU64::new(0),
            sender,
            stderr: Mutex::new(VecDeque::new()),
            pending: Mutex::new(HashMap::new()),
            restart_attempts: AtomicU32::new(0),
            give_up: AtomicBool::new(false),
            turn_active: AtomicBool::new(false),
            log: Mutex::new(None),
        }
    }

    /// Append diagnostics to this file (created or appended).
    pub(crate) fn set_log(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        *lock(&self.log) = Some(file);
        Ok(())
    }

    pub(crate) fn log(&self, line: &str) {
        if let Some(file) = lock(&self.log).as_mut() {
            let _ = writeln!(file, "{} {line}", crate::store::now_utc_ms());
        }
    }

    /// Mark that a prompt is in flight.
    pub(crate) fn begin_turn(&self) {
        self.turn_active.store(true, Ordering::Release);
    }

    /// Publish a TurnEnd when a prompt is still in flight, so the UI never
    /// waits forever after a crash, a rejected prompt, a stop or a restart.
    pub(crate) fn end_turn_if_active(&self, reason: &str) {
        if self.turn_active.swap(false, Ordering::AcqRel) {
            self.log(&format!(
                "event TurnEnd {{ reason: {reason:?} }} (synthesised)"
            ));
            self.emit(AgentEvent::TurnEnd {
                reason: reason.to_owned(),
            });
        }
    }

    /// A new send after "giving up" is an explicit retry: allow it.
    pub(crate) fn allow_retry(&self) {
        if self.give_up() {
            self.reset_failures();
        }
    }

    pub(crate) fn emit(&self, event: AgentEvent) {
        let _ = self.sender.send(event);
    }

    pub(crate) fn set_status(&self, status: AgentStatus) {
        self.status.store(encode_status(status), Ordering::Release);
    }

    pub(crate) fn status(&self) -> AgentStatus {
        decode_status(self.status.load(Ordering::Acquire))
    }

    pub(crate) fn next_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub(crate) fn register_pending(&self, id: &str, command: &str) {
        let mut pending = lock(&self.pending);
        pending.insert(id.to_owned(), command.to_owned());
    }

    pub(crate) fn take_pending(&self, id: &str) -> Option<String> {
        lock(&self.pending).remove(id)
    }

    pub(crate) fn clear_pending(&self) {
        lock(&self.pending).clear();
    }

    pub(crate) fn push_stderr(&self, line: String) {
        self.log(&format!("stderr {line}"));
        let mut lines = lock(&self.stderr);
        if lines.len() >= STDERR_LINES {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    pub(crate) fn stderr_tail(&self, limit: usize) -> Vec<String> {
        let lines = lock(&self.stderr);
        let start = lines.len().saturating_sub(limit);
        lines.iter().skip(start).cloned().collect()
    }

    pub(crate) fn clear_stderr(&self) {
        lock(&self.stderr).clear();
    }

    pub(crate) fn attempts(&self) -> u32 {
        self.restart_attempts.load(Ordering::Acquire)
    }

    pub(crate) fn give_up(&self) -> bool {
        self.give_up.load(Ordering::Acquire)
    }

    fn reset_failures(&self) {
        self.restart_attempts.store(0, Ordering::Release);
        self.give_up.store(false, Ordering::Release);
    }

    /// Fresh explicit start: clear per-process state but keep the channel.
    pub(crate) fn reset_for_start(&self) {
        self.reset_failures();
        self.clear_pending();
        self.clear_stderr();
        self.set_status(AgentStatus::Starting);
    }

    /// Intentional stop: keep the stderr ring for diagnostics, but clear the
    /// restart budget and pending correlation state.
    pub(crate) fn reset_after_stop(&self) {
        self.reset_failures();
        self.clear_pending();
        self.set_status(AgentStatus::Stopped);
    }

    /// A completed turn proves the child is healthy again.
    pub(crate) fn note_turn_end(&self) {
        self.reset_failures();
        self.set_status(AgentStatus::Ready);
    }

    /// Count a spawn/write failure and return the new attempt count.
    pub(crate) fn note_failure(&self, message: &str) -> u32 {
        self.set_status(AgentStatus::Error);
        self.clear_pending();
        let attempts = self.restart_attempts.fetch_add(1, Ordering::AcqRel) + 1;
        let tail = self.stderr_suffix();
        if attempts >= MAX_RESTARTS {
            self.give_up.store(true, Ordering::Release);
            self.emit(AgentEvent::Error(format!(
                "{message} (giving up after {MAX_RESTARTS} failed attempts){tail}"
            )));
        } else {
            self.emit(AgentEvent::Error(format!(
                "{message} (attempt {attempts}/{MAX_RESTARTS}){tail}"
            )));
        }
        self.end_turn_if_active("error");
        attempts
    }

    /// Called by the monitor thread when the child exits without stop().
    pub(crate) fn report_exit(&self, generation: u64, status: &ExitStatus) {
        if generation != self.generation.load(Ordering::Acquire) {
            return;
        }
        if status.success() {
            self.reset_failures();
            self.set_status(AgentStatus::Stopped);
            self.end_turn_if_active("error");
            return;
        }

        self.set_status(AgentStatus::Error);
        let attempts = self.restart_attempts.fetch_add(1, Ordering::AcqRel) + 1;
        let tail = self.stderr_suffix();
        if attempts >= MAX_RESTARTS {
            self.give_up.store(true, Ordering::Release);
            self.emit(AgentEvent::Error(format!(
                "pi exited unexpectedly ({status}); giving up after {MAX_RESTARTS} restart attempts{tail}"
            )));
        } else {
            self.emit(AgentEvent::Error(format!(
                "pi exited unexpectedly ({status}); will restart on the next message (attempt {attempts}/{MAX_RESTARTS}){tail}"
            )));
        }
        self.log(&format!("exit {status}"));
        self.end_turn_if_active("error");
    }

    /// Apply the status effects of a transcript event, then publish it.
    ///
    /// Generation checks discard records from a previous child after restart.
    pub(crate) fn handle_event(&self, generation: u64, event: AgentEvent) {
        if generation != self.generation.load(Ordering::Acquire) {
            return;
        }
        if !matches!(event, AgentEvent::TextBreak) {
            match &event {
                AgentEvent::TextDelta(text) => self.log(&format!("text {text:?}")),
                other => self.log(&format!("event {other:?}")),
            }
        }
        match &event {
            AgentEvent::TurnEnd { .. } => {
                self.turn_active.store(false, Ordering::Release);
                self.note_turn_end();
            }
            AgentEvent::Error(_) if self.status() != AgentStatus::Stopped => {
                self.set_status(AgentStatus::Error);
            }
            _ => {}
        }
        self.emit(event);
    }

    fn stderr_suffix(&self) -> String {
        let mut lines = self.stderr_tail(8);
        if lines.is_empty() {
            return String::new();
        }
        for line in &mut lines {
            if line.chars().count() > 300 {
                *line = line.chars().take(297).collect::<String>() + "...";
            }
        }
        format!("\nstderr: {}", lines.join(" | "))
    }
}

fn encode_status(status: AgentStatus) -> u8 {
    match status {
        AgentStatus::Stopped => 0,
        AgentStatus::Starting => 1,
        AgentStatus::Ready => 2,
        AgentStatus::Busy => 3,
        AgentStatus::Error => 4,
    }
}

fn decode_status(code: u8) -> AgentStatus {
    match code {
        1 => AgentStatus::Starting,
        2 => AgentStatus::Ready,
        3 => AgentStatus::Busy,
        4 => AgentStatus::Error,
        _ => AgentStatus::Stopped,
    }
}

/// Lock a mutex, recovering from a panic in another thread instead of
/// propagating the poison as a second panic.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> Shared {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        Shared::new(sender)
    }

    fn failed_exit() -> ExitStatus {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            ExitStatus::from_raw(1 << 8)
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::ExitStatusExt;
            ExitStatus::from_raw(1)
        }
    }

    #[test]
    fn a_crash_or_failure_mid_turn_always_ends_the_turn() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let shared = Shared::new(sender);
        let generation = shared.next_generation();

        shared.begin_turn();
        shared.report_exit(generation, &failed_exit());
        let events: Vec<AgentEvent> = receiver.try_iter().collect();
        assert!(matches!(events.first(), Some(AgentEvent::Error(_))));
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnEnd { reason }) if reason == "error"
        ));

        shared.begin_turn();
        let _ = shared.note_failure("spawn failed");
        assert!(
            receiver
                .try_iter()
                .any(|event| matches!(event, AgentEvent::TurnEnd { .. }))
        );

        shared.end_turn_if_active("aborted");
        assert_eq!(receiver.try_iter().count(), 0, "no turn, no TurnEnd");

        shared.begin_turn();
        shared.handle_event(
            shared.next_generation(),
            AgentEvent::TurnEnd {
                reason: "completed".to_owned(),
            },
        );
        shared.end_turn_if_active("aborted");
        let ends = receiver
            .try_iter()
            .filter(|event| matches!(event, AgentEvent::TurnEnd { .. }))
            .count();
        assert_eq!(ends, 1, "a real TurnEnd is never doubled");
    }

    #[test]
    fn a_retry_after_giving_up_is_allowed() {
        let shared = shared();
        for _ in 0..MAX_RESTARTS {
            let _ = shared.note_failure("boom");
        }
        assert!(shared.give_up());
        shared.allow_retry();
        assert!(!shared.give_up());
        assert_eq!(shared.attempts(), 0);
    }

    #[test]
    fn status_round_trips() {
        let shared = shared();
        for status in [
            AgentStatus::Stopped,
            AgentStatus::Starting,
            AgentStatus::Ready,
            AgentStatus::Busy,
            AgentStatus::Error,
        ] {
            shared.set_status(status);
            assert_eq!(shared.status(), status);
        }
    }

    #[test]
    fn pending_responses_correlate_by_id() {
        let shared = shared();
        shared.register_pending("p1", "prompt");
        assert_eq!(shared.take_pending("p1").as_deref(), Some("prompt"));
        assert_eq!(shared.take_pending("p1"), None);
    }

    #[test]
    fn stderr_ring_is_bounded() {
        let shared = shared();
        for index in 0..(STDERR_LINES + 10) {
            shared.push_stderr(format!("line {index}"));
        }
        let tail = shared.stderr_tail(STDERR_LINES);
        assert_eq!(tail.len(), STDERR_LINES);
        assert_eq!(tail.first().map(String::as_str), Some("line 10"));
        assert_eq!(
            tail.last().map(String::as_str),
            Some(format!("line {}", STDERR_LINES + 9).as_str())
        );
    }
}

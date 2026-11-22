//! Pi RPC bridge: a long-lived Pi child process with a typed event stream.
//!
//! The bridge owns one Pi process per mode (board or power), writes JSONL
//! commands on stdin, and turns Pi's stdout records into [`AgentEvent`]s for
//! the chat UI. stderr is diagnostics only: it is kept in a bounded ring
//! buffer and never parsed as protocol.
//!
//! ```text
//! 130 chat UI --send/steer/abort--> AgentHarness --JSONL--> pi --mode rpc
//!             <--events()-----------              <--JSONL--
//! ```
//!
//! The implementation is deliberately Windows-first: Pi is a `.cmd`/`.ps1`
//! shim, so the process is launched through `cmd.exe /d /s /c`, and a Job
//! Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` guarantees that stopping
//! or dropping the harness kills Pi and every descendant (MCP server, tools).
//! Other platforms compile with a direct spawn path so the type checks and
//! parser tests remain portable.

mod config;
mod harness;
mod job;
mod mcp;
mod parse;
mod process;
mod shared;
#[cfg(test)]
mod test_support;

pub use config::PiConfig;
pub use harness::PiHarness;
pub use mcp::ensure_mcp_config;

use crossbeam_channel::Receiver;
use thiserror::Error;

/// One item in the agent transcript.
///
/// Events are produced by the harness reader thread in the order Pi sent
/// them. [`AgentEvent::Ready`] is emitted once per successful spawn (including
/// restarts); consumers should treat it as "the composer may send now".
#[derive(Clone, Debug, PartialEq)]
pub enum AgentEvent {
    /// The Pi process started and its stdin/stdout pipes are ready.
    Ready,
    /// An incremental assistant text fragment. `text_end` records do not
    /// repeat text that was already streamed as deltas.
    TextDelta(String),
    /// A tool call began executing.
    ToolStart { name: String },
    /// A tool call finished. `ok` is false when Pi reported `isError: true`.
    ToolEnd { name: String, ok: bool },
    /// Pi settled after a turn. `reason` is `completed`, `aborted`, or
    /// `error`.
    TurnEnd { reason: String },
    /// Token/cost usage reported by the provider. Fields may be absent when
    /// the provider does not report them.
    Cost {
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cost_usd: Option<f64>,
    },
    /// A spawn failure, protocol error, auth error, or unexpected exit.
    /// The harness keeps running where possible; the UI should display this.
    Error(String),
}

/// Lifecycle state of an [`AgentHarness`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    /// No process is running (never started, stopped, or exited cleanly).
    Stopped,
    /// A spawn attempt is in flight.
    Starting,
    /// The process is alive and idle.
    Ready,
    /// A prompt is being processed.
    Busy,
    /// The last operation failed; a later send may restart the process.
    Error,
}

/// Typed errors returned by harness operations.
#[derive(Debug, Error)]
pub enum AgentError {
    /// The child process could not be created.
    #[error("failed to spawn {program}: {message}")]
    Spawn { program: String, message: String },
    /// The operation needs a live child process.
    #[error("pi is not running")]
    NotRunning,
    /// Three consecutive unexpected exits or spawn failures happened.
    #[error("pi restart limit reached; call start to try again")]
    RestartExhausted,
    /// An I/O operation failed.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON serialization or parsing failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// The child sent a record the bridge cannot use.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// Windows Job Object setup or termination failed.
    #[error("job object error: {0}")]
    JobObject(String),
    /// The Pi MCP configuration could not be written or read.
    #[error("mcp configuration error: {0}")]
    Mcp(String),
}

/// The two tool profiles exposed by the app.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PiProfile {
    /// Board tools only: no shell or filesystem built-ins.
    #[default]
    Board,
    /// Board tools plus the opt-in OS tools. The UI must warn before enabling.
    Power,
}

/// Object-safe interface used by the chat UI and by headless tests.
///
/// Implementations must be `Send + Sync` so the app can share one harness
/// between the UI thread and a background sender.
pub trait AgentHarness: Send + Sync {
    /// Start (or restart) the child with the given profile.
    fn start(&self, profile: PiProfile) -> Result<(), AgentError>;
    /// Send a user prompt. Starts the child lazily on the first message.
    fn send(&self, prompt: &str) -> Result<(), AgentError>;
    /// Queue a steering message while a run is active.
    fn steer(&self, text: &str) -> Result<(), AgentError>;
    /// Abort the active run (maps to the Stop button).
    fn abort(&self) -> Result<(), AgentError>;
    /// Stop the child and kill its process tree.
    fn stop(&self) -> Result<(), AgentError>;
    /// Current lifecycle state.
    fn status(&self) -> AgentStatus;
    /// Event receiver. Call once and fan out if several consumers are needed:
    /// cloning the returned receiver splits events between clones.
    fn events(&self) -> Receiver<AgentEvent>;
}

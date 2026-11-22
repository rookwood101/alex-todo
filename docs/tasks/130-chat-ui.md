# 130 — Chat panel above the composer

> **Read [000-index](000-index.md) first.** It carries the decisions, write-set
> ownership, dependency graph, and gates this task assumes.

**Goal:** show the Pi conversation in the app: streaming text, tool activity, stop, cost, and
"undo this turn". The transport is 120; this task is presentation and turn bookkeeping.

**Depends on:** 050 (batch range), 120 (events), 090 (composer). **Parallel with:** 150/160.
**Owns:** `src/ui/chat.rs`; one `ui::chat::show(...)` call in `src/app.rs` (coordinate with 170).

**Read first:** `src/agent/**`; `src/history.rs`; `src/ui/composer.rs`.

## Steps

1. `ChatState { messages: Vec<ChatMessage>, streaming: String, tool_chips: Vec<ToolChip>,
   busy: bool, mode: Board|Power, last_turn_batches: Vec<BatchId>, error: Option<String> }`.
2. Render a scrollable panel above the composer, capped at ~40 % of the window. Assistant text
   streams; tool calls appear as compact chips (`board_apply · 3 notes`, `board_sql · ok`).
   Empty state explains what the agent can do. Power mode shows a warning banner.
3. Wire `AgentEvent`: `TextDelta` appends; `ToolStart/ToolEnd` update chips; `TurnEnd` clears
   `busy`, records the turn's batch range; `Error` shows an inline error with the stderr tail.
4. Turn bookkeeping: on send, record `turn_started_at_utc_ms`; on `TurnEnd`, call a
   `history` helper that returns agent batches in `[start, now]` (actor `agent`, `kind='apply'`)
   and store the ids. "Undo this turn" calls `undo_batches(ids)`; refresh the board; show a toast.
5. Controls: Stop → `harness.abort()`; mode badge; token/cost from the bridge when available
   (`get_session_stats`); a "clear" button clears the transcript only.
6. Headless tests with fixture `AgentEvent`s: text appends and settles; tool chips appear;
   stop calls abort; undo-turn passes the expected batch ids; errors render without panicking;
   an empty turn is harmless.

## Acceptance

- `cargo test`/`clippy`/`fmt` clean; no live model calls.
- Long streams scroll smoothly and do not steal composer focus.
- "Undo this turn" is disabled while idle with no turn.

## Out of scope

Bridge process management (120), MCP tools (110), persisted chat history (Pi sessions handle
that), markdown rendering beyond plain text, action buttons inside messages.

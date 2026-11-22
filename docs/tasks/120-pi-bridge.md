# 120 — Pi RPC bridge

> **Read [000-index](000-index.md) first.** It carries the decisions, write-set
> ownership, dependency graph, and gates this task assumes.

**Goal:** run Pi as a long-lived child process, stream its events into the app, and keep board
mode and power mode as two tool profiles. No chat UI here.

**Depends on:** 005 (findings), 110 (MCP tools). **Parallel with:** 100.
**Owns:** `src/agent/**`, `src/agent` module declaration, bridge tests. Add no deps beyond
`serde_json`/`crossbeam-channel` unless 005 requires them.

**Read first:** `docs/tasks/005-findings.md`; [Pi RPC](https://docs.pi.dev/latest/rpc)
and [RPC commands](https://docs.pi.dev/latest/rpc-commands).

## Steps

1. `AgentHarness` trait: `start(profile)`, `send(prompt)`, `steer(text)`, `abort()`, `stop()`,
   `status()`, `events() -> Receiver<AgentEvent>`; `AgentEvent { Ready, TextDelta(String),
   ToolStart{name}, ToolEnd{name, ok}, TurnEnd{reason}, Cost{..}, Error(String) }`.
2. `PiHarness`: spawn `pi --mode rpc --no-session --provider deepseek --model <id>` with cwd =
   board home, and the profile tool list:
   - **board:** `--tools 'mcp__rustboard__*'` (no shell/file tools).
   - **power:** add `read,bash,powershell,edit,write,grep,find,ls`.
   Use the exact syntax confirmed by 005; keep it in one config struct.
3. JSONL I/O: write one command per line with an `id`; correlate `response` records by id;
   parse `message_update` deltas, tool events, `agent_settled`/`agent_end`, errors; ignore
   unknown record types. Map `abort` to the Stop button. Never parse stderr as protocol.
4. Lifecycle: lazy start on first message, keep alive between turns, restart with backoff (max
   3), clear state on restart, kill the child tree with a Windows **Job Object**
   (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) on exit; keep a stderr ring buffer for error text.
5. MCP wiring: ensure `rustboard-mcp` is registered for the board DB (user-level
   `~/.pi/agent/mcp.json` or `pi mcp add`, per 005). Pass the DB path in the MCP entry's `args`
   (`rustboard-mcp --db <path>`), not through the environment.
6. Tests: parser fixtures for a normal turn, a tool-using turn, an error, and an abort; a fake
   child executable (a test bin or script) for spawn/stdin/stdout/exit so tests never call the
   model; profile tool lists are asserted; orphan cleanup is exercised by dropping the harness.

## Acceptance

- `cargo test`/`clippy`/`fmt` clean; no live API calls in tests.
- Board mode cannot run shell/file tools; power mode adds them.
- Killing the app leaves no `pi` descendants (Job Object).
- A failed spawn or auth error surfaces as `AgentEvent::Error`, never a panic.

## Out of scope

Chat transcript/rendering (130), MCP tool implementations (110), cost UI, session resume beyond
`--no-session` in v1.

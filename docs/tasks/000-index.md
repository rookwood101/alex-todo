# Rustboard v2 — task index

This directory is the implementation plan for turning Rustboard from an infinite canvas with RON
persistence into an AI todo board on SQLite, driven by Pi. Each task file is a **one-page brief**
for one agent: goal, dependencies, files it owns, steps, acceptance, and out-of-scope. Spawn one
agent per file, in the order below, and never let two agents edit the same file set at once.

## Current state

- `main` at `96bc516`; ~1.6k LOC; **28 tests green**.
- Rust + egui/eframe 0.37 on WGPU. `src/app.rs` owns the shell, `src/canvas.rs` the board,
  `src/board.rs`/`src/camera.rs` the models.
- Persistence is `src/persist.rs`: a single RON state file with atomic writes and legacy
  eframe-storage migration. **This is replaced by SQLite and then deleted** (task 170).

## Decisions that are not up for re-litigation

| Area | Decision |
|---|---|
| Agent harness | **Pi 1.6.0**, `@earendil-works/pi-coding-agent`, RPC mode, DeepSeek provider |
| Board tools | Local MCP server (`rustboard-mcp`, Rust/`rmcp`, stdio) |
| Board mode | `pi --tools 'mcp__rustboard__*'` — no OS tools |
| Power mode | adds `read,bash,powershell,edit,write,grep,find,ls` — opt-in, warned |
| Storage | SQLite (`rusqlite` bundled, WAL, migrations, soft delete) |
| History | Append-only batches/events/snapshots; infinite undo by new batches |
| Board | Nested notes that tile recursively; arrows via `edges` |
| Composer | Bottom-centre; **Tab** toggles Capture ↔ Agent |
| Windows | Tray, two-key hotkey, Esc/X hide, real toasts |
| Not doing | Email/ingest, smart scheduling, search palette, v1 migration |

## Global conventions

**Toolchain** (cargo is not on PATH):

```powershell
$env:PATH = "$env:USERPROFILE\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin;$env:PATH"
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Edition 2024, MSRV 1.95, tested on 1.100.0.

**Code rules**

- No unrelated refactors. One task per branch/PR.
- No `unwrap`/`expect` in production paths on user or file data; return typed errors.
- No panic on malformed rows, bad time zones, or missing files.
- Tests must not call a live model, Pi, or the network. Use fixtures and fake child processes.
- Prefer `target/test-tmp/...` scratch directories (the old `persist.rs` tests show the pattern)
  when `%TEMP%` is restricted.
- New dependencies must be added in `Cargo.toml` with the reason in the PR description.

**Data conventions**

- Instants: epoch milliseconds UTC, columns suffixed `_utc_ms`.
- Deadlines/reminders/snooze: `*_local` (`YYYY-MM-DDTHH:MM:SS`) + `*_tz` (IANA) + resolved
  `*_utc_ms` cache. Resolve with `jiff`.
- Soft delete: `deleted_at_utc_ms`; every read filters it out.
- Audit tables (`batches`, `undo_targets`, `events`, `snapshots`, `schema_migrations`) are
  **append-only**: no UPDATE/DELETE, enforced by triggers and by the `board_sql` authorizer.
- Per-row `version` is the optimistic-concurrency token.
- JSON payloads use `serde_json` with stable field names.

**File ownership / write sets**

| Task | Owns |
|---|---|
| 020 | `src/store/mod.rs`, `src/store/paths.rs`, `Cargo.toml` (DB deps) |
| 030 | `migrations/**`, `src/store/schema.rs` |
| 040 | `src/model.rs`, `src/command.rs`, `src/bin/todo.rs` |
| 050 | `src/history.rs` |
| 060 | `src/time.rs` |
| 070 | `src/layout.rs` |
| 080 | `src/capture.rs` |
| 090 | `src/ui/mod.rs`, `src/ui/composer.rs` |
| 100 | `src/canvas.rs`, small `src/ui/**` additions |
| 110 | `src/mcp/**`, `src/bin/rustboard-mcp.rs` |
| 120 | `src/agent/**` |
| 130 | `src/ui/chat.rs` |
| 140 | `src/remind.rs` |
| 150 | `src/notify.rs` |
| 160 | `src/platform/win32.rs` |
| 170 | `src/app.rs`, `src/main.rs`, `src/config.rs`, `src/headless_tests.rs`, `README.md`, `scripts/build.ps1`, deletes `src/persist.rs` |

Tasks 090, 100, 130, 170 all touch `src/app.rs`; they are serialized. If a task needs a file
outside its set, stop and coordinate in this index first. Several tasks add one dependency to
`Cargo.toml`; keep those edits to one line and rebase before merging.

## Dependency graph

```
Lane S   005 ─┐
         010 ─┼─> 020 ─> 030
Lane A              030 ─> 060 ─> 040 ─> 050 ─> 110 ─> 120 ─> 130
Lane B              030 ─> 070 ─┐
                    060 ─> 080 ─┴─> 090 ─> 100
Lane C   010 ─> 150
         010 ─> 160
         050 + 060 ─> 140
All ─> 170
```

**Critical path:** `020 → 030 → 060 → 040 → 050 → 110 → 120 → 130 → 170`.

**Start together:** 005, 010, 020.
**After 030:** 060 and 070; after 060: 040 and 080.
**After 040:** 050; after 050: 110; after 110: 120.
**After 080 + 040:** 090; after 070 + 090: 100.
**After 010:** 150 and 160; 140 after 050 + 060 (and uses its own `ReminderSink`, so it can run
alongside 150).

## Tasks

| # | File | One-line scope | Depends |
|---|---|---|---|
| 0 | [000-index](000-index.md) | This file | — |
| 1 | [005-spike-pi](005-spike-pi.md) | Install/verify Pi RPC, auth, tools, MCP trust | — |
| 2 | [010-spike-win32](010-spike-win32.md) | Prove hotkey, tray, AUMID toast, X/Esc hide | — |
| 3 | [020-sqlite-foundation](020-sqlite-foundation.md) | DB paths, PRAGMAs, migrations runner, backups | — |
| 4 | [030-schema](030-schema.md) | Tables, indexes, triggers, FTS5 | 020 |
| 5 | [040-command-layer](040-command-layer.md) | `Command`, apply/validate, read models, debug CLI | 030, 060 |
| 6 | [050-history-undo](050-history-undo.md) | Append-only batches, undo/redo, snapshots | 040 |
| 7 | [060-civil-time-dst](060-civil-time-dst.md) | `TimeSpec`, `jiff`, gap/fold, "today" | 030 |
| 8 | [070-layout-tree](070-layout-tree.md) | Recursive tiling, groups/Inbox, edge anchors | 030 |
| 9 | [080-capture-parser](080-capture-parser.md) | `#group`, priority, trailing dates, chips | 060 |
| 10 | [090-composer-ui](090-composer-ui.md) | Tab modes, Enter, focus, chips | 040, 080 |
| 11 | [100-canvas-nesting](100-canvas-nesting.md) | Containers, arrows, snooze/done, drag in/out | 070, 090 |
| 12 | [110-mcp-server](110-mcp-server.md) | `rmcp` tools, authorizer, `board_sql` pipeline | 040, 050 |
| 13 | [120-pi-bridge](120-pi-bridge.md) | Pi process, RPC events, abort, modes, Job Object | 005, 110 |
| 14 | [130-chat-ui](130-chat-ui.md) | Transcript, streaming, tool chips, stop, undo-turn | 050, 120, 090 |
| 15 | [140-reminders](140-reminders.md) | Reminder thread, snooze, done, missed summary | 050, 060 |
| 16 | [150-toasts](150-toasts.md) | AUMID shortcut, WinRT toasts, fallback | 010 |
| 17 | [160-tray-hotkey](160-tray-hotkey.md) | Win32 thread, hotkey, tray, X/Esc, single instance | 010 |
| 18 | [170-integration-hardening](170-integration-hardening.md) | Wire app, delete `persist.rs`/RON, gates, release | all |

Tasks 090, 100 and 130 may each add **one** call in `src/app.rs` so they are runnable before
170; 170 consolidates and owns the final `app.rs`. 170 also owns `src/config.rs`.

## Definition of done (every task)

1. `cargo test` green, including new tests for the task's behavior.
2. `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` clean.
3. Errors surfaced, not swallowed; malformed data never panics.
4. The task's acceptance checklist in its file is satisfied.
5. No edits outside the write set; if unavoidable, coordinate and update this index first.
6. Commit as `task NNN: <summary>`.

## Spawning agents

```powershell
git worktree add .agents/worktrees/task-040 -b task/040-command-layer main
```

Then hand the agent that worktree plus the one task file. Tell it to read this index, the task
file, and the files it owns. Agents should read the existing code before editing, run the gates,
and report the exact commands and results. Merge in dependency order; rebase on `main` before
each merge. Remove the stale `.agents/worktrees/dsh-*` checkout once task 170 lands.

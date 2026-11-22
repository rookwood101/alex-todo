# Rustboard v2 — task index

This directory is the implementation plan for turning Rustboard from an infinite canvas with RON
persistence into an AI todo board on SQLite, driven by Pi. Each task file is a **one-page brief**
for one agent: goal, dependencies, files it owns, steps, acceptance, and out-of-scope. Spawn one
agent per file, in the order below, and never let two agents edit the same file set at once.

## Vision

Rustboard is a personal todo system that **feels like a Miro board** but behaves like an
intelligent inbox. The loop is **capture → classify/place → do → reflect**:

- **Capture is instant.** One line in, a sticky note out; no model round-trip on the fast path.
- **The board organizes itself.** Groups, an Inbox, nested containers and arrows carry meaning;
  layout is derived, so an agent can rearrange the board without fighting human drags.
- **The agent lives in the app.** A bottom-centre composer/chat backed by Pi can create, edit,
  link, complete and schedule notes through MCP tools; the UI reloads within ~100 ms.
- **The app is always available.** Global hotkey, tray, real toasts, snooze/due/done, and an
  append-only history that can be undone indefinitely.
- **Data is local-first and transparent.** SQLite in the user's profile, soft deletes,
  append-only events, exportable.
- **Later, not this version:** task detection from email and calendar-aware "good time"
  suggestions. The board works without them.

The canvas is the reflect/plan surface; the composer and reminders are the do surface. This is
not a Miro clone: no connectors, templates or multiplayer.

## The steer

Decisions made during the design review; these override earlier proposals.

- **Harness:** Pi 1.6.0 over Claude Code or DSH — install it now and get embedding right.
- **Storage:** SQLite, not a file. Powerful SQL is a feature; concurrency is SQLite's job.
- **History:** everything soft-deleted, infinite undo, append-only batch/event log.
- **Board shape:** notes tile recursively inside each other; arrows link notes; Inbox is the
  default landing area.
- **Capture:** plain text + Enter creates a note; **Tab** toggles Capture ↔ Agent; no Ctrl+Enter
  and no `?` prefix.
- **Time:** store civil wall time + IANA zone and resolve at runtime; no v1 data migration.
- **Appearance:** due dates and reminders never change opacity; only explicit snooze dims.
- **Windows:** tray icon, Esc/X hide, two-key hotkey, real Windows toasts.
- **Agent scope:** board mode gets board tools only; power mode adds OS tools. `board_sql` is
  allowed by default, but the agent must not touch undo history.
- **Delivery:** one-page Markdown briefs so agents can be spawned per task.

## Paths not taken (briefly)

- **Single JSON/RON file + file watcher.** Replaced by SQLite and `PRAGMA data_version` polling;
  WAL file events are noisy and miss commits.
- **Claude Code or DSH as the embedded runtime.** Claude Code is the strongest agent but its
  print-mode permissions are CLI-shaped; DSH is the best build harness but a heavy
  Node/Electron dependency. Pi's RPC mode is built for custom UIs.
- **A Rust agent framework (Pydantic-AI analogue) or a hand-rolled loop.** The MCP server is the
  typed tool layer; Pi supplies sessions, streaming, abort and compaction.
- **Email ingestion** (Outlook COM/IMAP/Graph/MCP) and **calendar-aware scheduling**. Out of
  scope for this version.
- **Pixel-first Miro model.** Agents would write coordinates; instead layout is derived and
  humans pin only where taste matters.
- **Two inputs (capture bar + AI chat).** One composer with a Tab mode switch.
- **Unrestricted SQLite access.** DML-only authorizer, FK/integrity/invariant checks, automatic
  snapshots; DDL belongs to migrations.
- **A web/Tauri UI.** Kept egui/eframe; the native canvas and hotkey path are already fast.
- **Search/command palette, recurrence, multi-board, sync.** Deferred; FTS5 is scaffolded but no
  UI yet.

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

---

## Implementation notes (maintained by the integrator)

These notes record decisions made while executing the plan. They are binding for later tasks.

### Build environment on this machine

- `cargo` is not on `PATH`; every build command begins with:

  ```powershell
  $env:PATH = "$env:USERPROFILE\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin;$env:PATH"
  ```

  `cargo-clippy` and `cargo-fmt` live in that same folder and are installed.
- **%TEMP% denies file creation here.** Any C compilation (`libsqlite3-sys`) fails without a
  writable temp dir (MSVC `error D8050: cannot execute c1.dll`, or `C1083 ... _CL_...`).
  `.cargo/config.toml` now forces `TMP`/`TEMP` to `target` for cargo (with `force = true`),
  so plain `cargo test` works. If a tool is run outside cargo and hits the same error, set the
  temp variables to a writable folder inside the checkout first:

  ```powershell
  $env:TMP = Join-Path (Get-Location) 'target\tmp'
  $env:TEMP = $env:TMP
  New-Item -ItemType Directory -Force -Path $env:TMP | Out-Null
  ```
- Native-command stderr becomes a PowerShell error; set `$ErrorActionPreference='Continue'` and
  check `$LASTEXITCODE`.

### Crate layout (scaffolded once)

`src/lib.rs` exposes every module publicly and `src/main.rs` is a thin shell that calls
`rustboard::app::RustboardApp`. The three binaries therefore share one command layer:
`todo` and `rustboard-mcp` use `rustboard::command`, `rustboard::model`, etc. Add new
modules to `src/lib.rs`, not `src/main.rs`.

### Module homes (scaffolded once, so tasks never edit `src/main.rs`)

`src/main.rs` declares every v2 module and each has an empty stub file:

`agent/mod.rs`, `capture.rs`, `command.rs`, `config.rs`, `history.rs`, `layout.rs`,
`mcp/mod.rs`, `model.rs`, `notify.rs`, `platform/mod.rs`, `remind.rs`, `store/schema.rs`,
`time.rs`, `ui/mod.rs`.

- Replace the stub with your implementation; **do not edit `src/main.rs`** and do not add
  `mod` lines for existing homes. Add submodules and new bins freely.
- Each declaration carries `#[allow(dead_code)]` until 170 wires the app; leave it alone.
  Inside your own module, do not add `#![allow(dead_code)]`; only test-only helpers may be
  `#[cfg(test)]`.
- `src/bin/todo.rs` (040) and `src/bin/rustboard-mcp.rs` (110) are auto-discovered; no
  `Cargo.toml` change is needed for them.

### Task 020 interface (available now)

- `store::StorePaths`: `resolve(db_override)`, `resolve_from(...)` (test seam), `from_home`,
  `from_db`, `with_migrations`, `ensure_dirs`, `home/db/backups/logs/migrations`.
  A `--db` override pins the whole board next to that file.
- `store::Store`: `open(path)`, `open_read_only(path)`, `open_board(&paths, &migrations)`,
  `migrate(&migrations, backups_dir)`, `data_version()`, `check_integrity()`,
  `connection()/connection_mut()`, `path()`.
- `store::Migrations`: `from_dir`, `from_sql(&[(file_name, sql)])` for `include_str!` sets,
  `Migration::checksum()`, `MigrationReport { applied, already_applied, backup }`.
- `store::now_utc_ms()` (epoch milliseconds) and `store::StoreError` (thiserror).
- 030 adds to `src/store/schema.rs`:
  `pub fn migrations() -> Migrations` (embedded `include_str!` pairs) and
  `pub fn open_board(paths: &StorePaths) -> Result<Store, StoreError>` wrapping
  `Store::open_board`. Callers (040/110/140/170) use that helper rather than reading files.
- Migration runner behaviour: creates `schema_migrations` itself; verifies checksums of applied
  rows; refuses a DB newer than the build; backs up an existing DB before applying new versions
  (fresh DBs are not backed up); applies each migration in one `BEGIN IMMEDIATE` transaction.
  `PRAGMA foreign_keys` cannot change inside that transaction — use `defer_foreign_keys` if a
  migration needs FK-off semantics.

### Progress (updated by the integrator)

Completed and merged on `main`: **005, 010, 020, 030, 040, 060, 070, 080, 140, 150, 160**.
In flight when this note was written: 050 (history), 090 (composer), 120 (Pi bridge).
Part 1 of 110 (the guarded `board_sql` execution pipeline in `src/mcp/sql.rs`) is merged; the
rmcp tool surface and the `rustboard-mcp` binary remain.

Build note for parallel agents: the shared `target/shared` directory can hand cargo another
worktree's `rustboard` test binary. Before final gates, set
`CARGO_TARGET_DIR=<repo>/target/<task-name>` and run `cargo clean -p rustboard`; verify the
compile line names your worktree. `main` itself builds in the normal `target/` directory.

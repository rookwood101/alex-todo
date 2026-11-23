# Rustboard

**Rustboard** is a personal todo board that feels like a Miro canvas but behaves like an
inbox. Notes tile recursively, a bottom-centre composer captures one line at a time, and an
embedded **Pi** agent can create, edit, link and schedule notes through an MCP tool server.
Everything is local-first in one SQLite database with append-only history and infinite undo.

Built with Rust, [egui/eframe](https://github.com/emilk/egui) on WGPU, <code>rusqlite</code>,
<code>jiff</code>, and [Pi](https://docs.pi.dev/) 1.6.0 over its RPC mode.

![Rustboard running on Windows](docs/screenshot.png)

## What works

- Infinite, zoomable canvas. Any note with children is a container; root containers sit in a
  row, and the **Inbox** container catches notes captured without a <code>#container</code>.
- Nested notes tile inside containers; arrows link notes; root notes can be pinned by dragging.
- Important notes show a red <code>!</code> and sort first in their container.
- One composer with two modes: **Capture** (Enter creates a note instantly, no model call) and
  **Agent** (Enter sends a prompt to Pi). **Tab** switches modes.
- Capture chips: <code>#container</code> (created if missing), <code>!</code> for important, and
  trailing deadlines such as <code>tomorrow 4pm</code>.
- SQLite storage (WAL) with versioned migrations, pre-migration backups and FTS5 search.
- Every mutation is one batch with before/after events; undo and redo are new batches, so history
  is append-only and never rewritten.
- Chat panel above the composer: streaming text, tool chips, stop, cost and **Undo this turn**.
- Reminders run on their own thread (snooze, done, missed-summary), with real Windows toasts.
- Tray icon, two-key global hotkey (<code>Ctrl+Space</code>), X/Esc hide-to-tray, single instance.
- CLI (<code>todo</code>) and MCP stdio server (<code>rustboard-mcp</code>) share the same command
  layer as the app.

Later, not this version: email ingestion, calendar-aware scheduling, a search palette,
multi-board and sync.

## Requirements

- Windows 10/11, Rust stable 1.95+ (tested on 1.100), MSVC C++ build tools.
- For the agent: Node 22.19+ and Pi 1.6.0:

      npm i -g @earendil-works/pi-coding-agent@1.6.0
      pi auth login deepseek        # or set DEEPSEEK_API_KEY
      pi --list-models deepseek

## Run

    cargo run --release --bin rustboard

If <code>cargo</code> is not on PATH:

    $rustBin = "$env:USERPROFILE\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin"
    $env:PATH = "$rustBin;$env:PATH"
    & "$rustBin\cargo.exe" run --release --bin rustboard

The first launch creates <code>%LOCALAPPDATA%\Rustboard\</code> with
<code>board.sqlite3</code>, <code>backups/</code>, <code>logs/</code> and
<code>config.json</code> (created on first save). <code>RUSTBOARD_HOME</code> relocates the whole
folder; the CLI and MCP server also accept <code>--db &lt;path&gt;</code>. To build and code-sign
all three binaries:

    .\scripts\build.ps1              # or -CreateCertificate once

## Controls

| Input | Action |
|---|---|
| Scroll wheel | Zoom under the pointer |
| Right-drag, middle-drag or Space+drag | Pan the canvas |
| Click a note or arrow | Select it |
| Click a selected note, double-click or Enter | Edit the note inline |
| Shift+click | Add to or remove from the selection |
| Drag on empty canvas | Select the notes inside the box |
| Ctrl+A | Select all notes |
| Click a note's checkbox / Ctrl+Enter | Toggle done (Ctrl+Enter acts on the selection) |
| Hide done (toolbar) | Hide done notes that have no open children |
| Delete or Backspace | Delete the selection (containers take their children); Undo is offered |
| Drag a note (or the selection) | Drop on a note's left/right edge to reorder next to it, on its middle to nest inside it, on empty canvas to pin it there |
| Ctrl+G / Ctrl+Shift+G | Group the selection into a new container / ungroup a container |
| Click the "!" slot / Ctrl+I | Toggle important (shown as a red "!") |
| Drag the dot on a selected note | Link it to another note |
| Right-click a note | Edit, done/reopen, important, snooze, unpin, group/ungroup, delete (acts on the whole selection) |
| Tab (composer) | Switch Capture / Agent |
| Enter (composer) | Create the note / send the prompt |
| Shift+Enter | New line in the composer |
| Esc | Save and leave the editor, then clear the selection, then hide to the tray |
| Ctrl+Space | Show and focus the window (configurable in Settings) |
| Ctrl+N | New note in the Inbox |
| Ctrl+Z / Ctrl+Shift+Z | Undo / redo |

Capture examples: <code>buy milk tomorrow 4pm #home !</code>,
<code>pay rent by friday</code>, <code>call #work</code>,
<code>ship 2026-12-01 09:30</code>. A date with no time means 09:00 local; a bare time already
past rolls to tomorrow. Ambiguous text is left alone ("the 4pm thing" stays text).

## Agent modes

The chat panel talks to Pi over JSONL RPC. Two tool profiles:

- **Board mode** (default): <code>mcp__rustboard__*</code> only - no shell, no file tools. The MCP
  server exposes <code>board_schema</code>, <code>board_query</code>, <code>board_apply</code>,
  <code>board_sql</code> and <code>board_history</code>.
- **Power mode** (opt-in, shown with a warning): adds
  <code>read,bash,powershell,edit,write,grep,find,ls</code>.

<code>board_sql</code> is DML-only: an authorizer denies DDL, pragmas, attach/detach, transaction
control and writes to the five audit tables; every statement runs inside a transaction with
foreign-key, integrity, cycle, edge-endpoint and zone checks, and records a batch, an event and a
before-image snapshot so the turn can be undone. Arguments:
<code>rustboard-mcp --db &lt;path&gt;</code>; Pi's project config
(<code>&lt;board home&gt;/.pi/mcp.json</code>) is written with <code>"exposure": "direct"</code>
and <code>--approve</code> is used.

## Storage

- Database: <code>%LOCALAPPDATA%\Rustboard\board.sqlite3</code> (WAL). Tables: <code>notes</code>
  and <code>edges</code> (the legacy <code>groups</code> table is unused since migration 0003), plus the append-only <code>batches</code>,
  <code>undo_targets</code>, <code>events</code>, <code>snapshots</code> and
  <code>schema_migrations</code>.
- Soft deletes (<code>deleted_at_utc_ms</code>); every read filters them out.
- Deadlines store civil wall time + IANA zone and a resolved UTC cache; DST gaps resolve to the
  first valid instant, folds to the earlier one.
- Migrations are embedded in the binary, checksummed, and backed up to <code>backups/</code>
  before an upgrade. A database newer than the app is refused rather than opened.
- <code>config.json</code>: <code>hotkey</code>, <code>board_db</code>, <code>tz</code>,
  <code>pi_model</code>, <code>pi_provider</code>, <code>pi_profile</code>,
  <code>reminder_minutes</code>. Bad values fall back per field with a warning.

## CLI and MCP

    cargo run --bin todo -- add "buy milk tomorrow 4pm #home"
    cargo run --bin todo -- list --json
    cargo run --bin todo -- done <id>
    cargo run --bin todo -- delete <id> && cargo run --bin todo -- restore <id>
    cargo run --bin todo -- apply --json < commands.json
    cargo run --bin todo -- query --read-only "SELECT id, text FROM notes"

Exit codes: 0 success, 2 usage/rejected input, 3 not found, 5 database error. Pass
<code>--db &lt;path&gt;</code> or <code>RUSTBOARD_HOME</code> to work on a specific board.

## Tests

    cargo test
    cargo clippy --all-targets -- -D warnings
    cargo fmt --check

The suite has 263 library tests plus 9 CLI tests. It covers migrations and schema invariants,
command validation and rollback, undo/redo and snapshot conflicts, DST transitions, the capture
parser, layout geometry, <code>board_sql</code> authorization, Pi protocol parsing with fake child
processes, the reminder engine with an injected clock, toast fallbacks, the tray/hotkey state
machine, the composer and chat panels headlessly, and app-level capture, external-change reloads
and undo.

## Project layout

| Path | Contents |
|---|---|
| <code>src/app.rs</code> | App shell: store, polling, toolbar, chat/composer wiring, tray, reminders |
| <code>src/ui/board.rs</code> | Canvas for the SQLite model: layout, painting, drag/link/snooze |
| <code>src/ui/composer.rs</code> | Capture/Agent composer with chips |
| <code>src/ui/chat.rs</code> | Chat transcript, streaming, tool chips, undo-turn intents |
| <code>src/command.rs</code> | <code>Command</code>, <code>Store::apply</code>, validation, batches |
| <code>src/history.rs</code> | Undo/redo, history reads, snapshots and restore |
| <code>src/model.rs</code> | Read models shared by the UI, CLI and MCP |
| <code>src/store/</code> | Pragmas, paths, migration runner, schema and FTS helpers |
| <code>migrations/</code> | <code>0001_init.sql</code>, <code>0002_fts.sql</code> |
| <code>src/time.rs</code> | Civil time, DST resolution, <code>today_bounds</code> |
| <code>src/layout.rs</code> | Recursive tiling, containers, edge geometry |
| <code>src/capture.rs</code> | Capture-line parser |
| <code>src/agent/</code> | Pi RPC bridge, profiles, Job Object cleanup, MCP config |
| <code>src/mcp/</code> | MCP tools, server and the <code>board_sql</code> authorizer |
| <code>src/remind.rs</code>, <code>src/notify.rs</code>, <code>src/platform/</code> | Reminders, toasts, tray/hotkey |
| <code>src/bin/todo.rs</code>, <code>src/bin/rustboard-mcp.rs</code> | CLI and MCP binaries |
| <code>docs/tasks/</code> | The v2 task briefs and implementation notes |
| <code>scripts/build.ps1</code> | Release build and signing for all three binaries |

## License

MIT - see [LICENSE](LICENSE).

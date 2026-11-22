# 040 — Command layer and debug CLI

> **Read [000-index](000-index.md) first.** It carries the decisions, write-set
> ownership, dependency graph, and gates this task assumes.

**Goal:** one typed mutation path shared by the UI, the CLI, and (later) MCP. Every command
validates, executes inside one transaction, and leaves a batch + events for 050. This is the
spine; no UI.

**Depends on:** 030, 060 (`TimeSpec`). **Parallel with:** 050 planning, 070.
**Owns:** `src/model.rs`, `src/command.rs`, `src/bin/todo.rs`.

**Read first:** `030`'s schema; `src/board.rs` (the old model, for reference only).

## Steps

1. `model.rs`: read models `Board { notes, groups, edges }`, `Note`, `Group`, `Edge`, loaded
   with active-only queries (`deleted_at_utc_ms IS NULL`). Include `TimeSpec` from `src/time.rs`
   (task 060) for due/remind/snooze. No UI types.
2. `command.rs`: `Command` (serde-tagged):
   `add_note{text,group,parent,due,remind,priority,source}`,
   `set_text`, `set_group`, `set_parent`, `set_priority`, `set_due`, `set_remind`,
   `snooze{until}`, `unsnooze`, `complete`, `reopen`, `delete`, `restore`,
   `pin{x,y}`, `unpin`, `create_group{name,color}`, `rename_group`,
   `link{from,to,kind}`, `unlink{edge_id}`.
3. `Store::apply(commands, actor, label) -> Result<Applied>`: `BEGIN IMMEDIATE`; insert a
   `batches` row (`kind='apply'`); for each command read the before rows, validate, execute,
   read the after rows, and append an `events` row (`entity`, `entity_id`, `before_json`,
   `after_json`); commit. Return `batch_id`, `Vec<Effect>`, and new ids. Any failure rolls back
   and reports the failing command index.
4. Validation: group/parent exist and are active; parent cycle via recursive CTE; edges reject
   self/duplicate/deleted endpoints; `delete` soft-deletes the subtree and touching edges;
   `restore` restores the subtree and the edges removed with it; `complete` sets
   `completed_at_utc_ms`, `reopen` clears it and clears `reminded_at_utc_ms` so the reminder
   re-arms; `pin` only for root notes.
5. `src/bin/todo.rs`: `add`, `list --json`, `show`, `set`, `done`, `reopen`, `delete`,
   `restore`, `snooze`, `move`, `link`, `apply --json`, `query --read-only`, `path`.
   `--db` / `RUSTBOARD_HOME`. Exit codes 0/2/3/5. Human output by default, JSON with `--json`.
6. Tests: every command round-trips and writes one event; cycle rejected; delete subtree and
   restore; rollback leaves no batch/event; CLI `--json` shape; read-only query cannot write.

## Acceptance

- `cargo test`/`clippy`/`fmt` clean.
- `todo add "x" && todo list --json` shows the note; `todo delete` hides it and `restore`
  returns it.
- No UI files touched; the old app still compiles using `board.rs`.

## Out of scope

Undo algorithms (050), MCP (110), rendering (100). Keep `Command`/`Effect` serializable; that is
the agent contract.

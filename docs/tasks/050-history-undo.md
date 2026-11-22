# 050 — History, soft delete and infinite undo

**Goal:** make every mutation reversible from append-only audit data, without ever updating or
deleting a history row. This is the foundation for Ctrl+Z, "undo that agent turn", and
`board_sql` snapshots.

**Depends on:** 040. **Parallel with:** 070, 110 planning.
**Owns:** `src/history.rs`, history tests. Do not add the authorizer here (110).

**Read first:** this index; `040`'s `apply`; `030`'s `batches`/`undo_targets`/`events`/`snapshots`.

## Steps

1. Model undo as new batches: `batches.kind IN ('apply','undo','redo')`, `undo_targets` links a
   reversing batch to one or more target batches. Never `UPDATE batches`; derive effective
   state by walking batch ids and targets.
2. `undo_last()`: pick the newest applied `apply` batch whose actor is **not** `system` (reminder
   markers are not user undo); load its events in reverse; apply the inverse SQL from
   `before_json` inside one `BEGIN IMMEDIATE`; verify each row's `version` still equals the
   event's `after_json.version` (else return `Conflict`); insert a new `kind='undo'` batch +
   `undo_targets`; commit. `redo_last()` re-applies the original `after_json` as a
   `kind='redo'` batch.
3. `undo_batches(&[BatchId])` reverses a set (a Pi turn range) in reverse batch order, skipping
   system batches. This is what the chat UI calls.
4. `history(note_id)` returns events ordered by id; `batches_between(a,b)` for the UI.
5. Snapshot API for `board_sql`: `snapshot_before()` dumps `notes/groups/edges` to JSON,
   `snapshot_after()` does it again; `restore_snapshot()` replaces those tables in a transaction
   with a revision check, in FK-safe order (edges, notes, groups; `defer_foreign_keys` if
   needed). DML-only SQL is still undoable through this.
6. Tests: undo add/update/delete-subtree/complete/snooze/reparent; redo; multiple undos;
   conflict when a later batch touched the row; turn-range undo; snapshot restore; audit tables
   unchanged (counts only grow) throughout.

## Acceptance

- `cargo test`/`clippy`/`fmt` clean.
- After any sequence of commands, N undos followed by N redos restores the exact user-visible
  fields (ignoring `version`/`updated_at_utc_ms`, which the triggers own).
- A conflict is reported, never silently clobbered.
- No SQL in this module updates or deletes `batches`, `undo_targets`, `events`, or `snapshots`.

## Out of scope

UI, MCP tool wiring, authorizer, retention/`VACUUM`, garbage collection of snapshots. If an
`events.entity/entity_id` column is missing, add migration `0003` in this task rather than
editing `0001`.

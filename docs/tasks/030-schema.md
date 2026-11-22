# 030 — Schema and migrations

> **Read [000-index](000-index.md) first.** It carries the decisions, write-set
> ownership, dependency graph, and gates this task assumes.

**Goal:** create the real schema: board tables, append-only audit tables, indexes, FTS5, and the
triggers that enforce the invariants.

**Depends on:** 020. **Parallel with:** 040 planning, 060, 070.
**Owns:** `migrations/**`, `src/store/schema.rs`. Do not touch app/UI files.

**Read first:** `020`'s migration runner, the approved schema in
the plan. Use `rusqlite` prepared statements and `PRAGMA foreign_keys=ON`.

## Steps

1. `migrations/0001_init.sql`:
   - `groups`, `notes`, `edges` exactly as in the plan (civil `*_local` + `*_tz` + cached
     `*_utc_ms`, `deleted_at_utc_ms`, `version`, `layout_mode`, `pos_x/pos_y`, `sort_order`).
   - `batches(id TEXT PK, created_at_utc_ms, actor, actor_ref, label, kind CHECK(...))`,
     `undo_targets(undo_batch_id, target_batch_id, PK(...))`,
     `events(id, batch_id, seq, at_utc_ms, actor, command, entity, entity_id, payload_json,
     before_json, after_json, UNIQUE(batch_id, seq))`,
     `snapshots(id, batch_id, at_utc_ms, data, reason)`.
   - Indexes on active notes, `(deleted_at_utc_ms, parent_id)`, `due_utc_ms`, edges
     `from/to`, `events(batch_id)`, `events(entity, entity_id)`. `CHECK` on status/priority/layout, `CHECK(from <> to)` on
     edges, `UNIQUE(from, to, kind)`.
   - `BEFORE UPDATE` triggers on `notes/groups/edges` that set `updated_at_utc_ms` and
     `version = OLD.version + 1` (do not enable `recursive_triggers`).
   - `BEFORE UPDATE`/`BEFORE DELETE` triggers on `batches`, `undo_targets`, `events`,
     `snapshots`, `schema_migrations` that `RAISE(ABORT, '<table> is append-only')`.
2. `migrations/0002_fts.sql`: FTS5 virtual table over `notes.text` (external content or
   contentless, with triggers on insert/update/delete), plus `notes_fts` query helpers.
   `rusqlite`'s `bundled-full` compiles FTS5; verify it, and if the virtual table cannot be
   created, stop and report the build flag instead of shipping a fallback.
3. Tests: apply from empty; all CHECKs reject bad rows; `updated_at`/`version` bump; every
   append-only trigger aborts UPDATE and DELETE; FK violations rejected; FTS finds text and
   updates on soft delete; `PRAGMA foreign_key_check` returns no rows; `PRAGMA integrity_check`
   returns `ok` after a normal workload.

## Acceptance

- `cargo test` green; schema tests run against a real file DB (not `:memory:` for migration
  tests), using `target/test-tmp`.
- Append-only protection is proven for all five audit tables.
- No strings, no seed board; 170 owns onboarding.

## Out of scope

Commands, history algorithms, authorizer setup, UI. If the schema needs a change later, add
`0003_*.sql`; never edit an applied migration.

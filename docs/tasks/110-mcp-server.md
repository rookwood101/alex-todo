# 110 — MCP server and safe `board_sql`

**Goal:** expose the board to Pi over stdio MCP and make `board_sql` powerful but unable to
touch history. This is the security boundary; take it seriously.

**Depends on:** 040 (commands), 050 (history/snapshots). **Parallel with:** 100, 140, 150, 160.
**Owns:** `src/mcp/**`, `src/bin/rustboard-mcp.rs`, MCP tests. Add `rmcp` to `Cargo.toml` in this
PR; coordinate the one-line merge conflict with 120.

**Read first:** this index; `src/command.rs`; `src/history.rs`; the
[rusqlite authorizer](https://docs.rs/rusqlite/latest/rusqlite/struct.Connection.html#method.authorizer)
and [`AuthAction`](https://docs.rs/rusqlite/latest/rusqlite/hooks/enum.AuthAction.html) docs.

## Steps

1. `rustboard-mcp --db <path>`: stdio MCP server (stdout is protocol only; logs to stderr).
   Tools: `board_schema`, `board_query`, `board_apply`, `board_sql`, `board_history`.
2. `board_query`: separate read-only connection (`SQLITE_OPEN_READ_ONLY` + `query_only`), one
   statement, max 500 rows / 64 KiB, returns columns/rows/truncated. No writes possible.
3. `board_apply { commands, description? }`: `Store::apply` with `actor='agent'` and a
   `session`/turn reference, returns batch id, effects, ids. `board_sql` uses the same actor, so a
   turn's batches are findable by 130.
4. `board_sql { sql, description }`: dedicated connection. The authorizer is installed **only
   while the user's SQL is prepared and executed**; the tool's own transaction and check
   pragmas run without it, and it is reinstalled before returning to the loop.
   - Begin with `BEGIN IMMEDIATE` (no authorizer), then install it.
   - Deny: all DDL variants, `Attach`, `Detach`, `Pragma`, `Transaction`, `Savepoint`, `Reindex`,
     `Analyze`; `Insert`/`Update`/`Delete` on `batches`, `undo_targets`, `events`, `snapshots`,
     `schema_migrations`; dangerous functions (`load_extension`, `writefile`, `readfile`,
     `fts3_tokenizer`, `zipfile`); `Unknown`. Allow `Select`, `Read`, `Recursive`, and DML on
     board tables. A user `BEGIN`/`COMMIT` is denied; the tool owns the transaction.
   - A leading-keyword guard rejects `VACUUM`/`BEGIN`/`COMMIT`/`ATTACH`/`PRAGMA` cases the
     authorizer may not see.
   - After the user's SQL completes, clear the authorizer, then run the checks and the audit
     write (no user statement remains prepared or running):
     `PRAGMA foreign_key_check` → `PRAGMA integrity_check` (or `quick_check` above 50 MB) →
     invariant queries (parent cycles, active edge endpoints, valid zones) → insert
     batch/events/snapshot with the before/after images → commit → reinstall the authorizer.
     Any failure rolls back and returns the failing check and SQL error.
5. `board_schema` lists tables/columns and marks the five audit tables `managed: read-only`.
   `board_history` reads events/batches; reading is explicitly allowed.
6. Tests: `DELETE FROM events`, `UPDATE batches`, `INSERT INTO events`, `DROP TABLE events`,
   `PRAGMA foreign_keys=OFF`, `ATTACH`, `BEGIN`, `VACUUM` all fail with `SQLITE_AUTH` and leave
   the DB unchanged; `UPDATE notes SET text=...` succeeds and undo restores it; a cycle-creating
   update rolls back; query is read-only and capped; tool names/schemas match the contract.

## Acceptance

- `cargo test`/`clippy`/`fmt` clean.
- The audit tables cannot be modified through the tool, by any SQL form the tests exercise.
- No user SQL can commit without the checks passing and the batch being recorded.

## Out of scope

Spawning Pi (120), chat UI (130), retention. DDL through SQL is deliberately rejected; schema
changes belong in migrations.

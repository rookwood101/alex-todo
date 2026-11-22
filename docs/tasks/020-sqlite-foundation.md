# 020 — SQLite foundation

> **Read [000-index](000-index.md) first.** It carries the decisions, write-set
> ownership, dependency graph, and gates this task assumes.

**Goal:** open a WAL SQLite database at a stable path, apply versioned SQL migrations with
checksums and backups, and expose the connection to the rest of the app. No tables or commands
yet; those are 030/040.

**Depends on:** nothing. **Parallel with:** 005, 010.
**Owns:** `src/store/**`, `Cargo.toml` (database dependencies only), `src/store` module
declaration in `src/main.rs`. Do **not** touch `src/app.rs` or delete `src/persist.rs` yet.

**Read first:** `src/persist.rs` (reuse its atomic-write mindset and scratch-dir test
helper); `src/main.rs`.

## Steps

1. Add dependencies: `rusqlite = { version = "0.43", features = ["bundled-full"] }`
   (this includes `hooks`, `backup`, `modern_sqlite`), plus direct `serde_json`, `uuid` (v4),
   `thiserror`, `jiff`, `sha2`. Keep `ron` until 170.
2. `StorePaths`: default `%LOCALAPPDATA%\Rustboard\` with `board.sqlite3`, `backups/`, `logs/`;
   `RUSTBOARD_HOME` override; `--db` override plumbed for binaries. Create directories on open.
3. `Store::open(path)`: open read-write-create, set `journal_mode=WAL`, `synchronous=NORMAL`,
   `foreign_keys=ON`, `busy_timeout=5000`, `temp_store=MEMORY`. Add `Store::open_read_only()`
   using `SQLITE_OPEN_READ_ONLY` + `PRAGMA query_only=ON`.
4. Migration runner: read `migrations/NNNN_*.sql` in order (embedded or from disk), keep
   `schema_migrations(version, applied_at_utc_ms, checksum)`, apply each in a transaction,
   verify SHA-256 checksums, refuse a DB whose max version is higher than the app knows.
   Before applying, copy the DB with the `backup` API to `backups/pre-migration-<ts>.sqlite3`.
5. Add `Store::data_version() -> i64` (`PRAGMA data_version`) and a `Store::check_integrity()`
   helper (`PRAGMA integrity_check`). These are used by 170 and 110.
6. Tests: fresh DB applies migrations; second open is a no-op; checksum mismatch fails; newer
   DB is refused; backup file exists; `data_version` changes after a second connection commits;
   read-only connection cannot write; paths honor `RUSTBOARD_HOME`. Test the runner against a
   **temporary migrations directory**, not the real `migrations/` (030 owns that).

## Acceptance

- `cargo test` green with the tests above.
- `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` clean.
- No app behavior changes yet; `src/persist.rs` still works.
- `migrations/` is left empty for 030; the runner is proven with fixture migrations.

## Out of scope

Schema design (030), commands (040), history (050), authorizer (110), UI wiring (170). Do not
change the hot path in `app.rs`; that is the integration task.

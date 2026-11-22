//! Board schema and embedded migrations (task 030).
//!
//! The SQL lives in `migrations/` and is embedded with [`include_str!`], so a
//! built binary never needs the source tree at runtime. [`migrations`] is the
//! single source of truth for every caller that opens the board.
//!
//! The schema itself documents its choices in SQL comments. In short:
//! board tables are `STRICT`, soft deletes live in `deleted_at_utc_ms`,
//! deadlines keep a civil triple plus a UTC cache, and the five audit tables
//! reject UPDATE and DELETE at the trigger level.

use rusqlite::{Connection, params};

use super::{Migrations, Store, StoreError, StorePaths};

/// The append-only audit tables, in the order the plan lists them.
///
/// `board_sql` (task 110) installs a DML authorizer that denies writes to
/// every name in this list; keeping the list here means the schema and the
/// authorizer cannot drift apart.
pub const AUDIT_TABLES: &[&str] = &[
    "batches",
    "undo_targets",
    "events",
    "snapshots",
    "schema_migrations",
];

/// SQL that searches live notes. Bind `?1` = FTS5 query and `?2` = limit.
///
/// `notes_fts` is contentless, so the note text is read back from `notes`
/// joined on the implicit rowid. The `deleted_at_utc_ms` filter is deliberate
/// defence in depth: the triggers already remove soft-deleted rows from the
/// index, but a query helper should not depend on maintenance tooling never
/// disabling them.
pub const NOTE_SEARCH_SQL: &str = "SELECT n.id, n.text
  FROM notes_fts
  JOIN notes AS n ON n.rowid = notes_fts.rowid
 WHERE notes_fts MATCH ?1
   AND n.deleted_at_utc_ms IS NULL
 ORDER BY n.id
 LIMIT ?2";

/// One logical table and its column names, for `board_schema` (task 110).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableColumns {
    pub table: String,
    pub columns: Vec<String>,
}

/// The embedded migration set: SQL files next to this crate's `migrations/`.
///
/// The names and versions are compile-time constants, so a malformed pair is a
/// programming error rather than bad user input. Never edit a migration that
/// has shipped; add the next numbered file (and pair it here) instead.
pub fn migrations() -> Migrations {
    Migrations::from_sql(&[
        (
            "0001_init.sql",
            include_str!("../../migrations/0001_init.sql"),
        ),
        (
            "0002_fts.sql",
            include_str!("../../migrations/0002_fts.sql"),
        ),
    ])
    .expect("embedded migrations have valid names and unique versions")
}

/// Open (and migrate) the board at `paths`.
///
/// This is the convenience entry point later tasks use; callers that need the
/// [`super::MigrationReport`] can call [`Store::open_board`] directly.
pub fn open_board(paths: &StorePaths) -> Result<Store, StoreError> {
    let (store, _report) = Store::open_board(paths, &migrations())?;
    Ok(store)
}

/// List the logical tables and their columns.
///
/// SQLite's internal tables (`sqlite_*`) and FTS5's shadow tables
/// (`notes_fts_data`, `notes_fts_idx`, ...) are implementation details and are
/// omitted; the `notes_fts` virtual table itself is listed.
pub fn table_columns(conn: &Connection) -> Result<Vec<TableColumns>, StoreError> {
    let mut tables = conn.prepare(
        "SELECT name
           FROM sqlite_master
          WHERE type = 'table'
            AND name NOT LIKE 'sqlite_%'
            AND name NOT LIKE 'notes_fts_%'
          ORDER BY name",
    )?;
    let table_names = tables.query_map([], |row| row.get::<_, String>(0))?;

    let mut column_statement =
        conn.prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")?;
    let mut result = Vec::new();
    for table in table_names {
        let table = table?;
        let mut columns = Vec::new();
        for column in column_statement.query_map([table.as_str()], |row| row.get::<_, String>(0))? {
            columns.push(column?);
        }
        result.push(TableColumns { table, columns });
    }
    Ok(result)
}

/// Search live notes with an FTS5 query.
///
/// Returns `(note id, note text)` ordered deterministically by id; ranking is
/// left to the caller (search UI is deferred). An empty query returns an empty
/// result instead of an FTS syntax error, and the limit is clamped to
/// `1..=500` to keep agent-supplied limits bounded.
pub fn search_notes(
    conn: &Connection,
    query: &str,
    limit: i64,
) -> Result<Vec<(String, String)>, StoreError> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let limit = limit.clamp(1, 500);
    let mut statement = conn.prepare(NOTE_SEARCH_SQL)?;
    let rows = statement.query_map(params![query, limit], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut found = Vec::new();
    for row in rows {
        found.push(row?);
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rusqlite::{ErrorCode, params};

    use super::*;
    use crate::store::{Migrations, Store, StorePaths, scratch_dir};

    /// A fixed creation instant so trigger bumps are unambiguous.
    const T0: i64 = 1_700_000_000_000;

    fn board(name: &str) -> Store {
        let dir = scratch_dir(name);
        let paths = StorePaths::from_home(&dir);
        open_board(&paths).expect("board opens and applies every migration")
    }

    fn add_group(conn: &Connection) {
        conn.execute(
            "INSERT INTO groups
                 (id, name, color, sort_order, created_at_utc_ms, updated_at_utc_ms)
             VALUES ('g1', 'Work', 0, 0, ?1, ?1)",
            [T0],
        )
        .expect("valid group inserts");
    }

    fn add_note(conn: &Connection, id: &str, text: &str) {
        conn.execute(
            "INSERT INTO notes (id, text, created_at_utc_ms, updated_at_utc_ms)
             VALUES (?1, ?2, ?3, ?3)",
            params![id, text, T0],
        )
        .expect("valid note inserts");
    }

    fn add_edge(conn: &Connection, id: &str, from: &str, to: &str) {
        conn.execute(
            "INSERT INTO edges (id, \"from\", \"to\", created_at_utc_ms, updated_at_utc_ms)
             VALUES (?1, ?2, ?3, ?4, ?4)",
            params![id, from, to, T0],
        )
        .expect("valid edge inserts");
    }

    fn add_batch(conn: &Connection, id: &str, kind: &str) {
        conn.execute(
            "INSERT INTO batches (id, created_at_utc_ms, actor, kind)
             VALUES (?1, ?2, 'human', ?3)",
            params![id, T0, kind],
        )
        .expect("valid batch inserts");
    }

    fn assert_constraint(error: rusqlite::Error, label: &str) {
        let is_constraint = matches!(
            &error,
            rusqlite::Error::SqliteFailure(failure, _)
                if failure.code == ErrorCode::ConstraintViolation
        );
        assert!(
            is_constraint,
            "{label}: expected a constraint error, got {error:?}"
        );
    }

    fn assert_append_only(error: rusqlite::Error, table: &str) {
        let message = error.to_string();
        assert!(message.contains("append-only"), "{table}: {message}");
        assert!(message.contains(table), "{table} named in error: {message}");
    }

    fn row_count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count rows")
    }

    fn fts_count(conn: &Connection, term: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM notes_fts WHERE notes_fts MATCH ?1",
            [term],
            |row| row.get(0),
        )
        .expect("count FTS matches")
    }

    fn assert_touch(conn: &Connection, table: &str, id: &str, update_sql: &str) {
        let before: (i64, i64) = conn
            .query_row(
                &format!("SELECT version, updated_at_utc_ms FROM {table} WHERE id = ?1"),
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read version before update");
        conn.execute(update_sql, []).expect("update applies");
        let after: (i64, i64) = conn
            .query_row(
                &format!("SELECT version, updated_at_utc_ms FROM {table} WHERE id = ?1"),
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read version after update");
        assert_eq!(after.0, before.0 + 1, "{table} version must bump by one");
        assert!(
            after.1 > before.1,
            "{table} updated_at_utc_ms must move forward"
        );
    }

    #[test]
    fn embedded_migrations_apply_to_an_empty_file_database() {
        let store = board("schema-apply");
        assert!(store.path().is_file(), "tests use a real file database");

        let conn = store.connection();
        let versions: Vec<i64> = conn
            .prepare("SELECT version FROM schema_migrations ORDER BY version")
            .expect("prepare history query")
            .query_map([], |row| row.get(0))
            .expect("query history")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("read history");
        assert_eq!(versions, vec![1, 2]);

        // The FTS table was created by 0002 (FTS5 is compiled in bundled-full).
        let fts_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'notes_fts'",
                [],
                |row| row.get(0),
            )
            .expect("notes_fts exists");
        assert!(fts_sql.contains("fts5"), "notes_fts is an FTS5 table");
    }

    #[test]
    fn migrations_folder_matches_the_embedded_set() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations");
        let on_disk = Migrations::from_dir(&dir).expect("migrations folder reads");
        let embedded = migrations();

        assert_eq!(embedded.latest_version(), 2);
        assert_eq!(on_disk.len(), embedded.len());
        for (disk, built) in on_disk.migrations().iter().zip(embedded.migrations()) {
            assert_eq!(disk.version, built.version);
            assert_eq!(disk.name, built.name);
            assert_eq!(disk.sql, built.sql, "{} is embedded verbatim", disk.name);
        }

        // Every file in migrations/ must be one of the embedded SQL files: no
        // half-written drafts, no notes, no stray fixtures.
        let mut expected: Vec<String> = embedded
            .migrations()
            .iter()
            .map(|migration| format!("{:04}_{}.sql", migration.version, migration.name))
            .collect();
        expected.sort();
        let mut actual: Vec<String> = std::fs::read_dir(&dir)
            .expect("read migrations folder")
            .map(|entry| {
                entry
                    .expect("read directory entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        actual.sort();
        assert_eq!(
            actual, expected,
            "migrations/ contains only migration files"
        );
    }

    #[test]
    fn fts_migration_backfills_notes_from_an_existing_board() {
        let dir = scratch_dir("schema-fts-backfill");
        let db = dir.join("board.sqlite3");
        let backups = dir.join("backups");
        let mut store = Store::open(&db).expect("open board database");

        // Simulate a board created before FTS shipped: only 0001 applied, then
        // a note inserted, then the build upgrades to include 0002.
        let only_init = Migrations::from_sql(&[(
            "0001_init.sql",
            include_str!("../../migrations/0001_init.sql"),
        )])
        .expect("0001 parses");
        store.migrate(&only_init, &backups).expect("0001 applies");
        add_note(store.connection(), "n1", "pre-existing text");

        store
            .migrate(&migrations(), &backups)
            .expect("0002 applies");
        assert_eq!(
            fts_count(store.connection(), "existing"),
            1,
            "0002 indexes notes that existed before FTS"
        );
        let found = search_notes(store.connection(), "existing", 10).expect("search works");
        assert_eq!(
            found,
            vec![("n1".to_owned(), "pre-existing text".to_owned())]
        );
    }

    #[test]
    fn check_constraints_reject_bad_rows() {
        let store = board("schema-checks");
        let conn = store.connection();
        add_note(conn, "n1", "first");
        add_note(conn, "n2", "second");
        add_edge(conn, "e1", "n1", "n2");
        add_batch(conn, "b1", "apply");
        add_batch(conn, "b2", "undo");

        let cases: &[(&str, &str)] = &[
            (
                "group blank name",
                "INSERT INTO groups (id, name, created_at_utc_ms, updated_at_utc_ms)
                 VALUES ('gbad1', '   ', 1, 1)",
            ),
            (
                "group negative colour",
                "INSERT INTO groups (id, name, color, created_at_utc_ms, updated_at_utc_ms)
                 VALUES ('gbad2', 'ok', -1, 1, 1)",
            ),
            (
                "note bad status",
                "INSERT INTO notes (id, status, created_at_utc_ms, updated_at_utc_ms)
                 VALUES ('nbad1', 'archived', 1, 1)",
            ),
            (
                "note bad priority",
                "INSERT INTO notes (id, priority, created_at_utc_ms, updated_at_utc_ms)
                 VALUES ('nbad2', 'urgent', 1, 1)",
            ),
            (
                "note bad layout mode",
                "INSERT INTO notes (id, layout_mode, created_at_utc_ms, updated_at_utc_ms)
                 VALUES ('nbad3', 'free', 1, 1)",
            ),
            (
                "pinned note without a position",
                "INSERT INTO notes (id, layout_mode, created_at_utc_ms, updated_at_utc_ms)
                 VALUES ('nbad4', 'pinned', 1, 1)",
            ),
            (
                "done note without completed_at",
                "INSERT INTO notes (id, status, created_at_utc_ms, updated_at_utc_ms)
                 VALUES ('nbad5', 'done', 1, 1)",
            ),
            (
                "open note with completed_at",
                "INSERT INTO notes (id, completed_at_utc_ms, created_at_utc_ms, updated_at_utc_ms)
                 VALUES ('nbad6', 1, 1, 1)",
            ),
            (
                "partial due triple",
                "INSERT INTO notes (id, due_local, created_at_utc_ms, updated_at_utc_ms)
                 VALUES ('nbad7', '2026-01-01T09:00:00', 1, 1)",
            ),
            (
                "partial remind triple",
                "INSERT INTO notes (id, remind_tz, created_at_utc_ms, updated_at_utc_ms)
                 VALUES ('nbad8', 'Europe/London', 1, 1)",
            ),
            (
                "partial snooze triple",
                "INSERT INTO notes (id, snooze_utc_ms, created_at_utc_ms, updated_at_utc_ms)
                 VALUES ('nbad9', 1, 1, 1)",
            ),
            (
                "negative version",
                "INSERT INTO notes (id, version, created_at_utc_ms, updated_at_utc_ms)
                 VALUES ('nbad10', 0, 1, 1)",
            ),
            (
                "negative colour",
                "INSERT INTO notes (id, color, created_at_utc_ms, updated_at_utc_ms)
                 VALUES ('nbad11', -1, 1, 1)",
            ),
            (
                "blank source",
                "INSERT INTO notes (id, source, created_at_utc_ms, updated_at_utc_ms)
                 VALUES ('nbad12', '  ', 1, 1)",
            ),
            (
                "self parent",
                "INSERT INTO notes (id, parent_id, created_at_utc_ms, updated_at_utc_ms)
                 VALUES ('nself', 'nself', 1, 1)",
            ),
            (
                "self edge",
                "INSERT INTO edges (id, \"from\", \"to\", created_at_utc_ms, updated_at_utc_ms)
                 VALUES ('ebad1', 'n1', 'n1', 1, 1)",
            ),
            (
                "duplicate live edge",
                "INSERT INTO edges (id, \"from\", \"to\", kind, created_at_utc_ms, updated_at_utc_ms)
                 VALUES ('ebad2', 'n1', 'n2', 'link', 1, 1)",
            ),
            (
                "batch bad kind",
                "INSERT INTO batches (id, created_at_utc_ms, actor, kind)
                 VALUES ('bbad1', 1, 'human', 'nope')",
            ),
            (
                "batch bad actor",
                "INSERT INTO batches (id, created_at_utc_ms, actor, kind)
                 VALUES ('bbad2', 1, 'robot', 'apply')",
            ),
            (
                "undo target self-reference",
                "INSERT INTO undo_targets (undo_batch_id, target_batch_id)
                 VALUES ('b1', 'b1')",
            ),
            (
                "event negative seq",
                "INSERT INTO events
                     (batch_id, seq, at_utc_ms, actor, command, entity, entity_id, payload_json)
                 VALUES ('b1', -1, 1, 'human', 'x', 'note', 'n1', '{}')",
            ),
            (
                "event bad actor",
                "INSERT INTO events
                     (batch_id, seq, at_utc_ms, actor, command, entity, entity_id, payload_json)
                 VALUES ('b1', 0, 1, 'robot', 'x', 'note', 'n1', '{}')",
            ),
            (
                "event invalid payload JSON",
                "INSERT INTO events
                     (batch_id, seq, at_utc_ms, actor, command, entity, entity_id, payload_json)
                 VALUES ('b1', 0, 1, 'human', 'x', 'note', 'n1', 'not json')",
            ),
            (
                "snapshot invalid data JSON",
                "INSERT INTO snapshots (id, at_utc_ms, data, reason)
                 VALUES ('sbad1', 1, 'not json', 'test')",
            ),
            (
                "snapshot blank reason",
                "INSERT INTO snapshots (id, at_utc_ms, data, reason)
                 VALUES ('sbad2', 1, '{}', '  ')",
            ),
        ];

        for (label, sql) in cases {
            let error = conn
                .execute(sql, [])
                .expect_err(&format!("{label} must be rejected"));
            assert_constraint(error, label);
        }
    }

    #[test]
    fn foreign_key_violations_are_rejected() {
        let store = board("schema-fk");
        let conn = store.connection();
        add_group(conn);
        add_note(conn, "n1", "first");
        conn.execute(
            "INSERT INTO notes (id, group_id, created_at_utc_ms, updated_at_utc_ms)
             VALUES ('n_grouped', 'g1', 1, 1)",
            [],
        )
        .expect("grouped note inserts");

        let cases: &[(&str, &str)] = &[
            (
                "note with missing group",
                "INSERT INTO notes (id, group_id, created_at_utc_ms, updated_at_utc_ms)
                 VALUES ('nfk1', 'ghost', 1, 1)",
            ),
            (
                "note with missing parent",
                "INSERT INTO notes (id, parent_id, created_at_utc_ms, updated_at_utc_ms)
                 VALUES ('nfk2', 'ghost', 1, 1)",
            ),
            (
                "edge with missing endpoint",
                "INSERT INTO edges (id, \"from\", \"to\", created_at_utc_ms, updated_at_utc_ms)
                 VALUES ('efk1', 'ghost', 'n1', 1, 1)",
            ),
            (
                "hard-deleting a referenced group",
                "DELETE FROM groups WHERE id = 'g1'",
            ),
        ];
        for (label, sql) in cases {
            let error = conn
                .execute(sql, [])
                .expect_err(&format!("{label} must be rejected"));
            assert_constraint(error, label);
        }

        let violations = conn
            .prepare("PRAGMA foreign_key_check")
            .expect("prepare foreign key check")
            .exists([])
            .expect("run foreign key check");
        assert!(!violations, "foreign_key_check reports no rows");
    }

    #[test]
    fn updates_bump_version_and_updated_at() {
        let store = board("schema-touch");
        let conn = store.connection();
        add_group(conn);
        add_note(conn, "n1", "first");
        add_note(conn, "n2", "second");
        add_edge(conn, "e1", "n1", "n2");

        assert_touch(
            conn,
            "notes",
            "n1",
            "UPDATE notes SET text = 'second text' WHERE id = 'n1'",
        );
        assert_touch(
            conn,
            "notes",
            "n1",
            "UPDATE notes SET priority = 'high' WHERE id = 'n1'",
        );
        assert_touch(
            conn,
            "groups",
            "g1",
            "UPDATE groups SET name = 'Errands' WHERE id = 'g1'",
        );
        assert_touch(
            conn,
            "edges",
            "e1",
            "UPDATE edges SET kind = 'blocks' WHERE id = 'e1'",
        );

        let versions: Vec<(String, i64)> = conn
            .prepare("SELECT id, version FROM notes ORDER BY id")
            .expect("prepare note versions")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query note versions")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("read note versions");
        assert_eq!(
            versions,
            vec![("n1".to_owned(), 3), ("n2".to_owned(), 1)],
            "only the updated note bumps"
        );
    }

    #[test]
    fn append_only_triggers_reject_update_and_delete() {
        let store = board("schema-append-only");
        let conn = store.connection();
        add_batch(conn, "b1", "apply");
        add_batch(conn, "b2", "undo");
        conn.execute(
            "INSERT INTO undo_targets (undo_batch_id, target_batch_id) VALUES ('b2', 'b1')",
            [],
        )
        .expect("undo target inserts");
        conn.execute(
            "INSERT INTO events
                 (id, batch_id, seq, at_utc_ms, actor, command, entity, entity_id, payload_json)
             VALUES (1, 'b1', 0, 1, 'human', 'AddNote', 'note', 'n1', '{}')",
            [],
        )
        .expect("event inserts");
        conn.execute(
            "INSERT INTO snapshots (id, batch_id, at_utc_ms, data, reason)
             VALUES ('s1', 'b1', 1, '{}', 'test')",
            [],
        )
        .expect("snapshot inserts");

        let before = [
            row_count(conn, "batches"),
            row_count(conn, "undo_targets"),
            row_count(conn, "events"),
            row_count(conn, "snapshots"),
            row_count(conn, "schema_migrations"),
        ];

        let cases: &[(&str, &str, &str)] = &[
            (
                "batches",
                "UPDATE batches SET label = 'edited' WHERE id = 'b1'",
                "DELETE FROM batches WHERE id = 'b1'",
            ),
            (
                "undo_targets",
                "UPDATE undo_targets SET target_batch_id = 'b2' WHERE undo_batch_id = 'b2'",
                "DELETE FROM undo_targets WHERE undo_batch_id = 'b2'",
            ),
            (
                "events",
                "UPDATE events SET command = 'edited' WHERE id = 1",
                "DELETE FROM events WHERE id = 1",
            ),
            (
                "snapshots",
                "UPDATE snapshots SET reason = 'edited' WHERE id = 's1'",
                "DELETE FROM snapshots WHERE id = 's1'",
            ),
            (
                "schema_migrations",
                "UPDATE schema_migrations SET checksum = 'edited' WHERE version = 1",
                "DELETE FROM schema_migrations WHERE version = 1",
            ),
        ];

        for (table, update_sql, delete_sql) in cases {
            let error = conn
                .execute(update_sql, [])
                .expect_err(&format!("{table} UPDATE must abort"));
            assert_append_only(error, table);

            let error = conn
                .execute(delete_sql, [])
                .expect_err(&format!("{table} DELETE must abort"));
            assert_append_only(error, table);
        }

        let after = [
            row_count(conn, "batches"),
            row_count(conn, "undo_targets"),
            row_count(conn, "events"),
            row_count(conn, "snapshots"),
            row_count(conn, "schema_migrations"),
        ];
        assert_eq!(after, before, "aborted writes leave audit rows untouched");
    }

    #[test]
    fn fts_tracks_text_and_soft_delete() {
        let store = board("schema-fts");
        let conn = store.connection();
        add_note(conn, "n1", "Buy milk and eggs");
        add_note(conn, "n2", "Write the report");

        assert_eq!(fts_count(conn, "milk"), 1);
        let found = search_notes(conn, "milk", 10).expect("search works");
        assert_eq!(
            found,
            vec![("n1".to_owned(), "Buy milk and eggs".to_owned())]
        );

        conn.execute("UPDATE notes SET text = 'Buy oat milk' WHERE id = 'n1'", [])
            .expect("text update applies");
        assert_eq!(fts_count(conn, "eggs"), 0, "old text leaves the index");
        assert_eq!(fts_count(conn, "oat"), 1, "new text enters the index");
        assert!(search_notes(conn, "eggs", 10).expect("search").is_empty());

        conn.execute(
            "UPDATE notes SET deleted_at_utc_ms = ?1 WHERE id = 'n1'",
            [T0 + 1],
        )
        .expect("soft delete applies");
        assert_eq!(
            fts_count(conn, "oat"),
            0,
            "soft delete removes the index row"
        );
        assert!(
            search_notes(conn, "oat", 10).expect("search").is_empty(),
            "search never returns a soft-deleted note"
        );
        assert_eq!(fts_count(conn, "report"), 1, "other notes stay indexed");

        conn.execute(
            "UPDATE notes SET deleted_at_utc_ms = NULL WHERE id = 'n1'",
            [],
        )
        .expect("restore applies");
        assert_eq!(fts_count(conn, "oat"), 1, "restore re-indexes the note");
        assert_eq!(search_notes(conn, "oat", 10).expect("search").len(), 1);

        conn.execute("DELETE FROM notes WHERE id = 'n1'", [])
            .expect("hard delete applies");
        assert_eq!(fts_count(conn, "oat"), 0, "hard delete leaves the index");

        conn.execute(
            "INSERT INTO notes_fts (notes_fts) VALUES ('integrity-check')",
            [],
        )
        .expect("FTS index is internally consistent");
    }

    #[test]
    fn integrity_checks_pass_after_a_normal_workload() {
        let store = board("schema-integrity");
        let conn = store.connection();
        add_group(conn);
        add_note(conn, "n1", "alpha");
        add_note(conn, "n2", "beta");
        add_edge(conn, "e1", "n1", "n2");
        conn.execute(
            "UPDATE notes
                SET due_local = '2026-03-29T09:00:00',
                    due_tz = 'Europe/London',
                    due_utc_ms = ?1,
                    remind_local = '2026-03-29T08:30:00',
                    remind_tz = 'Europe/London',
                    remind_utc_ms = ?2,
                    snooze_local = '2026-03-28T20:00:00',
                    snooze_tz = 'Europe/London',
                    snooze_utc_ms = ?3
              WHERE id = 'n2'",
            params![T0, T0 + 1_000, T0 + 2_000],
        )
        .expect("civil triples apply");
        conn.execute(
            "UPDATE notes
                SET status = 'done', completed_at_utc_ms = ?1
              WHERE id = 'n2'",
            [T0 + 3_000],
        )
        .expect("completion applies");
        conn.execute(
            "UPDATE notes SET deleted_at_utc_ms = ?1 WHERE id = 'n1'",
            [T0 + 4_000],
        )
        .expect("soft delete applies");

        add_batch(conn, "b1", "apply");
        conn.execute(
            "INSERT INTO events
                 (batch_id, seq, at_utc_ms, actor, command, entity, entity_id, payload_json)
             VALUES ('b1', 0, ?1, 'agent', 'SetDue', 'note', 'n2', '{\"due\":\"2026-03-29\"}')",
            [T0],
        )
        .expect("event inserts");
        conn.execute(
            "INSERT INTO snapshots (id, batch_id, at_utc_ms, data, reason)
             VALUES ('s1', 'b1', ?1, '{\"notes\":[]}', 'board_sql')",
            [T0],
        )
        .expect("snapshot inserts");

        let violations = conn
            .prepare("PRAGMA foreign_key_check")
            .expect("prepare foreign key check")
            .exists([])
            .expect("run foreign key check");
        assert!(!violations, "foreign_key_check returns no rows");

        let problems: Vec<String> = conn
            .prepare("PRAGMA integrity_check")
            .expect("prepare integrity check")
            .query_map([], |row| row.get(0))
            .expect("run integrity check")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("read integrity rows");
        assert_eq!(problems, vec!["ok".to_owned()]);

        conn.execute(
            "INSERT INTO notes_fts (notes_fts) VALUES ('integrity-check')",
            [],
        )
        .expect("FTS index is consistent after the workload");
    }

    #[test]
    fn table_columns_lists_logical_tables_and_columns() {
        let store = board("schema-tables");
        let tables = table_columns(store.connection()).expect("table listing works");
        let names: Vec<&str> = tables.iter().map(|table| table.table.as_str()).collect();

        for expected in [
            "batches",
            "edges",
            "events",
            "groups",
            "notes",
            "notes_fts",
            "schema_migrations",
            "snapshots",
            "undo_targets",
        ] {
            assert!(names.contains(&expected), "{expected} is listed");
        }
        assert!(
            !names.iter().any(|name| name.starts_with("notes_fts_")),
            "FTS shadow tables are hidden"
        );

        let notes = tables
            .iter()
            .find(|table| table.table == "notes")
            .expect("notes is listed");
        for expected in [
            "due_local",
            "due_tz",
            "due_utc_ms",
            "remind_local",
            "remind_tz",
            "remind_utc_ms",
            "reminded_at_utc_ms",
            "snooze_local",
            "snooze_tz",
            "snooze_utc_ms",
            "completed_at_utc_ms",
            "deleted_at_utc_ms",
            "version",
            "layout_mode",
            "pos_x",
            "pos_y",
            "sort_order",
        ] {
            assert!(
                notes.columns.iter().any(|column| column == expected),
                "notes.{expected} is listed"
            );
        }
    }
}

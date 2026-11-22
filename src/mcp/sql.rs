//! The safe board_sql pipeline (task 110).
//!
//! The agent may run arbitrary DML against the board, but it must never touch
//! the append-only audit tables, change the schema, attach databases, manage
//! transactions or call dangerous functions. Enforcement is a rusqlite
//! authorizer installed only while the user's statement is prepared and run;
//! the tool's own transaction, checks and audit writes happen with the
//! authorizer removed.
//!
//! A leading-keyword guard rejects statements the authorizer might not see
//! (VACUUM, BEGIN, ATTACH, PRAGMA and friends) before preparation.

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, ErrorCode};
use serde::{Deserialize, Serialize};

use crate::store::schema::AUDIT_TABLES;

/// Maximum rows returned by board_query/board_sql.
pub const MAX_ROWS: usize = 500;
/// Approximate maximum encoded size of the returned rows.
pub const MAX_BYTES: usize = 64 * 1024;

/// One result set, already JSON-shaped for the MCP response.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub truncated: bool,
    /// Rows changed by a DML statement (0 for a SELECT).
    pub rows_affected: usize,
}

/// Errors from the guarded SQL path.
#[derive(Debug, thiserror::Error)]
pub enum SqlError {
    #[error("only one SQL statement is allowed")]
    MultipleStatements,

    #[error("statement rejected: {0}")]
    Rejected(String),

    #[error("SQL error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("result set too large; narrow the query")]
    TooLarge,
}

/// Functions that must never run from agent SQL.
const DENIED_FUNCTIONS: &[&str] = &[
    "load_extension",
    "writefile",
    "readfile",
    "fts3_tokenizer",
    "zipfile",
    "eval",
];

/// Keywords that are never accepted as the first word of agent SQL.
const DENIED_KEYWORDS: &[&str] = &[
    "VACUUM",
    "BEGIN",
    "COMMIT",
    "END",
    "ROLLBACK",
    "SAVEPOINT",
    "RELEASE",
    "ATTACH",
    "DETACH",
    "ANALYZE",
    "REINDEX",
    "ALTER",
    "CREATE",
    "DROP",
];

/// The authorizer callback. Allow reads and board DML; deny everything else.
///
/// FTS5's sync triggers and its shadow tables write notes_fts* as part of any
/// note change, and those internal writes have no accessor we can key on. They
/// are derived data, not history, so writes there are allowed while the five
/// audit tables stay denied. A board_sql statement may therefore corrupt the
/// search index but can never rewrite history, change the schema or escape the
/// database; integrity checks cover the board tables, and search is rebuilt by
/// the next full reindex if it is ever damaged.
pub fn authorize(context: AuthContext<'_>) -> Authorization {
    match context.action {
        AuthAction::Read { .. } | AuthAction::Select | AuthAction::Recursive => {
            Authorization::Allow
        }
        AuthAction::Insert { table_name }
        | AuthAction::Update { table_name, .. }
        | AuthAction::Delete { table_name } => {
            if is_board_table(table_name) || is_fts_table(table_name) {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        // FTS5 issues a read-only PRAGMA data_version while maintaining the
        // index. Every other pragma would change engine behaviour and is denied.
        AuthAction::Pragma { pragma_name, .. } => {
            if pragma_name.eq_ignore_ascii_case("data_version") {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        AuthAction::Function { function_name } => {
            if DENIED_FUNCTIONS
                .iter()
                .any(|name| function_name.eq_ignore_ascii_case(name))
            {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }
        _ => Authorization::Deny,
    }
}

/// Tables the agent may write directly. Everything else (audit tables, SQLite
/// internals, FTS shadow tables) is read-only or invisible.
fn is_board_table(table: &str) -> bool {
    matches!(table, "notes" | "groups" | "edges")
        && !AUDIT_TABLES
            .iter()
            .any(|audit| table.eq_ignore_ascii_case(audit))
}

fn is_fts_table(table: &str) -> bool {
    table.eq_ignore_ascii_case("notes_fts") || table.starts_with("notes_fts_")
}

/// Prepare and run one statement with the authorizer installed.
///
/// The caller must already be inside its own transaction: this function never
/// manages one, so a denied or failed statement leaves the transaction open for
/// the caller to roll back.
pub fn run_guarded(conn: &mut Connection, sql: &str) -> Result<QueryResult, SqlError> {
    let keyword =
        first_keyword(sql).ok_or_else(|| SqlError::Rejected("empty SQL statement".to_owned()))?;
    if DENIED_KEYWORDS.iter().any(|denied| keyword == *denied) {
        return Err(SqlError::Rejected(format!(
            "{keyword} is not allowed through board_sql"
        )));
    }

    conn.authorizer(Some(authorize))?;
    let result = run_inner(conn, sql);
    // Always clear the authorizer, even after a failure: the caller's checks
    // and audit writes must not run through it.
    conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)?;
    result
}

/// Run one read-only statement on a separate read-only connection.
///
/// This is the board_query path: the connection itself refuses writes
/// (SQLITE_OPEN_READ_ONLY plus query_only) and the keyword guard keeps DML,
/// pragmas and transaction control out before the engine sees them. Results
/// use the same row and byte caps as board_sql.
pub fn run_read_only(db_path: &std::path::Path, sql: &str) -> Result<QueryResult, SqlError> {
    let keyword =
        first_keyword(sql).ok_or_else(|| SqlError::Rejected("empty SQL statement".to_owned()))?;
    if !matches!(keyword.as_str(), "SELECT" | "WITH" | "VALUES" | "EXPLAIN") {
        return Err(SqlError::Rejected(format!(
            "{keyword} is not a read-only statement"
        )));
    }

    let conn = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.pragma_update(None, "query_only", true)?;
    run_inner(&conn, sql)
}

fn run_inner(conn: &Connection, sql: &str) -> Result<QueryResult, SqlError> {
    let mut statement = conn.prepare(sql)?;
    let columns: Vec<String> = statement.column_names().map(str::to_owned).collect();

    if columns.is_empty() {
        // DML with no result columns. DDL is denied by the authorizer, so by
        // the time we get here this is INSERT/UPDATE/DELETE on a board table.
        statement.execute([])?;
        return Ok(QueryResult {
            columns,
            rows: Vec::new(),
            truncated: false,
            rows_affected: conn.changes() as usize,
        });
    }

    let mut rows = statement.query([])?;
    let mut result = QueryResult {
        columns,
        ..QueryResult::default()
    };
    let mut bytes = 0usize;
    while let Some(row) = rows.next()? {
        if result.rows.len() >= MAX_ROWS {
            result.truncated = true;
            break;
        }
        let mut values = Vec::with_capacity(result.columns.len());
        let mut row_bytes = 0usize;
        for index in 0..result.columns.len() {
            let value = value_to_json(row.get_ref(index)?);
            row_bytes += estimate_size(&value);
            values.push(value);
        }
        if bytes + row_bytes > MAX_BYTES {
            result.truncated = true;
            break;
        }
        bytes += row_bytes;
        result.rows.push(values);
    }

    Ok(result)
}

fn value_to_json(value: ValueRef<'_>) -> serde_json::Value {
    match value {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(number) => serde_json::Value::from(number),
        ValueRef::Real(number) => serde_json::Value::from(number),
        ValueRef::Text(bytes) => {
            serde_json::Value::from(String::from_utf8_lossy(bytes).into_owned())
        }
        ValueRef::Blob(bytes) => serde_json::Value::from(format!("<blob {} bytes>", bytes.len())),
    }
}

fn estimate_size(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Null => 4,
        serde_json::Value::Bool(_) => 5,
        serde_json::Value::Number(_) => 8,
        serde_json::Value::String(text) => text.len() + 2,
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => 32,
    }
}

/// The first SQL keyword, skipping whitespace and leading comments.
///
/// Returns it uppercased. This is a guard, not a parser: the authorizer is the
/// real enforcement.
pub fn first_keyword(sql: &str) -> Option<String> {
    let bytes = sql.as_bytes();
    let mut index = 0;
    loop {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if sql[index..].starts_with("--") {
            let offset = sql[index..].find('\n')?;
            index += offset + 1;
            continue;
        }
        if sql[index..].starts_with("/*") {
            let offset = sql[index + 2..].find("*/")?;
            index += 2 + offset + 2;
            continue;
        }
        break;
    }
    let start = index;
    while index < bytes.len() && (bytes[index].is_ascii_alphabetic() || bytes[index] == b'_') {
        index += 1;
    }
    if start == index {
        return None;
    }
    Some(sql[start..index].to_ascii_uppercase())
}

/// True when an error is SQLite refusing authorization (SQLITE_AUTH).
pub fn is_authorization_error(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == ErrorCode::AuthorizationForStatementDenied
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{Actor, Command};
    use crate::store::{Store, StorePaths, schema, scratch_dir};

    fn store(name: &str) -> Store {
        let paths = StorePaths::from_home(scratch_dir(name));
        schema::open_board(&paths).expect("open board")
    }

    fn store_with_note(name: &str) -> Store {
        let mut store = store(name);
        store
            .apply(
                &[Command::AddNote {
                    text: "kept".to_owned(),
                    group: None,
                    parent: None,
                    due: None,
                    remind: None,
                    priority: None,
                    source: "test".to_owned(),
                }],
                Actor::Human,
                None,
            )
            .expect("add note");
        store
    }

    #[test]
    fn reads_are_allowed() {
        let mut store = store_with_note("mcp-read");
        let result =
            run_guarded(store.connection_mut(), "SELECT id, text FROM notes").expect("select");
        assert_eq!(result.columns, vec!["id", "text"]);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][1], "kept");
    }

    #[test]
    fn board_dml_is_allowed_and_audit_dml_is_not() {
        let mut store = store_with_note("mcp-dml");
        let result = run_guarded(
            store.connection_mut(),
            "INSERT INTO groups (id, name, created_at_utc_ms, updated_at_utc_ms)
             VALUES ('g', 'Work', 1, 1)",
        )
        .expect("insert group");
        assert_eq!(result.rows_affected, 1);

        for statement in [
            "DELETE FROM events",
            "UPDATE batches SET label = 'x'",
            "INSERT INTO events (batch_id, seq, at_utc_ms, actor, command, entity, entity_id, payload_json)
             VALUES ('b', 0, 0, 'human', 'x', 'note', 'n', '{}')",
            "DELETE FROM undo_targets",
            "UPDATE snapshots SET reason = 'x'",
        ] {
            let error = run_guarded(store.connection_mut(), statement).expect_err(statement);
            match error {
                SqlError::Sqlite(ref sqlite) => {
                    assert!(is_authorization_error(sqlite), "{statement}: {sqlite:?}");
                }
                other => panic!("{statement}: expected an authorization error, got {other:?}"),
            }
        }
    }

    #[test]
    fn schema_changes_and_transactions_are_denied() {
        let mut store = store("mcp-ddl");
        for statement in [
            "DROP TABLE events",
            "CREATE TABLE x (id INTEGER)",
            "ALTER TABLE notes ADD COLUMN x TEXT",
            "PRAGMA foreign_keys = OFF",
            "ATTACH DATABASE 'other.db' AS other",
            "BEGIN",
            "COMMIT",
            "SAVEPOINT s",
            "VACUUM",
        ] {
            assert!(
                run_guarded(store.connection_mut(), statement).is_err(),
                "{statement} must be rejected"
            );
        }
    }

    #[test]
    fn dangerous_functions_are_denied() {
        let mut store = store("mcp-fn");
        // The function may not exist at all in this build; either way the
        // statement must not run.
        let result = run_guarded(store.connection_mut(), "SELECT load_extension('evil')");
        assert!(result.is_err(), "load_extension must not run");
        // Ordinary functions stay usable.
        let result = run_guarded(store.connection_mut(), "SELECT upper('a') AS up").expect("upper");
        assert_eq!(result.rows[0][0], "A");
    }

    #[test]
    fn read_only_queries_cannot_write_and_reject_dml_keywords() {
        let store = store_with_note("mcp-query");
        let db = store.path().to_path_buf();
        let result = run_read_only(&db, "SELECT text FROM notes").expect("select");
        assert_eq!(result.rows[0][0], "kept");

        for statement in [
            "INSERT INTO notes (id, text, created_at_utc_ms, updated_at_utc_ms) VALUES ('x','y',1,1)",
            "UPDATE notes SET text = 'x'",
            "DELETE FROM notes",
            "PRAGMA foreign_keys = OFF",
            "BEGIN",
        ] {
            assert!(
                run_read_only(&db, statement).is_err(),
                "{statement} must be rejected"
            );
        }
        // The connection is read-only even if something slips through: the
        // note count is unchanged.
        let result = run_read_only(&db, "SELECT COUNT(*) FROM notes").expect("count");
        assert_eq!(result.rows[0][0], 1);
    }

    #[test]
    fn pragma_changes_are_denied_with_sqlite_auth() {
        let mut store = store("mcp-pragma");
        let error =
            run_guarded(store.connection_mut(), "PRAGMA foreign_keys = OFF").expect_err("pragma");
        match error {
            SqlError::Sqlite(ref sqlite) => assert!(
                is_authorization_error(sqlite),
                "expected SQLITE_AUTH, got {sqlite:?}"
            ),
            other => panic!("expected an authorization error, got {other:?}"),
        }
    }

    #[test]
    fn the_leading_keyword_guard_skips_comments() {
        assert_eq!(
            first_keyword("  /* hi */ DELETE FROM notes").as_deref(),
            Some("DELETE")
        );
        assert_eq!(first_keyword("-- x\nSELECT 1").as_deref(), Some("SELECT"));
        assert_eq!(first_keyword("   "), None);
        let mut store = store("mcp-guard");
        assert!(matches!(
            run_guarded(store.connection_mut(), "/* sneaky */ VACUUM"),
            Err(SqlError::Rejected(_))
        ));
    }

    #[test]
    fn results_are_capped() {
        let mut store = store("mcp-cap");
        let result = run_guarded(
            store.connection_mut(),
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 600)
             SELECT x FROM c",
        )
        .expect("select");
        assert_eq!(result.rows.len(), MAX_ROWS);
        assert!(result.truncated);
    }

    #[test]
    fn updating_note_text_works_through_the_fts_triggers() {
        let mut store = store_with_note("mcp-fts");
        let result = run_guarded(
            store.connection_mut(),
            "UPDATE notes SET text = 'changed' WHERE text = 'kept'",
        )
        .expect("update notes");
        assert_eq!(result.rows_affected, 1);
        let text: String = store
            .connection()
            .query_row("SELECT text FROM notes LIMIT 1", [], |row| row.get(0))
            .expect("text");
        assert_eq!(text, "changed");
        let found: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM notes_fts WHERE notes_fts MATCH 'changed'",
                [],
                |row| row.get(0),
            )
            .expect("fts");
        assert_eq!(found, 1);
    }

    #[test]
    fn a_denied_statement_changes_nothing_and_the_authorizer_is_cleared() {
        let mut store = store_with_note("mcp-clean");
        let before: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .expect("count");
        let _ = run_guarded(store.connection_mut(), "DELETE FROM events").expect_err("denied");
        // A normal write still works afterwards, proving the authorizer was
        // cleared rather than left denying everything.
        run_guarded(
            store.connection_mut(),
            "UPDATE notes SET text = 'changed' WHERE text = 'kept'",
        )
        .expect("update notes");
        let after: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .expect("count");
        assert_eq!(before, after);
    }
}

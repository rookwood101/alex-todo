//! Plain, synchronous board tool implementations (task 110).
//!
//! Each function here is the whole behaviour of one MCP tool and is unit
//! tested without rmcp, a transport or a live Pi. The async server in
//! super::server only decodes arguments, calls one of these functions and
//! serializes the result, so the security-critical board_sql path is the
//! same code the tests exercise.
//!
//! Every function opens its own connection and drops it before returning.
//! rusqlite is synchronous, so no rusqlite::Connection ever crosses an
//! .await.

use std::path::{Path, PathBuf};

use rmcp::schemars;
use rmcp::schemars::{JsonSchema, Schema, SchemaGenerator};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::command::{Actor, Applied, Command, CommandError};
use crate::history::{self, BatchSummary, HistoryEntry, HistoryError, record_snapshot};
use crate::mcp::sql::{self, SqlError};
use crate::store::schema::{self, AUDIT_TABLES, TableColumns};
use crate::store::{Store, StoreError, StorePaths};
use crate::time::validate_zone;

/// Rows returned by board_query when no smaller limit is asked for.
pub const MAX_QUERY_ROWS: i64 = sql::MAX_ROWS as i64;

/// Above this size integrity_check is replaced by quick_check.
const QUICK_CHECK_BYTES: i64 = 50 * 1024 * 1024;

/// Tables board_sql may write.
const BOARD_TABLES: &[&str] = &["notes", "groups", "edges"];

/// The managed value reported for the append-only audit tables.
pub const MANAGED_READ_ONLY: &str = "read-only";
/// The managed value reported for notes, groups and edges.
pub const MANAGED_READ_WRITE: &str = "read-write";

/// Errors from a board tool.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error(
        "the board database does not exist at {path}; start Rustboard once or pass --db with an existing board"
    )]
    DatabaseMissing { path: PathBuf },

    #[error(transparent)]
    Store(#[from] StoreError),

    #[error("board_sql failed check sql: {0}")]
    Sql(#[from] SqlError),

    #[error("board_query rejected the statement: {0}")]
    Query(SqlError),

    #[error(transparent)]
    Command(#[from] CommandError),

    #[error(transparent)]
    History(#[from] HistoryError),

    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("invalid argument: {message}")]
    Invalid { message: String },

    #[error("board_sql failed check {check}: {detail}")]
    CheckFailed { check: &'static str, detail: String },
}

/// One table and its columns, as reported by board_schema.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TableSchema {
    pub table: String,
    pub columns: Vec<String>,
    /// "read-only" for the five audit tables, "read-write" for board tables.
    pub managed: String,
}

/// The result of board_schema.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BoardSchema {
    pub tables: Vec<TableSchema>,
}

/// Arguments for board_query.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BoardQueryArgs {
    /// One SELECT (or WITH/VALUES/EXPLAIN) statement.
    pub sql: String,
    /// Maximum rows to return, clamped to 1..=500.
    #[serde(default)]
    pub limit: Option<i64>,
}

/// The result of board_query.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BoardQueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub truncated: bool,
}

/// Arguments for board_apply.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BoardApplyArgs {
    /// Commands from the shared command layer, applied as one batch.
    pub commands: Vec<Command>,
    /// Human-readable batch label.
    #[serde(default)]
    pub description: Option<String>,
    /// Pi turn or tool-call reference, for task 130's transcript.
    #[serde(default)]
    pub actor_ref: Option<String>,
}

/// Arguments for board_sql.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BoardSqlArgs {
    /// One DML or SELECT statement. DML on notes/groups/edges only.
    pub sql: String,
    /// Required audit label; stored on the batch and as the snapshot reason.
    pub description: String,
}

/// The result of board_sql.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BoardSqlResult {
    /// The apply batch recorded for this statement.
    pub batch_id: String,
    /// Rows changed by the statement (0 for a SELECT).
    pub changes: usize,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub truncated: bool,
    /// Names of the checks that passed before the batch committed.
    pub checks: Vec<String>,
}

/// Arguments for board_history.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BoardHistoryArgs {
    /// A note id: return that note's events.
    #[serde(default)]
    pub note_id: Option<String>,
    /// Batch range start, epoch milliseconds UTC. Defaults to 0.
    #[serde(default)]
    pub from_utc_ms: Option<i64>,
    /// Batch range end, epoch milliseconds UTC. Defaults to now.
    #[serde(default)]
    pub to_utc_ms: Option<i64>,
}

/// The result of board_history.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BoardHistoryResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_utc_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_utc_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<HistoryEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub batches: Vec<BatchSummary>,
}

/// The minimum schema of Command, for board_apply's MCP input schema.
///
/// Command lives in the command module and is not allowed to depend on
/// schemars, so the impl lives here next to the MCP surface that needs it. It
/// lists every variant and its own fields; unknown fields follow serde's
/// permissive default. Keep it in step with Command.
impl JsonSchema for Command {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("Command")
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        let note_id = || json!({"type": "string", "description": "Note id"});
        let group = || json!({"type": ["string", "null"], "description": "Group name; null moves the note to the Inbox"});
        let parent = || json!({"type": ["string", "null"], "description": "Parent note id; null makes the note a root"});
        let time = time_spec_schema;
        let priority = || json!({"type": ["string", "null"], "enum": ["low", "med", "high", null]});
        let mut one_of = Vec::new();
        one_of.push(command_variant(
            "add_note",
            &[
                ("text", json!({"type": "string"})),
                ("group", group()),
                ("parent", parent()),
                ("due", time()),
                ("remind", time()),
                ("priority", priority()),
                ("source", json!({"type": "string"})),
            ],
            &["text"],
        ));
        one_of.push(command_variant(
            "set_text",
            &[("id", note_id()), ("text", json!({"type": "string"}))],
            &["id", "text"],
        ));
        one_of.push(command_variant(
            "set_group",
            &[("id", note_id()), ("group", group())],
            &["id"],
        ));
        one_of.push(command_variant(
            "set_parent",
            &[("id", note_id()), ("parent", parent())],
            &["id"],
        ));
        one_of.push(command_variant(
            "set_priority",
            &[("id", note_id()), ("priority", priority())],
            &["id"],
        ));
        one_of.push(command_variant(
            "set_due",
            &[("id", note_id()), ("due", time())],
            &["id"],
        ));
        one_of.push(command_variant(
            "set_remind",
            &[("id", note_id()), ("remind", time())],
            &["id"],
        ));
        one_of.push(command_variant(
            "snooze",
            &[("id", note_id()), ("until", time())],
            &["id", "until"],
        ));
        for name in [
            "unsnooze", "complete", "reopen", "delete", "restore", "unpin",
        ] {
            one_of.push(command_variant(name, &[("id", note_id())], &["id"]));
        }
        one_of.push(command_variant(
            "pin",
            &[
                ("id", note_id()),
                ("x", json!({"type": "number"})),
                ("y", json!({"type": "number"})),
            ],
            &["id", "x", "y"],
        ));
        one_of.push(command_variant(
            "create_group",
            &[
                ("name", json!({"type": "string"})),
                ("color", json!({"type": "integer"})),
            ],
            &["name"],
        ));
        one_of.push(command_variant(
            "rename_group",
            &[
                ("id", json!({"type": "string", "description": "Group id"})),
                ("name", json!({"type": "string"})),
            ],
            &["id", "name"],
        ));
        one_of.push(command_variant(
            "link",
            &[
                ("from", note_id()),
                ("to", note_id()),
                ("kind", json!({"type": ["string", "null"]})),
            ],
            &["from", "to"],
        ));
        one_of.push(command_variant(
            "unlink",
            &[(
                "edge_id",
                json!({"type": "string", "description": "Edge id"}),
            )],
            &["edge_id"],
        ));

        let mut schema = Map::new();
        schema.insert("type".to_owned(), json!("object"));
        schema.insert(
            "description".to_owned(),
            json!("One command from the shared command layer; command names its variant."),
        );
        schema.insert("oneOf".to_owned(), Value::Array(one_of));
        Schema::from(schema)
    }
}

fn time_spec_schema() -> Value {
    json!({
        "type": ["object", "null"],
        "description": "Civil wall time plus IANA zone",
        "properties": {
            "local": {"type": "string", "description": "YYYY-MM-DDTHH:MM:SS"},
            "tz": {"type": "string", "description": "IANA zone, e.g. Europe/London"}
        },
        "required": ["local", "tz"]
    })
}

fn command_variant(name: &str, fields: &[(&str, Value)], required: &[&str]) -> Value {
    let mut properties = Map::new();
    properties.insert(
        "command".to_owned(),
        json!({"type": "string", "enum": [name]}),
    );
    for (field, schema) in fields {
        properties.insert((*field).to_owned(), schema.clone());
    }
    let mut required_fields = vec![json!("command")];
    required_fields.extend(required.iter().map(|field| json!(field)));
    json!({
        "type": "object",
        "properties": properties,
        "required": required_fields
    })
}

/// Reject a board that does not exist instead of letting a write tool create a
/// new one at a mistyped path.
fn ensure_database(paths: &StorePaths) -> Result<(), ToolError> {
    if paths.db().exists() {
        Ok(())
    } else {
        Err(ToolError::DatabaseMissing {
            path: paths.db().to_path_buf(),
        })
    }
}

/// List tables and columns, marking the append-only audit tables read-only.
pub fn board_schema(paths: &StorePaths) -> Result<BoardSchema, ToolError> {
    ensure_database(paths)?;
    let store = Store::open_read_only(paths.db())?;
    let tables = schema::table_columns(store.connection())?;
    Ok(BoardSchema {
        tables: tables
            .into_iter()
            .map(|TableColumns { table, columns }| {
                let managed = if AUDIT_TABLES
                    .iter()
                    .any(|audit| table.eq_ignore_ascii_case(audit))
                    || table.starts_with("notes_fts")
                {
                    MANAGED_READ_ONLY
                } else if BOARD_TABLES
                    .iter()
                    .any(|board| table.eq_ignore_ascii_case(board))
                {
                    MANAGED_READ_WRITE
                } else {
                    MANAGED_READ_ONLY
                };
                TableSchema {
                    table,
                    columns,
                    managed: managed.to_owned(),
                }
            })
            .collect(),
    })
}

/// Run one read-only statement on a dedicated read-only connection.
pub fn board_query(
    paths: &StorePaths,
    args: BoardQueryArgs,
) -> Result<BoardQueryResult, ToolError> {
    ensure_database(paths)?;
    let limit = args
        .limit
        .unwrap_or(MAX_QUERY_ROWS)
        .clamp(1, MAX_QUERY_ROWS) as usize;
    let mut result = sql::run_read_only(paths.db(), &args.sql).map_err(ToolError::Query)?;
    if result.rows.len() > limit {
        result.rows.truncate(limit);
        result.truncated = true;
    }
    Ok(BoardQueryResult {
        columns: result.columns,
        rows: result.rows,
        truncated: result.truncated,
    })
}

/// Apply typed commands as one agent batch.
pub fn board_apply(paths: &StorePaths, args: BoardApplyArgs) -> Result<Applied, ToolError> {
    ensure_database(paths)?;
    let mut store = schema::open_board(paths)?;
    let applied = store.apply_with(
        &args.commands,
        Actor::Agent,
        args.actor_ref.as_deref(),
        args.description.as_deref(),
    )?;
    Ok(applied)
}

/// Run one guarded SQL statement, check the board, and commit the audit trail.
///
/// The transaction is opened before the authorizer is installed, the user's
/// statement runs through sql::run_guarded, and only after that does the
/// authorizer-free code run the checks and append the batch, one board event
/// and the before-image snapshot. Any failure rolls the whole thing back.
pub fn board_sql(paths: &StorePaths, args: BoardSqlArgs) -> Result<BoardSqlResult, ToolError> {
    ensure_database(paths)?;
    if args.description.trim().is_empty() {
        return Err(ToolError::Invalid {
            message: "description must not be blank".to_owned(),
        });
    }

    let mut store = schema::open_board(paths)?;
    let conn = store.connection_mut();
    // BEGIN IMMEDIATE without rusqlite's Transaction: run_guarded needs a
    // mutable Connection for its authorizer, and Transaction only derefs
    // immutably. The tool owns this transaction and rolls it back on every
    // error path below.
    conn.execute_batch("BEGIN IMMEDIATE")?;

    let attempt = (|| -> Result<BoardSqlResult, ToolError> {
        let before = history::capture_board(conn)?;
        let query = sql::run_guarded(conn, &args.sql)?;
        run_checks(conn, paths.db())?;
        let after = history::capture_board(conn)?;
        let batch_id = uuid::Uuid::new_v4().to_string();
        let now = crate::store::now_utc_ms();
        let payload = json!({"command": "board_sql", "sql": args.sql});

        conn.execute(
            "INSERT INTO batches (id, created_at_utc_ms, actor, actor_ref, label, kind)
             VALUES (?1, ?2, 'agent', NULL, ?3, 'apply')",
            params![batch_id, now, args.description],
        )?;
        conn.execute(
            "INSERT INTO events
                 (batch_id, seq, at_utc_ms, actor, command, entity, entity_id,
                  payload_json, before_json, after_json)
             VALUES (?1, 0, ?2, 'agent', 'board_sql', 'board', 'board', ?3, ?4, ?5)",
            params![
                batch_id,
                now,
                payload.to_string(),
                before.to_string(),
                after.to_string()
            ],
        )?;
        record_snapshot(conn, &before, &args.description, Some(&batch_id))?;

        Ok(BoardSqlResult {
            batch_id,
            changes: query.rows_affected,
            columns: query.columns,
            rows: query.rows,
            truncated: query.truncated,
            checks: check_names(),
        })
    })();

    match attempt {
        Ok(result) => {
            conn.execute_batch("COMMIT")?;
            Ok(result)
        }
        Err(error) => {
            drop(conn.execute_batch("ROLLBACK"));
            Err(error)
        }
    }
}

/// Read a note's events, or a batch range, on a read-only connection.
pub fn board_history(
    paths: &StorePaths,
    args: BoardHistoryArgs,
) -> Result<BoardHistoryResult, ToolError> {
    ensure_database(paths)?;
    let store = Store::open_read_only(paths.db())?;
    let conn = store.connection();

    if let Some(note_id) = args.note_id {
        let events = history::history(conn, &note_id)?;
        return Ok(BoardHistoryResult {
            note_id: Some(note_id),
            events,
            ..BoardHistoryResult::default()
        });
    }

    let from_utc_ms = args.from_utc_ms.unwrap_or(0);
    let to_utc_ms = args.to_utc_ms.unwrap_or_else(crate::store::now_utc_ms);
    let batches = history::batches_between(conn, from_utc_ms, to_utc_ms)?;
    Ok(BoardHistoryResult {
        from_utc_ms: Some(from_utc_ms),
        to_utc_ms: Some(to_utc_ms),
        batches,
        ..BoardHistoryResult::default()
    })
}

fn check_names() -> Vec<String> {
    CHECK_NAMES.iter().map(|name| (*name).to_owned()).collect()
}

fn run_checks(conn: &Connection, db_path: &Path) -> Result<(), ToolError> {
    check_foreign_keys(conn)?;
    check_integrity(conn, db_path)?;
    check_parent_cycles(conn)?;
    check_edge_endpoints(conn)?;
    check_time_zones(conn)?;
    Ok(())
}

fn check_foreign_keys(conn: &Connection) -> Result<(), ToolError> {
    let mut statement = conn.prepare("PRAGMA foreign_key_check")?;
    let mut rows = statement.query([])?;
    let mut problems = Vec::new();
    while let Some(row) = rows.next()? {
        let table: String = row.get(0)?;
        let rowid: Option<i64> = row.get(1)?;
        let parent: String = row.get(2)?;
        let fk: i64 = row.get(3)?;
        problems.push(format!(
            "{table} row {rowid:?} references {parent} (fk {fk})"
        ));
        if problems.len() >= 10 {
            break;
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(ToolError::CheckFailed {
            check: "foreign_key_check",
            detail: problems.join("; "),
        })
    }
}

fn check_integrity(conn: &Connection, db_path: &Path) -> Result<(), ToolError> {
    let size = database_size(conn, db_path)?;
    let (pragma, check) = if size > QUICK_CHECK_BYTES {
        ("PRAGMA quick_check", "quick_check")
    } else {
        ("PRAGMA integrity_check", "integrity_check")
    };
    let mut statement = conn.prepare(pragma)?;
    let mut rows = statement.query([])?;
    let mut problems = Vec::new();
    while let Some(row) = rows.next()? {
        let text: String = row.get(0)?;
        if !text.eq_ignore_ascii_case("ok") {
            problems.push(text);
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(ToolError::CheckFailed {
            check,
            detail: problems.join("; "),
        })
    }
}

fn database_size(conn: &Connection, db_path: &Path) -> Result<i64, ToolError> {
    if let Ok(metadata) = std::fs::metadata(db_path) {
        let length = metadata.len();
        if let Ok(length) = i64::try_from(length) {
            return Ok(length);
        }
    }
    let pages: i64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    Ok(pages.saturating_mul(page_size))
}

fn check_parent_cycles(conn: &Connection) -> Result<(), ToolError> {
    // Start from every parent edge, then follow parents with UNION so a cycle
    // stops as soon as a (start, current) pair repeats. A row that comes back
    // to its start is a cycle.
    let mut statement = conn.prepare(
        "WITH RECURSIVE reach(start_id, current_id) AS (
             SELECT id, parent_id
               FROM notes
              WHERE deleted_at_utc_ms IS NULL AND parent_id IS NOT NULL
             UNION
             SELECT reach.start_id, notes.parent_id
               FROM reach
               JOIN notes ON notes.id = reach.current_id
              WHERE notes.deleted_at_utc_ms IS NULL AND notes.parent_id IS NOT NULL
         )
         SELECT DISTINCT start_id FROM reach
          WHERE start_id = current_id
          ORDER BY start_id
          LIMIT 10",
    )?;
    let mut rows = statement.query([])?;
    let mut problems = Vec::new();
    while let Some(row) = rows.next()? {
        problems.push(row.get::<_, String>(0)?);
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(ToolError::CheckFailed {
            check: "parent_cycles",
            detail: format!("notes form parent cycles: {}", problems.join(", ")),
        })
    }
}

fn check_edge_endpoints(conn: &Connection) -> Result<(), ToolError> {
    let mut statement = conn.prepare(
        "SELECT edges.id
           FROM edges
           LEFT JOIN notes AS from_note
             ON from_note.id = edges.\"from\" AND from_note.deleted_at_utc_ms IS NULL
           LEFT JOIN notes AS to_note
             ON to_note.id = edges.\"to\" AND to_note.deleted_at_utc_ms IS NULL
          WHERE edges.deleted_at_utc_ms IS NULL
            AND (from_note.id IS NULL OR to_note.id IS NULL)
          ORDER BY edges.id
          LIMIT 10",
    )?;
    let mut rows = statement.query([])?;
    let mut problems = Vec::new();
    while let Some(row) = rows.next()? {
        problems.push(row.get::<_, String>(0)?);
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(ToolError::CheckFailed {
            check: "edge_endpoints",
            detail: format!(
                "active edges have missing or deleted endpoints: {}",
                problems.join(", ")
            ),
        })
    }
}

const CHECK_NAMES: &[&str] = &[
    "foreign_key_check",
    "integrity_check",
    "parent_cycles",
    "edge_endpoints",
    "time_zones",
];

fn check_time_zones(conn: &Connection) -> Result<(), ToolError> {
    let mut statement = conn.prepare(
        "SELECT id, due_tz, remind_tz, snooze_tz
           FROM notes
          WHERE deleted_at_utc_ms IS NULL
            AND (due_tz IS NOT NULL OR remind_tz IS NOT NULL OR snooze_tz IS NOT NULL)
          ORDER BY id",
    )?;
    let mut rows = statement.query([])?;
    let mut problems = Vec::new();
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        for (index, field) in [(1, "due_tz"), (2, "remind_tz"), (3, "snooze_tz")] {
            if let Some(tz) = row.get::<_, Option<String>>(index)?
                && let Err(error) = validate_zone(&tz)
            {
                problems.push(format!("{id}: {field}={tz:?}: {error}"));
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(ToolError::CheckFailed {
            check: "time_zones",
            detail: problems.join("; "),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::store::scratch_dir;

    fn board(name: &str) -> StorePaths {
        let paths = StorePaths::from_home(scratch_dir(name));
        schema::open_board(&paths).expect("board opens");
        paths
    }

    fn open(paths: &StorePaths) -> Store {
        schema::open_board(paths).expect("board opens")
    }

    fn add_note(store: &mut Store, text: &str) -> String {
        let applied = store
            .apply(
                &[Command::AddNote {
                    text: text.to_owned(),
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
        applied.new_ids.notes[0].clone()
    }

    fn counts(store: &Store) -> (i64, i64, i64) {
        let batches = store
            .connection()
            .query_row("SELECT COUNT(*) FROM batches", [], |row| row.get(0))
            .expect("batches");
        let events = store
            .connection()
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .expect("events");
        let snapshots = store
            .connection()
            .query_row("SELECT COUNT(*) FROM snapshots", [], |row| row.get(0))
            .expect("snapshots");
        (batches, events, snapshots)
    }

    fn note_text(store: &Store, id: &str) -> String {
        store
            .connection()
            .query_row("SELECT text FROM notes WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .expect("note text")
    }

    fn parent_of(store: &Store, id: &str) -> Option<String> {
        store
            .connection()
            .query_row("SELECT parent_id FROM notes WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .expect("parent")
    }

    #[test]
    fn schema_marks_the_five_audit_tables_read_only() {
        let paths = board("mcp-schema");
        let schema = board_schema(&paths).expect("schema");
        let by_name: HashMap<&str, &TableSchema> = schema
            .tables
            .iter()
            .map(|table| (table.table.as_str(), table))
            .collect();
        for audit in AUDIT_TABLES {
            let table = by_name
                .get(audit)
                .unwrap_or_else(|| panic!("{audit} missing"));
            assert_eq!(table.managed, MANAGED_READ_ONLY, "{audit}");
        }
        for board in ["notes", "groups", "edges"] {
            assert_eq!(by_name[board].managed, MANAGED_READ_WRITE, "{board}");
        }
        assert!(
            by_name["notes"]
                .columns
                .iter()
                .any(|column| column == "text")
        );
        assert!(
            by_name["events"]
                .columns
                .iter()
                .any(|column| column == "payload_json")
        );
    }

    #[test]
    fn query_is_capped_and_cannot_write() {
        let paths = board("mcp-query");
        let result = board_query(
            &paths,
            BoardQueryArgs {
                sql: "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 600)
                      SELECT x FROM c"
                    .to_owned(),
                limit: Some(7),
            },
        )
        .expect("query");
        assert_eq!(result.rows.len(), 7);
        assert!(result.truncated);

        let result = board_query(
            &paths,
            BoardQueryArgs {
                sql: "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 600)
                      SELECT x FROM c"
                    .to_owned(),
                limit: None,
            },
        )
        .expect("query");
        assert_eq!(result.rows.len(), MAX_QUERY_ROWS as usize);
        assert!(result.truncated);

        let store = open(&paths);
        let before = counts(&store);
        for statement in [
            "INSERT INTO notes (id, text, created_at_utc_ms, updated_at_utc_ms) VALUES ('x','y',1,1)",
            "UPDATE notes SET text = 'x'",
            "DELETE FROM notes",
            "PRAGMA foreign_keys = OFF",
        ] {
            let error = board_query(
                &paths,
                BoardQueryArgs {
                    sql: statement.to_owned(),
                    limit: None,
                },
            )
            .expect_err(statement);
            assert!(
                matches!(error, ToolError::Query(_)),
                "{statement}: {error:?}"
            );
        }
        assert_eq!(counts(&store), before);
    }

    #[test]
    fn apply_writes_one_agent_batch_and_returns_effects() {
        let paths = board("mcp-apply");
        let applied = board_apply(
            &paths,
            BoardApplyArgs {
                commands: vec![Command::AddNote {
                    text: "from mcp".to_owned(),
                    group: None,
                    parent: None,
                    due: None,
                    remind: None,
                    priority: None,
                    source: "mcp-test".to_owned(),
                }],
                description: Some("mcp apply".to_owned()),
                actor_ref: Some("turn-1".to_owned()),
            },
        )
        .expect("apply");
        assert_eq!(applied.effects.len(), 1);
        assert_eq!(applied.effects[0].command, "add_note");
        assert_eq!(applied.new_ids.notes.len(), 1);

        let store = open(&paths);
        assert_eq!(counts(&store), (1, 1, 0));
        let (actor, actor_ref, label, kind): (String, Option<String>, Option<String>, String) =
            store
                .connection()
                .query_row(
                    "SELECT actor, actor_ref, label, kind FROM batches WHERE id = ?1",
                    [&applied.batch_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .expect("batch");
        assert_eq!(actor, "agent");
        assert_eq!(actor_ref.as_deref(), Some("turn-1"));
        assert_eq!(label.as_deref(), Some("mcp apply"));
        assert_eq!(kind, "apply");
        assert_eq!(note_text(&store, &applied.new_ids.notes[0]), "from mcp");
    }

    #[test]
    fn sql_update_commits_a_batch_event_snapshot_and_undo_restores_it() {
        let paths = board("mcp-sql-ok");
        let mut store = open(&paths);
        let id = add_note(&mut store, "kept");
        let before = counts(&store);

        let result = board_sql(
            &paths,
            BoardSqlArgs {
                sql: "UPDATE notes SET text = 'changed' WHERE text = 'kept'".to_owned(),
                description: "rename through sql".to_owned(),
            },
        )
        .expect("board_sql");
        assert_eq!(result.changes, 1);
        assert_eq!(result.rows.len(), 0);
        assert_eq!(
            result.checks,
            CHECK_NAMES
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>()
        );
        assert_eq!(note_text(&store, &id), "changed");
        let after = counts(&store);
        assert_eq!(after, (before.0 + 1, before.1 + 1, before.2 + 1));

        let (actor, label, kind): (String, Option<String>, String) = store
            .connection()
            .query_row(
                "SELECT actor, label, kind FROM batches WHERE id = ?1",
                [&result.batch_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("batch");
        assert_eq!(actor, "agent");
        assert_eq!(label.as_deref(), Some("rename through sql"));
        assert_eq!(kind, "apply");

        let (command, entity, entity_id, payload, before_json, after_json): (
            String,
            String,
            String,
            String,
            String,
            String,
        ) = store
            .connection()
            .query_row(
                "SELECT command, entity, entity_id, payload_json, before_json, after_json
                   FROM events WHERE batch_id = ?1",
                [&result.batch_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .expect("event");
        assert_eq!(command, "board_sql");
        assert_eq!(entity, "board");
        assert_eq!(entity_id, "board");
        let payload: Value = serde_json::from_str(&payload).expect("payload json");
        assert_eq!(payload["command"], "board_sql");
        assert_eq!(
            payload["sql"],
            "UPDATE notes SET text = 'changed' WHERE text = 'kept'"
        );

        let (snapshot_batch, reason, data): (Option<String>, String, String) = store
            .connection()
            .query_row(
                "SELECT batch_id, reason, data FROM snapshots WHERE batch_id = ?1",
                [&result.batch_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("snapshot");
        assert_eq!(snapshot_batch.as_deref(), Some(result.batch_id.as_str()));
        assert_eq!(reason, "rename through sql");
        assert_eq!(data, before_json);
        assert!(!data.contains("changed"));
        assert!(after_json.contains("changed"));

        let outcome = history::undo_last(&mut store).expect("undo board_sql");
        assert_eq!(outcome.targets, vec![result.batch_id]);
        assert_eq!(note_text(&store, &id), "kept");
    }

    #[test]
    fn audit_ddl_pragma_attach_and_transaction_forms_are_denied() {
        let paths = board("mcp-sql-deny");
        let mut store = open(&paths);
        add_note(&mut store, "kept");
        let before = counts(&store);
        let statements = [
            "DELETE FROM events",
            "UPDATE batches SET label = 'x'",
            "INSERT INTO events (batch_id, seq, at_utc_ms, actor, command, entity, entity_id, payload_json)
             VALUES ('b', 0, 0, 'agent', 'x', 'note', 'n', '{}')",
            "DELETE FROM undo_targets",
            "UPDATE snapshots SET reason = 'x'",
            "DROP TABLE events",
            "ALTER TABLE notes ADD COLUMN x TEXT",
            "CREATE TABLE x (id INTEGER)",
            "PRAGMA foreign_keys = OFF",
            "ATTACH DATABASE 'other.db' AS other",
            "BEGIN",
            "COMMIT",
            "VACUUM",
            "SELECT load_extension('evil')",
        ];
        for statement in statements {
            let error = board_sql(
                &paths,
                BoardSqlArgs {
                    sql: statement.to_owned(),
                    description: "must roll back".to_owned(),
                },
            )
            .expect_err(statement);
            assert!(
                matches!(error, ToolError::Sql(_)),
                "{statement}: expected a SQL/authorization error, got {error:?}"
            );
            assert_eq!(counts(&store), before, "{statement} changed history");
            assert_eq!(note_text(&store, &add_note_id(&store)), "kept");
        }
    }

    fn add_note_id(store: &Store) -> String {
        store
            .connection()
            .query_row("SELECT id FROM notes LIMIT 1", [], |row| row.get(0))
            .expect("note id")
    }

    #[test]
    fn a_parent_cycle_rolls_the_whole_statement_back() {
        let paths = board("mcp-sql-cycle");
        let mut store = open(&paths);
        let a = add_note(&mut store, "a");
        let b = add_note(&mut store, "b");
        let before = counts(&store);

        let error = board_sql(
            &paths,
            BoardSqlArgs {
                sql: format!(
                    "UPDATE notes SET parent_id = CASE id WHEN '{a}' THEN '{b}' WHEN '{b}' THEN '{a}' END WHERE id IN ('{a}','{b}')"
                ),
                description: "make a cycle".to_owned(),
            },
        )
        .expect_err("cycle must roll back");
        match error {
            ToolError::CheckFailed { check, .. } => assert_eq!(check, "parent_cycles"),
            other => panic!("expected a parent_cycles failure, got {other:?}"),
        }
        assert_eq!(parent_of(&store, &a), None);
        assert_eq!(parent_of(&store, &b), None);
        assert_eq!(counts(&store), before);
    }

    #[test]
    fn deleted_edge_endpoints_and_bad_zones_roll_back() {
        let paths = board("mcp-sql-invariants");
        let mut store = open(&paths);
        let a = add_note(&mut store, "a");
        let b = add_note(&mut store, "b");
        store
            .apply(
                &[Command::Link {
                    from: a.clone(),
                    to: b.clone(),
                    kind: None,
                }],
                Actor::Human,
                None,
            )
            .expect("link");
        let before = counts(&store);

        let error = board_sql(
            &paths,
            BoardSqlArgs {
                sql: format!("UPDATE notes SET deleted_at_utc_ms = 123 WHERE id = '{a}'"),
                description: "soft delete one endpoint".to_owned(),
            },
        )
        .expect_err("edge endpoint must roll back");
        match error {
            ToolError::CheckFailed { check, .. } => assert_eq!(check, "edge_endpoints"),
            other => panic!("expected an edge_endpoints failure, got {other:?}"),
        }
        assert_eq!(counts(&store), before);

        let error = board_sql(
            &paths,
            BoardSqlArgs {
                sql: format!(
                    "UPDATE notes SET due_local = '2026-01-01T00:00:00', due_tz = 'Nowhere/Nope', due_utc_ms = 0 WHERE id = '{b}'"
                ),
                description: "bad zone".to_owned(),
            },
        )
        .expect_err("bad zone must roll back");
        match error {
            ToolError::CheckFailed { check, .. } => assert_eq!(check, "time_zones"),
            other => panic!("expected a time_zones failure, got {other:?}"),
        }
        assert_eq!(counts(&store), before);
    }

    #[test]
    fn history_reads_note_events_and_batch_ranges() {
        let paths = board("mcp-history");
        let applied = board_apply(
            &paths,
            BoardApplyArgs {
                commands: vec![Command::AddNote {
                    text: "history".to_owned(),
                    group: None,
                    parent: None,
                    due: None,
                    remind: None,
                    priority: None,
                    source: "mcp-test".to_owned(),
                }],
                description: Some("add through mcp".to_owned()),
                actor_ref: Some("turn-2".to_owned()),
            },
        )
        .expect("apply");
        let note_id = applied.new_ids.notes[0].clone();

        let by_note = board_history(
            &paths,
            BoardHistoryArgs {
                note_id: Some(note_id.clone()),
                ..BoardHistoryArgs::default()
            },
        )
        .expect("note history");
        assert_eq!(by_note.note_id.as_deref(), Some(note_id.as_str()));
        assert_eq!(by_note.events.len(), 1);
        assert_eq!(by_note.events[0].command, "add_note");
        assert_eq!(by_note.events[0].batch_id, applied.batch_id);

        let by_range = board_history(
            &paths,
            BoardHistoryArgs {
                note_id: None,
                from_utc_ms: Some(0),
                to_utc_ms: Some(crate::store::now_utc_ms() + 1),
            },
        )
        .expect("range history");
        assert_eq!(by_range.batches.len(), 1);
        assert_eq!(by_range.batches[0].id, applied.batch_id);
        assert_eq!(by_range.batches[0].actor, Actor::Agent);
        assert_eq!(
            by_range.batches[0].label.as_deref(),
            Some("add through mcp")
        );
    }

    #[test]
    fn a_missing_database_is_a_tool_error_not_a_panic() {
        let paths = StorePaths::from_home(scratch_dir("mcp-missing"));
        let error = board_schema(&paths).expect_err("missing board");
        assert!(matches!(error, ToolError::DatabaseMissing { .. }));
        let error = board_sql(
            &paths,
            BoardSqlArgs {
                sql: "SELECT 1".to_owned(),
                description: "x".to_owned(),
            },
        )
        .expect_err("missing board");
        assert!(matches!(error, ToolError::DatabaseMissing { .. }));
    }
}

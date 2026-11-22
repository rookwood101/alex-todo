//! Read models for the SQLite board (task 040).
//!
//! These are the types the UI, the CLI, the MCP server and the agent contract
//! share. They are plain data: no egui types, no painter calls, no behaviour
//! tied to a window. Every board table is read with the soft-delete filter
//! applied; the command layer loads individual rows (including deleted ones)
//! through the same helpers so before/after images always agree with the read
//! model.

use rusqlite::{Connection, Row, params};
use serde::{Deserialize, Serialize};

use crate::time::TimeSpec;

/// Note identifier (a UUID string minted by the command layer).
pub type NoteId = String;
/// Group identifier.
pub type GroupId = String;
/// Edge identifier.
pub type EdgeId = String;
/// Batch identifier.
pub type BatchId = String;

/// A note is open or done. Done notes are omitted from the canvas and never
/// fire reminders.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Open,
    Done,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Done => "done",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "open" => Some(Self::Open),
            "done" => Some(Self::Done),
            _ => None,
        }
    }
}

/// Priority stored as low / med / high, matching the schema CHECK.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Priority {
    #[serde(rename = "low")]
    Low,
    #[serde(rename = "med")]
    Medium,
    #[serde(rename = "high")]
    High,
}

impl Priority {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "med",
            Self::High => "high",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "low" => Some(Self::Low),
            "med" | "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }
}

impl From<crate::capture::Priority> for Priority {
    fn from(value: crate::capture::Priority) -> Self {
        match value {
            crate::capture::Priority::Low => Self::Low,
            crate::capture::Priority::Medium => Self::Medium,
            crate::capture::Priority::High => Self::High,
        }
    }
}

/// How the layout engine treats a note.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LayoutMode {
    Flow,
    Pinned,
}

impl LayoutMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Flow => "flow",
            Self::Pinned => "pinned",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "flow" => Some(Self::Flow),
            "pinned" => Some(Self::Pinned),
            _ => None,
        }
    }
}

/// A world-space position for a pinned root note.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

/// One note, active or soft-deleted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub id: NoteId,
    pub text: String,
    pub group: Option<GroupId>,
    pub parent: Option<NoteId>,
    pub layout: LayoutMode,
    pub pos: Option<Point>,
    pub sort: i64,
    pub color: i64,
    pub priority: Option<Priority>,
    pub status: Status,
    pub source: String,
    pub due: Option<TimeSpec>,
    pub remind: Option<TimeSpec>,
    pub reminded_at_utc_ms: Option<i64>,
    pub snooze: Option<TimeSpec>,
    pub completed_at_utc_ms: Option<i64>,
    pub created_at_utc_ms: i64,
    pub updated_at_utc_ms: i64,
    pub deleted_at_utc_ms: Option<i64>,
    pub version: i64,
}

impl Note {
    pub fn is_active(&self) -> bool {
        self.deleted_at_utc_ms.is_none()
    }
}

/// A root column on the board.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub id: GroupId,
    pub name: String,
    pub color: i64,
    pub sort_order: i64,
    pub created_at_utc_ms: i64,
    pub updated_at_utc_ms: i64,
    pub deleted_at_utc_ms: Option<i64>,
    pub version: i64,
}

/// A directed arrow between two notes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub id: EdgeId,
    pub from: NoteId,
    pub to: NoteId,
    pub kind: String,
    pub created_at_utc_ms: i64,
    pub updated_at_utc_ms: i64,
    pub deleted_at_utc_ms: Option<i64>,
    pub version: i64,
}

/// Every active note, group and edge: one consistent snapshot for rendering.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Board {
    pub notes: Vec<Note>,
    pub groups: Vec<Group>,
    pub edges: Vec<Edge>,
}

impl Board {
    /// Load the active board, or an empty board for a fresh database.
    pub fn load(conn: &Connection) -> Result<Self, ModelError> {
        Ok(Self {
            notes: load_notes(conn)?,
            groups: load_groups(conn)?,
            edges: load_edges(conn)?,
        })
    }

    pub fn note(&self, id: &str) -> Option<&Note> {
        self.notes.iter().find(|note| note.id == id)
    }

    pub fn group(&self, id: &str) -> Option<&Group> {
        self.groups.iter().find(|group| group.id == id)
    }

    pub fn edge(&self, id: &str) -> Option<&Edge> {
        self.edges.iter().find(|edge| edge.id == id)
    }

    /// Direct children of a note (or roots when parent is None), in sort order.
    pub fn children(&self, parent: Option<&str>) -> Vec<&Note> {
        let mut children: Vec<&Note> = self
            .notes
            .iter()
            .filter(|note| note.parent.as_deref() == parent)
            .collect();
        children.sort_by(|left, right| {
            left.sort
                .cmp(&right.sort)
                .then_with(|| left.id.cmp(&right.id))
        });
        children
    }
}

/// Errors from reading the board.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("malformed {entity} row {id:?}: {detail}")]
    Malformed {
        entity: &'static str,
        id: String,
        detail: String,
    },
}

const NOTE_COLUMNS: &str = "id, text, group_id, parent_id, layout_mode, pos_x, pos_y,
     sort_order, color, priority, status, source,
     created_at_utc_ms, updated_at_utc_ms, deleted_at_utc_ms, version,
     due_local, due_tz, due_utc_ms,
     remind_local, remind_tz, remind_utc_ms, reminded_at_utc_ms,
     snooze_local, snooze_tz, snooze_utc_ms, completed_at_utc_ms";

const GROUP_COLUMNS: &str =
    "id, name, color, sort_order, created_at_utc_ms, updated_at_utc_ms, deleted_at_utc_ms, version";

const EDGE_COLUMNS: &str = "id, \"from\", \"to\", kind,
     created_at_utc_ms, updated_at_utc_ms, deleted_at_utc_ms, version";

/// Every active note, ordered for a stable layout.
pub fn load_notes(conn: &Connection) -> Result<Vec<Note>, ModelError> {
    let sql = format!(
        "SELECT {NOTE_COLUMNS} FROM notes WHERE deleted_at_utc_ms IS NULL ORDER BY sort_order, id"
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map([], RawNote::read)?;
    let mut notes = Vec::new();
    for row in rows {
        notes.push(row?.into_note()?);
    }
    Ok(notes)
}

/// One note by id, including soft-deleted rows.
pub fn load_note(conn: &Connection, id: &str) -> Result<Option<Note>, ModelError> {
    let sql = format!("SELECT {NOTE_COLUMNS} FROM notes WHERE id = ?1");
    let mut statement = conn.prepare(&sql)?;
    let mut rows = statement.query_map([id], RawNote::read)?;
    match rows.next() {
        Some(row) => Ok(Some(row?.into_note()?)),
        None => Ok(None),
    }
}

/// Every active group, ordered by sort_order then id.
pub fn load_groups(conn: &Connection) -> Result<Vec<Group>, ModelError> {
    let sql = format!(
        "SELECT {GROUP_COLUMNS} FROM groups WHERE deleted_at_utc_ms IS NULL ORDER BY sort_order, id"
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map([], RawGroup::read)?;
    let mut groups = Vec::new();
    for row in rows {
        groups.push(row?.into_group()?);
    }
    Ok(groups)
}

/// One group by id, including soft-deleted rows.
pub fn load_group(conn: &Connection, id: &str) -> Result<Option<Group>, ModelError> {
    let sql = format!("SELECT {GROUP_COLUMNS} FROM groups WHERE id = ?1");
    let mut statement = conn.prepare(&sql)?;
    let mut rows = statement.query_map([id], RawGroup::read)?;
    match rows.next() {
        Some(row) => Ok(Some(row?.into_group()?)),
        None => Ok(None),
    }
}

/// Every active edge.
pub fn load_edges(conn: &Connection) -> Result<Vec<Edge>, ModelError> {
    let sql =
        format!("SELECT {EDGE_COLUMNS} FROM edges WHERE deleted_at_utc_ms IS NULL ORDER BY id");
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map([], RawEdge::read)?;
    let mut edges = Vec::new();
    for row in rows {
        edges.push(row?.into_edge()?);
    }
    Ok(edges)
}

/// One edge by id, including soft-deleted rows.
pub fn load_edge(conn: &Connection, id: &str) -> Result<Option<Edge>, ModelError> {
    let sql = format!("SELECT {EDGE_COLUMNS} FROM edges WHERE id = ?1");
    let mut statement = conn.prepare(&sql)?;
    let mut rows = statement.query_map([id], RawEdge::read)?;
    match rows.next() {
        Some(row) => Ok(Some(row?.into_edge()?)),
        None => Ok(None),
    }
}

/// The id of the first active group whose name matches case-insensitively.
pub fn find_group_by_name(conn: &Connection, name: &str) -> Result<Option<GroupId>, ModelError> {
    let mut statement = conn.prepare(
        "SELECT id FROM groups
          WHERE deleted_at_utc_ms IS NULL AND lower(name) = lower(?1)
          ORDER BY id LIMIT 1",
    )?;
    let mut rows = statement.query_map([name], |row| row.get::<_, String>(0))?;
    match rows.next() {
        Some(id) => Ok(Some(id?)),
        None => Ok(None),
    }
}

struct RawNote {
    id: String,
    text: String,
    group_id: Option<String>,
    parent_id: Option<String>,
    layout_mode: String,
    pos_x: Option<f64>,
    pos_y: Option<f64>,
    sort_order: i64,
    color: i64,
    priority: Option<String>,
    status: String,
    source: String,
    created_at_utc_ms: i64,
    updated_at_utc_ms: i64,
    deleted_at_utc_ms: Option<i64>,
    version: i64,
    due_local: Option<String>,
    due_tz: Option<String>,
    due_utc_ms: Option<i64>,
    remind_local: Option<String>,
    remind_tz: Option<String>,
    remind_utc_ms: Option<i64>,
    reminded_at_utc_ms: Option<i64>,
    snooze_local: Option<String>,
    snooze_tz: Option<String>,
    snooze_utc_ms: Option<i64>,
    completed_at_utc_ms: Option<i64>,
}

impl RawNote {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            text: row.get(1)?,
            group_id: row.get(2)?,
            parent_id: row.get(3)?,
            layout_mode: row.get(4)?,
            pos_x: row.get(5)?,
            pos_y: row.get(6)?,
            sort_order: row.get(7)?,
            color: row.get(8)?,
            priority: row.get(9)?,
            status: row.get(10)?,
            source: row.get(11)?,
            created_at_utc_ms: row.get(12)?,
            updated_at_utc_ms: row.get(13)?,
            deleted_at_utc_ms: row.get(14)?,
            version: row.get(15)?,
            due_local: row.get(16)?,
            due_tz: row.get(17)?,
            due_utc_ms: row.get(18)?,
            remind_local: row.get(19)?,
            remind_tz: row.get(20)?,
            remind_utc_ms: row.get(21)?,
            reminded_at_utc_ms: row.get(22)?,
            snooze_local: row.get(23)?,
            snooze_tz: row.get(24)?,
            snooze_utc_ms: row.get(25)?,
            completed_at_utc_ms: row.get(26)?,
        })
    }

    fn into_note(self) -> Result<Note, ModelError> {
        let bad = |detail: String| ModelError::Malformed {
            entity: "note",
            id: self.id.clone(),
            detail,
        };

        let layout = LayoutMode::parse(&self.layout_mode)
            .ok_or_else(|| bad(format!("unknown layout_mode {:?}", self.layout_mode)))?;
        let status = Status::parse(&self.status)
            .ok_or_else(|| bad(format!("unknown status {:?}", self.status)))?;
        let priority = match self.priority.as_deref() {
            Some(value) => Some(
                Priority::parse(value).ok_or_else(|| bad(format!("unknown priority {value:?}")))?,
            ),
            None => None,
        };
        let pos = match (layout, self.pos_x, self.pos_y) {
            (LayoutMode::Pinned, Some(x), Some(y)) => Some(Point {
                x: x as f32,
                y: y as f32,
            }),
            (LayoutMode::Pinned, _, _) => {
                return Err(bad("pinned note has no position".to_owned()));
            }
            _ => None,
        };
        let due = time_from_columns(
            "due",
            self.due_local,
            self.due_tz,
            self.due_utc_ms,
            &self.id,
        )?;
        let remind = time_from_columns(
            "remind",
            self.remind_local,
            self.remind_tz,
            self.remind_utc_ms,
            &self.id,
        )?;
        let snooze = time_from_columns(
            "snooze",
            self.snooze_local,
            self.snooze_tz,
            self.snooze_utc_ms,
            &self.id,
        )?;

        Ok(Note {
            id: self.id,
            text: self.text,
            group: self.group_id,
            parent: self.parent_id,
            layout,
            pos,
            sort: self.sort_order,
            color: self.color,
            priority,
            status,
            source: self.source,
            due,
            remind,
            reminded_at_utc_ms: self.reminded_at_utc_ms,
            snooze,
            completed_at_utc_ms: self.completed_at_utc_ms,
            created_at_utc_ms: self.created_at_utc_ms,
            updated_at_utc_ms: self.updated_at_utc_ms,
            deleted_at_utc_ms: self.deleted_at_utc_ms,
            version: self.version,
        })
    }
}

/// A civil triple must be all present or all NULL; the schema enforces the
/// same invariant, so a partial triple means the row was written around it.
fn time_from_columns(
    prefix: &str,
    local: Option<String>,
    tz: Option<String>,
    utc_ms: Option<i64>,
    note_id: &str,
) -> Result<Option<TimeSpec>, ModelError> {
    match (local, tz, utc_ms) {
        (None, None, None) => Ok(None),
        (Some(local), Some(tz), Some(_)) => Ok(Some(TimeSpec { local, tz })),
        _ => Err(ModelError::Malformed {
            entity: "note",
            id: note_id.to_owned(),
            detail: format!("{prefix} has a partial civil triple"),
        }),
    }
}

struct RawGroup {
    id: String,
    name: String,
    color: i64,
    sort_order: i64,
    created_at_utc_ms: i64,
    updated_at_utc_ms: i64,
    deleted_at_utc_ms: Option<i64>,
    version: i64,
}

impl RawGroup {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            name: row.get(1)?,
            color: row.get(2)?,
            sort_order: row.get(3)?,
            created_at_utc_ms: row.get(4)?,
            updated_at_utc_ms: row.get(5)?,
            deleted_at_utc_ms: row.get(6)?,
            version: row.get(7)?,
        })
    }

    fn into_group(self) -> Result<Group, ModelError> {
        if self.name.trim().is_empty() {
            return Err(ModelError::Malformed {
                entity: "group",
                id: self.id,
                detail: "empty name".to_owned(),
            });
        }
        Ok(Group {
            id: self.id,
            name: self.name,
            color: self.color,
            sort_order: self.sort_order,
            created_at_utc_ms: self.created_at_utc_ms,
            updated_at_utc_ms: self.updated_at_utc_ms,
            deleted_at_utc_ms: self.deleted_at_utc_ms,
            version: self.version,
        })
    }
}

struct RawEdge {
    id: String,
    from: String,
    to: String,
    kind: String,
    created_at_utc_ms: i64,
    updated_at_utc_ms: i64,
    deleted_at_utc_ms: Option<i64>,
    version: i64,
}

impl RawEdge {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            from: row.get(1)?,
            to: row.get(2)?,
            kind: row.get(3)?,
            created_at_utc_ms: row.get(4)?,
            updated_at_utc_ms: row.get(5)?,
            deleted_at_utc_ms: row.get(6)?,
            version: row.get(7)?,
        })
    }

    fn into_edge(self) -> Result<Edge, ModelError> {
        if self.from == self.to {
            return Err(ModelError::Malformed {
                entity: "edge",
                id: self.id,
                detail: "edge connects a note to itself".to_owned(),
            });
        }
        Ok(Edge {
            id: self.id,
            from: self.from,
            to: self.to,
            kind: self.kind,
            created_at_utc_ms: self.created_at_utc_ms,
            updated_at_utc_ms: self.updated_at_utc_ms,
            deleted_at_utc_ms: self.deleted_at_utc_ms,
            version: self.version,
        })
    }
}

/// A helper for counting active notes; used by the CLI and tests.
pub fn count_active_notes(conn: &Connection) -> Result<i64, ModelError> {
    let count = conn.query_row(
        "SELECT COUNT(*) FROM notes WHERE deleted_at_utc_ms IS NULL",
        [],
        |row| row.get(0),
    )?;
    Ok(count)
}

/// Loads a batch id for tests and history helpers.
pub fn batch_exists(conn: &Connection, id: &str) -> Result<bool, ModelError> {
    let found: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM batches WHERE id = ?1)",
        params![id],
        |row| row.get(0),
    )?;
    Ok(found != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Store, StorePaths, schema, scratch_dir};

    fn board(name: &str) -> (Store, Board) {
        let paths = StorePaths::from_home(scratch_dir(name));
        let store = schema::open_board(&paths).expect("board opens");
        let board = Board::load(store.connection()).expect("board loads");
        (store, board)
    }

    #[test]
    fn a_fresh_board_is_empty() {
        let (_store, board) = board("model-fresh");
        assert!(board.notes.is_empty());
        assert!(board.groups.is_empty());
        assert!(board.edges.is_empty());
        assert_eq!(count_active_notes(_store.connection()).expect("count"), 0);
    }

    #[test]
    fn malformed_rows_are_reported_not_panicked() {
        let (store, _board) = board("model-malformed");
        store
            .connection()
            .execute(
                "INSERT INTO notes (id, text, layout_mode, status, source,
                                    created_at_utc_ms, updated_at_utc_ms)
                 VALUES ('bad', 'x', 'sideways', 'open', 'human', 1, 1)",
                [],
            )
            .expect_err("CHECK rejects an unknown layout");
        // Bypass the CHECK by reading a row through a view is not possible;
        // instead prove the parser rejects an unknown status value directly.
        let raw = RawNote {
            id: "bad".to_owned(),
            text: "x".to_owned(),
            group_id: None,
            parent_id: None,
            layout_mode: "flow".to_owned(),
            pos_x: None,
            pos_y: None,
            sort_order: 0,
            color: 0,
            priority: Some("urgent".to_owned()),
            status: "open".to_owned(),
            source: "human".to_owned(),
            created_at_utc_ms: 1,
            updated_at_utc_ms: 1,
            deleted_at_utc_ms: None,
            version: 1,
            due_local: None,
            due_tz: None,
            due_utc_ms: None,
            remind_local: None,
            remind_tz: None,
            remind_utc_ms: None,
            reminded_at_utc_ms: None,
            snooze_local: None,
            snooze_tz: None,
            snooze_utc_ms: None,
            completed_at_utc_ms: None,
        };
        let error = raw.into_note().expect_err("unknown priority is malformed");
        assert!(matches!(
            error,
            ModelError::Malformed { entity: "note", .. }
        ));
    }

    #[test]
    fn children_are_ordered_by_sort_then_id() {
        let (store, _board) = board("model-children");
        for (id, sort) in [("b", 2), ("a", 1), ("c", 1)] {
            store
                .connection()
                .execute(
                    "INSERT INTO notes (id, text, sort_order, created_at_utc_ms, updated_at_utc_ms)
                     VALUES (?1, 'x', ?2, 1, 1)",
                    params![id, sort],
                )
                .expect("insert");
        }
        let board = Board::load(store.connection()).expect("load");
        let ids: Vec<&str> = board
            .children(None)
            .iter()
            .map(|note| note.id.as_str())
            .collect();
        assert_eq!(ids, vec!["a", "c", "b"]);
    }
}

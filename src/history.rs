//! Batches, events and undo (task 050).
//!
//! Rustboard never rewrites its history. The command layer (task 040) appends
//! one apply batch per [crate::command::Store::apply], one event per changed
//! row, and the before/after image of that row as JSON. This module turns that
//! log into behaviour:
//!
//! * [undo_last], [redo_last] and [undo_batches] append a *new* undo or redo
//!   batch whose events reverse (or re-apply) the originals. The batches,
//!   undo_targets, events and snapshots tables are only ever inserted into: the
//!   schema triggers reject UPDATE and DELETE and no SQL in this module tries.
//! * [undone_apply_batches] derives the undo state instead of storing it: an
//!   apply batch is undone when an undo batch targets it and no redo batch
//!   targets that undo batch.
//! * [history] and [batches_between] expose the log to the UI, and
//!   [board_revision], [capture_board], [record_snapshot] and
//!   [restore_snapshot] are the snapshot API board_sql (task 110) needs.
//!
//! # Reversing an event
//!
//! The inverse of a row write is "write the other image": an undo writes
//! before_json over the row, a redo writes after_json. before_json = NULL
//! describes a row that did not exist (an insert), so its inverse is a soft
//! delete; after_json = NULL is handled symmetrically. Only user-visible
//! columns are written: version and updated_at_utc_ms are owned by the schema
//! triggers, and the new event records the images read back after the write.
//!
//! Events of one batch are reversed in seq order (newest first for undo, the
//! original order for redo), which is the order that makes each intermediate
//! row image match the next event. This matters when one batch changes the same
//! row twice (SetText followed by SetPriority) or interleaves rows. Each event
//! in the reversing batch copies the original command name and payload, so the
//! log reads as the same command in the other direction; the batch's kind and
//! label say which way it went.
//!
//! # Conflict detection
//!
//! Before the first write to a row inside one target batch, the row's current
//! image is compared with the image the batch left it in: after_json of the
//! last event for that row when undoing, before_json of the first event when
//! redoing. A mismatch is [HistoryError::Conflict] and the whole reversal rolls
//! back. The comparison ignores version and updated_at_utc_ms (triggers own
//! them, and every reversal bumps them) and compares every other column. That
//! is deliberately equivalent to, and in practice stricter than, the plan's
//! "version must still equal after_json.version" rule: any later batch that
//! touched the row changed one of the compared columns, so it is reported as a
//! conflict, while a sequence of undos (each of which bumps the version without
//! changing what this batch produced) keeps working. A row that no longer
//! exists is a conflict too - never a panic.
//!
//! actor = 'system' batches (reminder markers, task 140) are never chosen by
//! user undo. [undo_batches] also skips batches that are already undone,
//! because a Pi turn range may contain both.
//!
//! # Snapshots
//!
//! [capture_board] returns the active board as canonical JSON
//! ({"notes": [...], "groups": [...], "edges": [...]}, each list sorted by id,
//! full rows including version), and [board_revision] is the SHA-256 of exactly
//! that text. [restore_snapshot] verifies the caller's expected revision,
//! records the pre-restore image as a new snapshot, replaces the board tables
//! with the snapshot in FK-safe order and verifies the result hashes back to
//! the snapshot before committing; rows outside the snapshot are hard-deleted
//! (they are board rows, not audit rows) and the snapshot's ids, timestamps and
//! versions are written back verbatim.
//!
//! A restore writes no events: the snapshots table is its audit record. It
//! appends one redo-kind batch labelled "restore snapshot <id>" with an
//! undo_targets row pointing at the snapshot's batch when the snapshot has one,
//! and no batch at all when it does not (the schema allows a NULL
//! snapshots.batch_id for exactly this case: an automatic pre-restore image).
//!
//! # Raw SQL turns (board_sql, task 110)
//!
//! board_sql records one apply batch whose single event has entity "board" and
//! whole-board images in before_json/after_json, plus a snapshot whose batch_id
//! is that batch and whose data is the before image. Undoing such a batch
//! restores that snapshot (after checking the board still matches the event's
//! after image), and redoing it writes the event's after image; both write one
//! new "board" event into the reversing batch. A batch with a "board" event but
//! no linked snapshot falls back to the event images, so a hand-written board
//! event is still reversible. The batch is reversed as a whole: any other events
//! it carries are already covered by the board image and are not reversed
//! individually. Rows that do not appear in the image are hard-deleted, exactly
//! as in a snapshot restore.
//! Because the image only holds active rows, soft-deleted rows that are outside
//! it are not recoverable from it - the frozen history is, but a restore is the
//! board recovery tool, not the history one.

use std::collections::HashSet;

use rusqlite::{Connection, ToSql, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::command::Actor;
use crate::model::{self, BatchId, Edge, Group, Note};
use crate::store::{Store, now_utc_ms};
use crate::time::{TimeError, TimeSpec};

/// Identifier of a snapshots row (a UUID string minted by this module).
pub type SnapshotId = String;

/// The value of batches.kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BatchKind {
    /// One command-layer transaction.
    Apply,
    /// A batch that reversed one or more apply batches.
    Undo,
    /// A batch that re-applied one or more apply batches (or a snapshot
    /// restore).
    Redo,
}

impl BatchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Apply => "apply",
            Self::Undo => "undo",
            Self::Redo => "redo",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "apply" => Some(Self::Apply),
            "undo" => Some(Self::Undo),
            "redo" => Some(Self::Redo),
            _ => None,
        }
    }
}

/// Which way a batch is being walked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reversal {
    /// Write before_json, newest event first.
    Undo,
    /// Write after_json, oldest event first.
    Redo,
}

/// The result of a successful undo or redo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoOutcome {
    /// The reversing batch that was appended.
    pub batch_id: BatchId,
    /// Its kind: Undo for [undo_last]/[undo_batches], Redo for [redo_last].
    pub kind: BatchKind,
    /// The batches recorded in undo_targets, in application order. For an undo
    /// these are the reversed apply batches; for a redo it is the single undo
    /// batch that was re-applied.
    pub targets: Vec<BatchId>,
    /// Events written into the reversing batch (one per restored row).
    pub events: usize,
    /// Events skipped because their entity kind is unknown to this build.
    pub skipped: usize,
}

/// One event in a note's history, ready for the UI.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub event_id: i64,
    pub batch_id: BatchId,
    pub batch_kind: BatchKind,
    pub seq: i64,
    pub at_utc_ms: i64,
    pub actor: Actor,
    /// The command name stored with the event (set_text, ...).
    pub command: String,
    /// The command itself, as stored in events.payload_json.
    pub payload: Value,
    /// The row image before the write; None for an insert.
    pub before: Option<Value>,
    /// The row image after the write.
    pub after: Option<Value>,
}

/// One batch in a time range, for the chat UI (task 130).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BatchSummary {
    pub id: BatchId,
    pub kind: BatchKind,
    pub actor: Actor,
    pub actor_ref: Option<String>,
    pub label: Option<String>,
    pub created_at_utc_ms: i64,
    /// The undo_targets of this batch, ordered by target id.
    pub targets: Vec<BatchId>,
    pub events: i64,
}

/// The canonical board image used by [capture_board] and [restore_snapshot]:
/// the active notes, groups and edges, sorted by id.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BoardImage {
    pub notes: Vec<Note>,
    pub groups: Vec<Group>,
    pub edges: Vec<Edge>,
}

impl BoardImage {
    /// Load the active board and sort every list by id.
    fn capture(conn: &Connection) -> Result<Self, HistoryError> {
        let board = model::Board::load(conn)?;
        Ok(Self {
            notes: board.notes,
            groups: board.groups,
            edges: board.edges,
        }
        .sorted())
    }

    /// Sort every list by id, which is what makes the JSON canonical.
    fn sorted(mut self) -> Self {
        self.notes.sort_by_key(|note| note.id.clone());
        self.groups.sort_by_key(|group| group.id.clone());
        self.edges.sort_by_key(|edge| edge.id.clone());
        self
    }
}

/// Errors from reading or rewriting history.
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("there is nothing to undo")]
    NothingToUndo,

    #[error("there is nothing to redo")]
    NothingToRedo,

    #[error("batch {batch_id:?} is {kind}, not an apply batch")]
    NotReversible { batch_id: BatchId, kind: String },

    #[error("batch {batch_id:?} does not exist")]
    BatchNotFound { batch_id: BatchId },

    #[error(
        "history conflict: batch {batch_id:?} recorded {entity} {entity_id:?}, which has changed since"
    )]
    Conflict {
        batch_id: BatchId,
        entity: String,
        entity_id: String,
    },

    #[error("board revision conflict: expected {expected}, found {found}")]
    RevisionConflict { expected: String, found: String },

    #[error("malformed {what} {id:?}: {detail}")]
    Malformed {
        what: &'static str,
        id: String,
        detail: String,
    },

    #[error("malformed event {event_id} in batch {batch_id:?}: {detail}")]
    MalformedEvent {
        batch_id: BatchId,
        event_id: i64,
        detail: String,
    },

    #[error("snapshot {id:?} does not exist")]
    SnapshotNotFound { id: String },

    #[error("malformed snapshot {id:?}: {detail}")]
    MalformedSnapshot { id: String, detail: String },

    #[error("cannot restore snapshot {id:?}: {detail}")]
    Restore { id: String, detail: String },

    #[error("cannot rewrite the whole board: {detail}")]
    Board { detail: String },

    #[error("invalid argument: {message}")]
    Invalid { message: String },

    #[error(transparent)]
    Time(#[from] TimeError),

    #[error(transparent)]
    Model(#[from] model::ModelError),

    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

// ---------------------------------------------------------------------------
// Undo selection
// ---------------------------------------------------------------------------

/// The apply batches that are currently undone, oldest first.
///
/// Derived, never stored: a batch is undone when an undo batch targets it and
/// no redo batch targets that undo batch. One query, so the definition lives in
/// one place.
pub fn undone_apply_batches(conn: &Connection) -> Result<Vec<BatchId>, HistoryError> {
    let mut statement = conn.prepare(UNDONE_APPLY_SQL)?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut ids = Vec::new();
    for row in rows {
        ids.push(row?);
    }
    Ok(ids)
}

const UNDONE_APPLY_SQL: &str = "SELECT DISTINCT b.id
  FROM batches AS b
  JOIN undo_targets AS ut ON ut.target_batch_id = b.id
  JOIN batches AS u ON u.id = ut.undo_batch_id AND u.kind = 'undo'
 WHERE b.kind = 'apply'
   AND NOT EXISTS (
        SELECT 1
          FROM undo_targets AS rt
          JOIN batches AS rb ON rb.id = rt.undo_batch_id AND rb.kind = 'redo'
         WHERE rt.target_batch_id = u.id
   )
 ORDER BY b.created_at_utc_ms, b.rowid";

/// The newest apply batch that user undo may reverse and that still has
/// events. undo_last and redo_last pick their targets with this shape.
const NEWEST_UNDOABLE_SQL: &str = "SELECT b.id
  FROM batches AS b
 WHERE b.kind = 'apply'
   AND b.actor <> 'system'
   AND EXISTS (SELECT 1 FROM events AS e WHERE e.batch_id = b.id)
   AND NOT EXISTS (
        SELECT 1
          FROM undo_targets AS ut
          JOIN batches AS u ON u.id = ut.undo_batch_id AND u.kind = 'undo'
         WHERE ut.target_batch_id = b.id
           AND NOT EXISTS (
                SELECT 1
                  FROM undo_targets AS rt
                  JOIN batches AS rb ON rb.id = rt.undo_batch_id AND rb.kind = 'redo'
                 WHERE rt.target_batch_id = u.id
           )
   )
 ORDER BY b.created_at_utc_ms DESC, b.rowid DESC
 LIMIT 1";

/// The newest undo batch that no redo batch has re-applied yet.
const NEWEST_REDOABLE_SQL: &str = "SELECT u.id
  FROM batches AS u
 WHERE u.kind = 'undo'
   AND EXISTS (SELECT 1 FROM undo_targets AS ut WHERE ut.undo_batch_id = u.id)
   AND NOT EXISTS (
        SELECT 1
          FROM undo_targets AS rt
          JOIN batches AS rb ON rb.id = rt.undo_batch_id AND rb.kind = 'redo'
         WHERE rt.target_batch_id = u.id
   )
 ORDER BY u.created_at_utc_ms DESC, u.rowid DESC
 LIMIT 1";

fn newest_undoable_batch(conn: &Connection) -> Result<Option<BatchId>, HistoryError> {
    optional_id(conn, NEWEST_UNDOABLE_SQL)
}

fn newest_redoable_batch(conn: &Connection) -> Result<Option<BatchId>, HistoryError> {
    optional_id(conn, NEWEST_REDOABLE_SQL)
}

fn optional_id(conn: &Connection, sql: &str) -> Result<Option<BatchId>, HistoryError> {
    let mut statement = conn.prepare(sql)?;
    let mut rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Undo, redo and turn ranges
// ---------------------------------------------------------------------------

/// Reverse the newest user apply batch.
///
/// The reversing batch is inserted in the same transaction as the inverse
/// writes, so a conflict or SQL error leaves the board and the audit log
/// untouched.
pub fn undo_last(store: &mut Store) -> Result<UndoOutcome, HistoryError> {
    let now = now_utc_ms();
    let tx = store
        .connection_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let Some(target) = newest_undoable_batch(&tx)? else {
        return Err(HistoryError::NothingToUndo);
    };
    let steps = [(Reversal::Undo, target.clone())];
    let outcome = append_reversal(
        &tx,
        now,
        BatchKind::Undo,
        std::slice::from_ref(&target),
        &format!("undo {target}"),
        &steps,
    )?;
    tx.commit()?;
    Ok(outcome)
}

/// Re-apply the newest undo batch that no redo batch has reversed.
///
/// Redo re-applies the undo's target apply batches in their original order, and
/// records undo_targets(redo_batch -> undo_batch).
pub fn redo_last(store: &mut Store) -> Result<UndoOutcome, HistoryError> {
    let now = now_utc_ms();
    let tx = store
        .connection_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let Some(undo_batch) = newest_redoable_batch(&tx)? else {
        return Err(HistoryError::NothingToRedo);
    };
    let targets = undo_target_batches(&tx, &undo_batch)?;
    if targets.is_empty() {
        return Err(HistoryError::NothingToRedo);
    }
    let steps: Vec<(Reversal, BatchId)> = targets
        .iter()
        .map(|target| (Reversal::Redo, target.clone()))
        .collect();
    let outcome = append_reversal(
        &tx,
        now,
        BatchKind::Redo,
        std::slice::from_ref(&undo_batch),
        &format!("redo {undo_batch}"),
        &steps,
    )?;
    tx.commit()?;
    Ok(outcome)
}

/// Reverse a set of batches as one undo (a Pi turn range, task 130).
///
/// The set is reversed newest-first, so a range that edited the same row twice
/// walks back through the intermediate states. actor = 'system' batches and
/// batches that are already undone are skipped; if nothing remains the call
/// returns [HistoryError::NothingToUndo] and writes nothing. A batch that is
/// not an apply batch is a [HistoryError::NotReversible].
pub fn undo_batches(store: &mut Store, batch_ids: &[BatchId]) -> Result<UndoOutcome, HistoryError> {
    let now = now_utc_ms();
    let tx = store
        .connection_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let undone: HashSet<BatchId> = undone_apply_batches(&tx)?.into_iter().collect();

    let mut seen: HashSet<&str> = HashSet::new();
    let mut selected: Vec<(i64, i64, BatchId)> = Vec::new();
    for id in batch_ids {
        if !seen.insert(id.as_str()) {
            continue;
        }
        let meta = batch_meta(&tx, id)?.ok_or_else(|| HistoryError::BatchNotFound {
            batch_id: id.clone(),
        })?;
        if meta.actor == Actor::System.as_str() || undone.contains(id.as_str()) {
            continue;
        }
        let kind = BatchKind::parse(&meta.kind).ok_or_else(|| HistoryError::Malformed {
            what: "batch",
            id: id.clone(),
            detail: format!("unknown kind {:?}", meta.kind),
        })?;
        if kind != BatchKind::Apply {
            return Err(HistoryError::NotReversible {
                batch_id: id.clone(),
                kind: meta.kind,
            });
        }
        selected.push((meta.created_at_utc_ms, meta.rowid, id.clone()));
    }
    if selected.is_empty() {
        return Err(HistoryError::NothingToUndo);
    }
    selected.sort_by_key(|(created_at, rowid, _)| (*created_at, *rowid));

    let links: Vec<BatchId> = selected.into_iter().map(|(_, _, id)| id).collect();
    let steps: Vec<(Reversal, BatchId)> = links
        .iter()
        .rev()
        .map(|id| (Reversal::Undo, id.clone()))
        .collect();
    let label = if links.len() == 1 {
        format!("undo {}", links[0])
    } else {
        format!("undo {} batches", links.len())
    };
    let outcome = append_reversal(&tx, now, BatchKind::Undo, &links, &label, &steps)?;
    tx.commit()?;
    Ok(outcome)
}

fn undo_target_batches(conn: &Connection, undo_batch: &str) -> Result<Vec<BatchId>, HistoryError> {
    let mut statement = conn.prepare(
        "SELECT ut.target_batch_id
           FROM undo_targets AS ut
           JOIN batches AS b ON b.id = ut.target_batch_id
          WHERE ut.undo_batch_id = ?1
          ORDER BY b.created_at_utc_ms, b.rowid",
    )?;
    let rows = statement.query_map([undo_batch], |row| row.get::<_, String>(0))?;
    let mut targets = Vec::new();
    for row in rows {
        let id = row?;
        let meta = batch_meta(conn, &id)?.ok_or_else(|| HistoryError::BatchNotFound {
            batch_id: id.clone(),
        })?;
        if BatchKind::parse(&meta.kind) != Some(BatchKind::Apply) {
            return Err(HistoryError::NotReversible {
                batch_id: id,
                kind: meta.kind,
            });
        }
        targets.push(id);
    }
    Ok(targets)
}

struct BatchMeta {
    created_at_utc_ms: i64,
    rowid: i64,
    actor: String,
    kind: String,
}

fn batch_meta(conn: &Connection, id: &str) -> Result<Option<BatchMeta>, HistoryError> {
    let mut statement =
        conn.prepare("SELECT created_at_utc_ms, rowid, actor, kind FROM batches WHERE id = ?1")?;
    let mut rows = statement.query_map([id], |row| {
        Ok(BatchMeta {
            created_at_utc_ms: row.get(0)?,
            rowid: row.get(1)?,
            actor: row.get(2)?,
            kind: row.get(3)?,
        })
    })?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

/// Append one reversing batch and every event it needs.
///
/// The links go into undo_targets; the steps say which batches are walked and
/// in which order. Both are already validated by the caller.
fn append_reversal(
    conn: &Connection,
    now: i64,
    kind: BatchKind,
    links: &[BatchId],
    label: &str,
    steps: &[(Reversal, BatchId)],
) -> Result<UndoOutcome, HistoryError> {
    let batch_id = new_id();
    conn.execute(
        "INSERT INTO batches (id, created_at_utc_ms, actor, actor_ref, label, kind)
         VALUES (?1, ?2, 'human', NULL, ?3, ?4)",
        params![batch_id, now, label, kind.as_str()],
    )?;

    let mut state = ReversalState {
        now,
        batch_id: batch_id.clone(),
        seq: 0,
        events: 0,
        skipped: 0,
    };
    for (reversal, target) in steps {
        reverse_target(conn, &mut state, *reversal, target)?;
    }
    for link in links {
        conn.execute(
            "INSERT INTO undo_targets (undo_batch_id, target_batch_id) VALUES (?1, ?2)",
            params![batch_id, link],
        )?;
    }

    Ok(UndoOutcome {
        batch_id,
        kind,
        targets: links.to_vec(),
        events: state.events,
        skipped: state.skipped,
    })
}

// ---------------------------------------------------------------------------
// The reversal engine
// ---------------------------------------------------------------------------

struct ReversalState {
    now: i64,
    batch_id: BatchId,
    seq: i64,
    events: usize,
    skipped: usize,
}

fn reverse_target(
    conn: &Connection,
    state: &mut ReversalState,
    reversal: Reversal,
    batch_id: &str,
) -> Result<(), HistoryError> {
    let events = load_events(conn, batch_id)?;
    // board_sql (task 110) records one whole-board event per batch and stores
    // the pre-image as a snapshot tied to that batch. Reversing that snapshot is
    // the correct inverse, so it takes precedence over the per-event images.
    if let Some(board_event) = events.iter().find(|event| event.entity == BOARD_ENTITY)
        && let Some(snapshot) = linked_snapshot(conn, batch_id)?
    {
        return reverse_board_snapshot(conn, state, reversal, batch_id, board_event, &snapshot);
    }
    let mut ordered: Vec<&StoredEvent> = events.iter().collect();
    if reversal == Reversal::Undo {
        ordered.reverse();
    }
    let mut checked: HashSet<RowKey> = HashSet::new();
    for event in ordered {
        reverse_event(conn, state, reversal, batch_id, event, &mut checked)?;
    }
    Ok(())
}

/// The entity name of a whole-board event (task 110's board_sql).
const BOARD_ENTITY: &str = "board";

/// A snapshot tied to a history batch, as recorded by board_sql.
struct LinkedSnapshot {
    id: SnapshotId,
    data: String,
}

fn linked_snapshot(
    conn: &Connection,
    batch_id: &str,
) -> Result<Option<LinkedSnapshot>, HistoryError> {
    let mut statement = conn.prepare(
        "SELECT id, data FROM snapshots
          WHERE batch_id = ?1
          ORDER BY rowid
          LIMIT 1",
    )?;
    let mut rows = statement.query_map([batch_id], |row| {
        Ok(LinkedSnapshot {
            id: row.get(0)?,
            data: row.get(1)?,
        })
    })?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

/// Reverse a batch that ran raw SQL: undo restores the linked snapshot, redo
/// re-applies the board event's after image. The current board must still be
/// the image the batch left behind, or the call is a conflict.
fn reverse_board_snapshot(
    conn: &Connection,
    state: &mut ReversalState,
    reversal: Reversal,
    target_batch: &str,
    event: &StoredEvent,
    snapshot: &LinkedSnapshot,
) -> Result<(), HistoryError> {
    let expected = match reversal {
        Reversal::Undo => parse_optional_board(&event.after_json, event)?,
        Reversal::Redo => parse_optional_board(&event.before_json, event)?,
    };
    let image = match reversal {
        Reversal::Undo => parse_snapshot_board(snapshot)?,
        Reversal::Redo => parse_optional_board(&event.after_json, event)?.ok_or_else(|| {
            HistoryError::MalformedEvent {
                batch_id: event.batch_id.clone(),
                event_id: event.id,
                detail: "a board event needs an after image".to_owned(),
            }
        })?,
    };

    let current = BoardImage::capture(conn)?;
    let conflict = || HistoryError::Conflict {
        batch_id: target_batch.to_owned(),
        entity: BOARD_ENTITY.to_owned(),
        entity_id: event.entity_id.clone(),
    };
    match expected {
        Some(expected) if board_content_matches(&expected, &current) => {}
        _ => return Err(conflict()),
    }

    let before = serde_json::to_value(&current)?;
    replace_board(conn, &image)?;
    if board_revision(conn)? != revision_of(&image)? {
        return Err(HistoryError::Board {
            detail: "the board does not hash back to the image that was written".to_owned(),
        });
    }
    let after = serde_json::to_value(BoardImage::capture(conn)?)?;
    push_event(conn, state, event, Some(before), Some(after))
}

fn parse_optional_board(
    text: &Option<String>,
    event: &StoredEvent,
) -> Result<Option<BoardImage>, HistoryError> {
    match text {
        Some(text) => serde_json::from_str::<BoardImage>(text)
            .map(|image| Some(image.sorted()))
            .map_err(|error| HistoryError::MalformedEvent {
                batch_id: event.batch_id.clone(),
                event_id: event.id,
                detail: format!("board image: {error}"),
            }),
        None => Ok(None),
    }
}

fn parse_snapshot_board(snapshot: &LinkedSnapshot) -> Result<BoardImage, HistoryError> {
    serde_json::from_str::<BoardImage>(&snapshot.data)
        .map(BoardImage::sorted)
        .map_err(|error| HistoryError::MalformedSnapshot {
            id: snapshot.id.clone(),
            detail: format!("expected a board image: {error}"),
        })
}

/// Compare two board images without the trigger-owned version columns.
fn board_content_matches(expected: &BoardImage, current: &BoardImage) -> bool {
    canonical_visible(expected.clone()) == canonical_visible(current.clone())
}

fn canonical_visible(mut image: BoardImage) -> BoardImage {
    for note in &mut image.notes {
        note.version = 0;
        note.updated_at_utc_ms = 0;
    }
    for group in &mut image.groups {
        group.version = 0;
        group.updated_at_utc_ms = 0;
    }
    for edge in &mut image.edges {
        edge.version = 0;
        edge.updated_at_utc_ms = 0;
    }
    image.sorted()
}

/// The (entity, entity_id) of one board row.
type RowKey = (String, String);

fn reverse_event(
    conn: &Connection,
    state: &mut ReversalState,
    reversal: Reversal,
    target_batch: &str,
    event: &StoredEvent,
    checked: &mut HashSet<RowKey>,
) -> Result<(), HistoryError> {
    if !is_board_entity(&event.entity) {
        state.skipped += 1;
        return Ok(());
    }

    // The image the row must currently hold, and the image to write over it.
    let (expected, image) = match reversal {
        Reversal::Undo => (
            parse_optional_image(event, &event.after_json)?,
            parse_optional_image(event, &event.before_json)?,
        ),
        Reversal::Redo => (
            parse_optional_image(event, &event.before_json)?,
            parse_optional_image(event, &event.after_json)?,
        ),
    };

    if event.entity == BOARD_ENTITY && image.is_none() {
        return Err(HistoryError::MalformedEvent {
            batch_id: event.batch_id.clone(),
            event_id: event.id,
            detail: "a board event needs a whole-board image".to_owned(),
        });
    }

    let current = load_current_row(conn, &event.entity, &event.entity_id)?;
    if checked.insert((event.entity.clone(), event.entity_id.clone())) {
        check_row_state(target_batch, event, expected.as_ref(), current.as_ref())?;
    }

    match &image {
        Some(image) => write_row(conn, image, WriteMode::Update)?,
        None => soft_delete_row(conn, &event.entity, &event.entity_id, state.now)?,
    }

    let after = load_current_row(conn, &event.entity, &event.entity_id)?;
    let before_value = current.map(|image| image.to_value()).transpose()?;
    let after_value = after.map(|image| image.to_value()).transpose()?;
    push_event(conn, state, event, before_value, after_value)
}

fn check_row_state(
    target_batch: &str,
    event: &StoredEvent,
    expected: Option<&RowImage>,
    current: Option<&RowImage>,
) -> Result<(), HistoryError> {
    let conflict = || HistoryError::Conflict {
        batch_id: target_batch.to_owned(),
        entity: event.entity.clone(),
        entity_id: event.entity_id.clone(),
    };
    match (expected, current) {
        (Some(expected), Some(current)) => {
            if expected.matches(current) {
                Ok(())
            } else {
                Err(conflict())
            }
        }
        // A row that did not exist is "absent"; a soft-deleted row is how the
        // board expresses that, so it satisfies the expectation.
        (None, Some(current)) => {
            if image_is_deleted(current) {
                Ok(())
            } else {
                Err(conflict())
            }
        }
        // A missing row cannot be rewritten without inventing it.
        _ => Err(conflict()),
    }
}

fn push_event(
    conn: &Connection,
    state: &mut ReversalState,
    original: &StoredEvent,
    before: Option<Value>,
    after: Option<Value>,
) -> Result<(), HistoryError> {
    let batch_id = state.batch_id.clone();
    let seq = state.seq;
    let now = state.now;
    let before = before
        .map(|value| serde_json::to_string(&value))
        .transpose()?;
    let after = after
        .map(|value| serde_json::to_string(&value))
        .transpose()?;
    conn.execute(
        "INSERT INTO events
             (batch_id, seq, at_utc_ms, actor, command, entity, entity_id,
              payload_json, before_json, after_json)
         VALUES (?1, ?2, ?3, 'human', ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            batch_id,
            seq,
            now,
            original.command,
            original.entity,
            original.entity_id,
            original.payload_json,
            before,
            after
        ],
    )?;
    state.seq += 1;
    state.events += 1;
    Ok(())
}

// ---------------------------------------------------------------------------
// Row images
// ---------------------------------------------------------------------------

/// A full board row as recorded in an event or read from a table.
enum RowImage {
    // Boxed because a board image and a Note are much larger than an Edge.
    Board(Box<BoardImage>),
    Note(Box<Note>),
    Group(Group),
    Edge(Edge),
}

impl RowImage {
    fn parse(event: &StoredEvent, text: &str) -> Result<Self, HistoryError> {
        let malformed = |detail: String| HistoryError::MalformedEvent {
            batch_id: event.batch_id.clone(),
            event_id: event.id,
            detail,
        };
        match event.entity.as_str() {
            "board" => serde_json::from_str(text)
                .map(|image: BoardImage| Self::Board(Box::new(image.sorted())))
                .map_err(|error| malformed(format!("board image: {error}"))),
            "note" => serde_json::from_str(text)
                .map(|note| Self::Note(Box::new(note)))
                .map_err(|error| malformed(format!("note image: {error}"))),
            "group" => serde_json::from_str(text)
                .map(Self::Group)
                .map_err(|error| malformed(format!("group image: {error}"))),
            _ => serde_json::from_str(text)
                .map(Self::Edge)
                .map_err(|error| malformed(format!("edge image: {error}"))),
        }
    }

    fn to_value(&self) -> Result<Value, HistoryError> {
        Ok(match self {
            Self::Board(image) => serde_json::to_value(image)?,
            Self::Note(note) => serde_json::to_value(note)?,
            Self::Group(group) => serde_json::to_value(group)?,
            Self::Edge(edge) => serde_json::to_value(edge)?,
        })
    }

    /// Compare everything the user can see. version and updated_at_utc_ms are
    /// copied from current first because the triggers own them and every
    /// reversal bumps them.
    fn matches(&self, current: &Self) -> bool {
        match (self, current) {
            (Self::Board(wanted), Self::Board(current)) => board_content_matches(wanted, current),
            (Self::Note(wanted), Self::Note(current)) => {
                let mut wanted = wanted.as_ref().clone();
                wanted.version = current.version;
                wanted.updated_at_utc_ms = current.updated_at_utc_ms;
                wanted == **current
            }
            (Self::Group(wanted), Self::Group(current)) => {
                let mut wanted = wanted.clone();
                wanted.version = current.version;
                wanted.updated_at_utc_ms = current.updated_at_utc_ms;
                wanted == *current
            }
            (Self::Edge(wanted), Self::Edge(current)) => {
                let mut wanted = wanted.clone();
                wanted.version = current.version;
                wanted.updated_at_utc_ms = current.updated_at_utc_ms;
                wanted == *current
            }
            _ => false,
        }
    }
}

fn is_board_entity(entity: &str) -> bool {
    matches!(entity, "board" | "note" | "group" | "edge")
}

fn image_is_deleted(image: &RowImage) -> bool {
    match image {
        RowImage::Board(_) => false,
        RowImage::Note(note) => note.deleted_at_utc_ms.is_some(),
        RowImage::Group(group) => group.deleted_at_utc_ms.is_some(),
        RowImage::Edge(edge) => edge.deleted_at_utc_ms.is_some(),
    }
}

fn parse_optional_image(
    event: &StoredEvent,
    text: &Option<String>,
) -> Result<Option<RowImage>, HistoryError> {
    match text {
        Some(text) => Ok(Some(RowImage::parse(event, text)?)),
        None => Ok(None),
    }
}

fn load_current_row(
    conn: &Connection,
    entity: &str,
    id: &str,
) -> Result<Option<RowImage>, HistoryError> {
    Ok(match entity {
        "board" => Some(RowImage::Board(Box::new(BoardImage::capture(conn)?))),
        "note" => model::load_note(conn, id)?.map(|note| RowImage::Note(Box::new(note))),
        "group" => model::load_group(conn, id)?.map(RowImage::Group),
        _ => model::load_edge(conn, id)?.map(RowImage::Edge),
    })
}

fn soft_delete_row(
    conn: &Connection,
    entity: &str,
    id: &str,
    now: i64,
) -> Result<(), HistoryError> {
    let sql = match entity {
        "note" => {
            "UPDATE notes SET deleted_at_utc_ms = ?1
              WHERE id = ?2 AND deleted_at_utc_ms IS NULL"
        }
        "group" => {
            "UPDATE groups SET deleted_at_utc_ms = ?1
              WHERE id = ?2 AND deleted_at_utc_ms IS NULL"
        }
        _ => {
            "UPDATE edges SET deleted_at_utc_ms = ?1
              WHERE id = ?2 AND deleted_at_utc_ms IS NULL"
        }
    };
    conn.execute(sql, params![now, id])?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WriteMode {
    /// User-visible columns only; the triggers own version/updated_at.
    Update,
    /// Every column, including version/updated_at (snapshot restore).
    Replace,
    /// Insert every column, including version/updated_at.
    Insert,
}

fn write_row(conn: &Connection, image: &RowImage, mode: WriteMode) -> Result<(), HistoryError> {
    match image {
        RowImage::Board(image) => replace_board(conn, image),
        RowImage::Note(note) => write_note(conn, note, mode),
        RowImage::Group(group) => write_group(conn, group, mode),
        RowImage::Edge(edge) => write_edge(conn, edge, mode),
    }
}

const NOTE_UPDATE_SQL: &str = "UPDATE notes SET
        text = :text,
        group_id = :group_id,
        parent_id = :parent_id,
        layout_mode = :layout_mode,
        pos_x = :pos_x,
        pos_y = :pos_y,
        sort_order = :sort_order,
        color = :color,
        priority = :priority,
        status = :status,
        source = :source,
        created_at_utc_ms = :created_at,
        deleted_at_utc_ms = :deleted_at,
        due_local = :due_local,
        due_tz = :due_tz,
        due_utc_ms = :due_utc_ms,
        remind_local = :remind_local,
        remind_tz = :remind_tz,
        remind_utc_ms = :remind_utc_ms,
        reminded_at_utc_ms = :reminded_at,
        snooze_local = :snooze_local,
        snooze_tz = :snooze_tz,
        snooze_utc_ms = :snooze_utc_ms,
        completed_at_utc_ms = :completed_at
    WHERE id = :id";

const NOTE_REPLACE_SQL: &str = "UPDATE notes SET
        text = :text,
        group_id = :group_id,
        parent_id = :parent_id,
        layout_mode = :layout_mode,
        pos_x = :pos_x,
        pos_y = :pos_y,
        sort_order = :sort_order,
        color = :color,
        priority = :priority,
        status = :status,
        source = :source,
        created_at_utc_ms = :created_at,
        updated_at_utc_ms = :updated_at,
        deleted_at_utc_ms = :deleted_at,
        version = :version,
        due_local = :due_local,
        due_tz = :due_tz,
        due_utc_ms = :due_utc_ms,
        remind_local = :remind_local,
        remind_tz = :remind_tz,
        remind_utc_ms = :remind_utc_ms,
        reminded_at_utc_ms = :reminded_at,
        snooze_local = :snooze_local,
        snooze_tz = :snooze_tz,
        snooze_utc_ms = :snooze_utc_ms,
        completed_at_utc_ms = :completed_at
    WHERE id = :id";

const NOTE_INSERT_SQL: &str = "INSERT INTO notes
        (id, text, group_id, parent_id, layout_mode, pos_x, pos_y, sort_order,
         color, priority, status, source, created_at_utc_ms, updated_at_utc_ms,
         deleted_at_utc_ms, version, due_local, due_tz, due_utc_ms, remind_local,
         remind_tz, remind_utc_ms, reminded_at_utc_ms, snooze_local, snooze_tz,
         snooze_utc_ms, completed_at_utc_ms)
    VALUES
        (:id, :text, :group_id, :parent_id, :layout_mode, :pos_x, :pos_y, :sort_order,
         :color, :priority, :status, :source, :created_at, :updated_at,
         :deleted_at, :version, :due_local, :due_tz, :due_utc_ms, :remind_local,
         :remind_tz, :remind_utc_ms, :reminded_at, :snooze_local, :snooze_tz,
         :snooze_utc_ms, :completed_at)";

fn write_note(conn: &Connection, note: &Note, mode: WriteMode) -> Result<(), HistoryError> {
    let (due_local, due_tz, due_utc_ms) = time_columns(note.due.as_ref())?;
    let (remind_local, remind_tz, remind_utc_ms) = time_columns(note.remind.as_ref())?;
    let (snooze_local, snooze_tz, snooze_utc_ms) = time_columns(note.snooze.as_ref())?;
    let (pos_x, pos_y) = position(note);
    let priority = note.priority.map(|priority| priority.as_str());
    let layout_mode = note.layout.as_str();
    let status = note.status.as_str();

    let mut values: Vec<(&'static str, &dyn ToSql)> = vec![
        (":id", &note.id),
        (":text", &note.text),
        (":group_id", &note.group),
        (":parent_id", &note.parent),
        (":layout_mode", &layout_mode),
        (":pos_x", &pos_x),
        (":pos_y", &pos_y),
        (":sort_order", &note.sort),
        (":color", &note.color),
        (":priority", &priority),
        (":status", &status),
        (":source", &note.source),
        (":created_at", &note.created_at_utc_ms),
        (":deleted_at", &note.deleted_at_utc_ms),
        (":due_local", &due_local),
        (":due_tz", &due_tz),
        (":due_utc_ms", &due_utc_ms),
        (":remind_local", &remind_local),
        (":remind_tz", &remind_tz),
        (":remind_utc_ms", &remind_utc_ms),
        (":reminded_at", &note.reminded_at_utc_ms),
        (":snooze_local", &snooze_local),
        (":snooze_tz", &snooze_tz),
        (":snooze_utc_ms", &snooze_utc_ms),
        (":completed_at", &note.completed_at_utc_ms),
    ];
    if mode != WriteMode::Update {
        values.push((":updated_at", &note.updated_at_utc_ms));
        values.push((":version", &note.version));
    }
    let sql = match mode {
        WriteMode::Update => NOTE_UPDATE_SQL,
        WriteMode::Replace => NOTE_REPLACE_SQL,
        WriteMode::Insert => NOTE_INSERT_SQL,
    };
    conn.execute(sql, values.as_slice())?;
    Ok(())
}

const GROUP_UPDATE_SQL: &str = "UPDATE groups SET
        name = :name,
        color = :color,
        sort_order = :sort_order,
        created_at_utc_ms = :created_at,
        deleted_at_utc_ms = :deleted_at
    WHERE id = :id";

const GROUP_REPLACE_SQL: &str = "UPDATE groups SET
        name = :name,
        color = :color,
        sort_order = :sort_order,
        created_at_utc_ms = :created_at,
        updated_at_utc_ms = :updated_at,
        deleted_at_utc_ms = :deleted_at,
        version = :version
    WHERE id = :id";

const GROUP_INSERT_SQL: &str = "INSERT INTO groups
        (id, name, color, sort_order, created_at_utc_ms, updated_at_utc_ms,
         deleted_at_utc_ms, version)
    VALUES
        (:id, :name, :color, :sort_order, :created_at, :updated_at, :deleted_at, :version)";

fn write_group(conn: &Connection, group: &Group, mode: WriteMode) -> Result<(), HistoryError> {
    let mut values: Vec<(&'static str, &dyn ToSql)> = vec![
        (":id", &group.id),
        (":name", &group.name),
        (":color", &group.color),
        (":sort_order", &group.sort_order),
        (":created_at", &group.created_at_utc_ms),
        (":deleted_at", &group.deleted_at_utc_ms),
    ];
    if mode != WriteMode::Update {
        values.push((":updated_at", &group.updated_at_utc_ms));
        values.push((":version", &group.version));
    }
    let sql = match mode {
        WriteMode::Update => GROUP_UPDATE_SQL,
        WriteMode::Replace => GROUP_REPLACE_SQL,
        WriteMode::Insert => GROUP_INSERT_SQL,
    };
    conn.execute(sql, values.as_slice())?;
    Ok(())
}

const EDGE_UPDATE_SQL: &str = "UPDATE edges SET
        \"from\" = :from,
        \"to\" = :to,
        kind = :kind,
        created_at_utc_ms = :created_at,
        deleted_at_utc_ms = :deleted_at
    WHERE id = :id";

const EDGE_REPLACE_SQL: &str = "UPDATE edges SET
        \"from\" = :from,
        \"to\" = :to,
        kind = :kind,
        created_at_utc_ms = :created_at,
        updated_at_utc_ms = :updated_at,
        deleted_at_utc_ms = :deleted_at,
        version = :version
    WHERE id = :id";

const EDGE_INSERT_SQL: &str = "INSERT INTO edges
        (id, \"from\", \"to\", kind, created_at_utc_ms, updated_at_utc_ms,
         deleted_at_utc_ms, version)
    VALUES
        (:id, :from, :to, :kind, :created_at, :updated_at, :deleted_at, :version)";

fn write_edge(conn: &Connection, edge: &Edge, mode: WriteMode) -> Result<(), HistoryError> {
    let mut values: Vec<(&'static str, &dyn ToSql)> = vec![
        (":id", &edge.id),
        (":from", &edge.from),
        (":to", &edge.to),
        (":kind", &edge.kind),
        (":created_at", &edge.created_at_utc_ms),
        (":deleted_at", &edge.deleted_at_utc_ms),
    ];
    if mode != WriteMode::Update {
        values.push((":updated_at", &edge.updated_at_utc_ms));
        values.push((":version", &edge.version));
    }
    let sql = match mode {
        WriteMode::Update => EDGE_UPDATE_SQL,
        WriteMode::Replace => EDGE_REPLACE_SQL,
        WriteMode::Insert => EDGE_INSERT_SQL,
    };
    conn.execute(sql, values.as_slice())?;
    Ok(())
}

fn position(note: &Note) -> (Option<f64>, Option<f64>) {
    match note.pos {
        Some(point) => (Some(f64::from(point.x)), Some(f64::from(point.y))),
        None => (None, None),
    }
}

type TimeColumns = (Option<String>, Option<String>, Option<i64>);

fn time_columns(spec: Option<&TimeSpec>) -> Result<TimeColumns, HistoryError> {
    match spec {
        None => Ok((None, None, None)),
        Some(spec) => {
            let parts = spec.to_sql_parts()?;
            Ok((Some(parts.local), Some(parts.tz), Some(parts.utc_ms)))
        }
    }
}

// ---------------------------------------------------------------------------
// Reading the log
// ---------------------------------------------------------------------------

const EVENT_COLUMNS: &str = "e.id, e.batch_id, e.seq, e.at_utc_ms, e.actor, e.command,
     e.entity, e.entity_id, e.payload_json, e.before_json, e.after_json";

struct StoredEvent {
    id: i64,
    batch_id: BatchId,
    seq: i64,
    at_utc_ms: i64,
    actor: String,
    command: String,
    entity: String,
    entity_id: String,
    payload_json: String,
    before_json: Option<String>,
    after_json: Option<String>,
}

fn map_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredEvent> {
    Ok(StoredEvent {
        id: row.get(0)?,
        batch_id: row.get(1)?,
        seq: row.get(2)?,
        at_utc_ms: row.get(3)?,
        actor: row.get(4)?,
        command: row.get(5)?,
        entity: row.get(6)?,
        entity_id: row.get(7)?,
        payload_json: row.get(8)?,
        before_json: row.get(9)?,
        after_json: row.get(10)?,
    })
}

fn load_events(conn: &Connection, batch_id: &str) -> Result<Vec<StoredEvent>, HistoryError> {
    let sql =
        format!("SELECT {EVENT_COLUMNS} FROM events AS e WHERE e.batch_id = ?1 ORDER BY e.seq");
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map([batch_id], map_event)?;
    let mut events = Vec::new();
    for row in rows {
        events.push(row?);
    }
    Ok(events)
}

fn actor_from(value: &str) -> Option<Actor> {
    match value {
        "human" => Some(Actor::Human),
        "agent" => Some(Actor::Agent),
        "system" => Some(Actor::System),
        _ => None,
    }
}

/// Every event recorded for one note, in the order it was written.
pub fn history(conn: &Connection, note_id: &str) -> Result<Vec<HistoryEntry>, HistoryError> {
    let sql = format!(
        "SELECT {EVENT_COLUMNS}, b.kind
           FROM events AS e
           JOIN batches AS b ON b.id = e.batch_id
          WHERE e.entity = 'note' AND e.entity_id = ?1
          ORDER BY e.id"
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map([note_id], |row| {
        let event = map_event(row)?;
        let kind: String = row.get(11)?;
        Ok((event, kind))
    })?;

    let mut entries = Vec::new();
    for row in rows {
        let (event, kind) = row?;
        let batch_kind = BatchKind::parse(&kind).ok_or_else(|| HistoryError::Malformed {
            what: "batch",
            id: event.batch_id.clone(),
            detail: format!("unknown kind {kind:?}"),
        })?;
        let actor = actor_from(&event.actor).ok_or_else(|| HistoryError::Malformed {
            what: "event",
            id: event.id.to_string(),
            detail: format!("unknown actor {:?}", event.actor),
        })?;
        entries.push(HistoryEntry {
            event_id: event.id,
            batch_id: event.batch_id,
            batch_kind,
            seq: event.seq,
            at_utc_ms: event.at_utc_ms,
            actor,
            command: event.command,
            payload: serde_json::from_str(&event.payload_json)?,
            before: optional_json(event.before_json)?,
            after: optional_json(event.after_json)?,
        });
    }
    Ok(entries)
}

/// Batches created in [from_utc_ms, to_utc_ms], oldest first.
///
/// The chat UI (task 130) records a turn's range and filters this list by
/// actor and kind; returning the whole range keeps one read for the transcript.
pub fn batches_between(
    conn: &Connection,
    from_utc_ms: i64,
    to_utc_ms: i64,
) -> Result<Vec<BatchSummary>, HistoryError> {
    if from_utc_ms > to_utc_ms {
        return Ok(Vec::new());
    }
    let base = {
        let mut statement = conn.prepare(
            "SELECT id, kind, actor, actor_ref, label, created_at_utc_ms
               FROM batches
              WHERE created_at_utc_ms >= ?1 AND created_at_utc_ms <= ?2
              ORDER BY created_at_utc_ms, rowid",
        )?;
        let rows = statement.query_map(params![from_utc_ms, to_utc_ms], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;
        let mut base = Vec::new();
        for row in rows {
            base.push(row?);
        }
        base
    };

    let mut summaries = Vec::new();
    for (id, kind, actor, actor_ref, label, created_at_utc_ms) in base {
        let batch_kind = BatchKind::parse(&kind).ok_or_else(|| HistoryError::Malformed {
            what: "batch",
            id: id.clone(),
            detail: format!("unknown kind {kind:?}"),
        })?;
        let actor = actor_from(&actor).ok_or_else(|| HistoryError::Malformed {
            what: "batch",
            id: id.clone(),
            detail: format!("unknown actor {actor:?}"),
        })?;
        let targets = {
            let mut statement = conn.prepare(
                "SELECT target_batch_id FROM undo_targets
                  WHERE undo_batch_id = ?1 ORDER BY target_batch_id",
            )?;
            let rows = statement.query_map([&id], |row| row.get::<_, String>(0))?;
            let mut targets = Vec::new();
            for row in rows {
                targets.push(row?);
            }
            targets
        };
        let events = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE batch_id = ?1",
            [&id],
            |row| row.get(0),
        )?;
        summaries.push(BatchSummary {
            id,
            kind: batch_kind,
            actor,
            actor_ref,
            label,
            created_at_utc_ms,
            targets,
            events,
        });
    }
    Ok(summaries)
}

fn optional_json(text: Option<String>) -> Result<Option<Value>, HistoryError> {
    match text {
        Some(text) => Ok(Some(serde_json::from_str(&text)?)),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------

/// The SHA-256 of the canonical board image.
///
/// Two reads of an unchanged board return the same string; any committed write
/// to an active note, group or edge (including a version bump) changes it. That
/// is what lets board_sql (task 110) detect that its snapshot is stale.
pub fn board_revision(conn: &Connection) -> Result<String, HistoryError> {
    revision_of(&BoardImage::capture(conn)?)
}

/// The canonical board image as JSON: active notes, groups and edges sorted by
/// id, full rows including version.
pub fn capture_board(conn: &Connection) -> Result<Value, HistoryError> {
    Ok(serde_json::to_value(BoardImage::capture(conn)?)?)
}

/// Record one snapshot row and return its id.
///
/// The reason must not be blank (the schema enforces it too). The batch id
/// links the snapshot to the history batch it was taken around, when there is
/// one.
pub fn record_snapshot(
    conn: &Connection,
    data: &Value,
    reason: &str,
    batch_id: Option<&BatchId>,
) -> Result<SnapshotId, HistoryError> {
    insert_snapshot(conn, data, reason, batch_id, now_utc_ms())
}

/// Replace the board tables with a recorded snapshot.
///
/// Fails with [HistoryError::RevisionConflict] unless the board still hashes to
/// expected_revision. Rows in a board table that are not in the snapshot are
/// hard-deleted (edges, then notes, then groups); rows in the snapshot are
/// inserted or rewritten with the snapshot's ids, timestamps and versions.
/// Foreign keys are deferred inside the transaction rather than toggled.
///
/// The pre-restore image is recorded as a new snapshot, and a redo-kind batch
/// is appended when the snapshot names a batch; see the module docs.
pub fn restore_snapshot(
    store: &mut Store,
    snapshot_id: &str,
    expected_revision: &str,
) -> Result<(), HistoryError> {
    let now = now_utc_ms();
    let tx = store
        .connection_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    restore_in(&tx, snapshot_id, expected_revision, now)?;
    tx.commit()?;
    Ok(())
}

fn restore_in(
    conn: &Connection,
    snapshot_id: &str,
    expected_revision: &str,
    now: i64,
) -> Result<(), HistoryError> {
    let (batch_id, data) = load_snapshot(conn, snapshot_id)?;
    let image: BoardImage =
        serde_json::from_str(&data).map_err(|error| HistoryError::MalformedSnapshot {
            id: snapshot_id.to_owned(),
            detail: format!(
                "expected {{\"notes\": [...], \"groups\": [...], \"edges\": [...]}}: {error}"
            ),
        })?;
    let image = image.sorted();
    let wanted = revision_of(&image)?;

    let found = board_revision(conn)?;
    if found != expected_revision {
        return Err(HistoryError::RevisionConflict {
            expected: expected_revision.to_owned(),
            found,
        });
    }

    let pre_restore = capture_board(conn)?;
    insert_snapshot(
        conn,
        &pre_restore,
        &format!("pre-restore {snapshot_id}"),
        None,
        now,
    )?;

    if let Some(target) = batch_id {
        let restore_batch = new_id();
        conn.execute(
            "INSERT INTO batches (id, created_at_utc_ms, actor, actor_ref, label, kind)
             VALUES (?1, ?2, 'human', NULL, ?3, 'redo')",
            params![
                restore_batch,
                now,
                format!("restore snapshot {snapshot_id}")
            ],
        )?;
        conn.execute(
            "INSERT INTO undo_targets (undo_batch_id, target_batch_id) VALUES (?1, ?2)",
            params![restore_batch, target],
        )?;
    }

    replace_board(conn, &image).map_err(|error| match error {
        HistoryError::Board { detail } => HistoryError::Restore {
            id: snapshot_id.to_owned(),
            detail,
        },
        other => other,
    })?;

    let got = board_revision(conn)?;
    if got != wanted {
        return Err(HistoryError::Restore {
            id: snapshot_id.to_owned(),
            detail: "the board does not hash back to the snapshot after the restore".to_owned(),
        });
    }
    Ok(())
}

/// Make the board tables hold exactly this image.
///
/// Rows outside the image are hard-deleted (edges, then notes, then groups) and
/// the image's rows are inserted or rewritten with its ids, timestamps and
/// versions. An unchanged row is skipped, and a changed one is written with the
/// image's version so the triggers leave it alone (they only bump a row whose
/// version and updated_at are both unchanged, which normal writes always are).
/// Foreign keys are deferred inside the caller's transaction rather than
/// toggled off, and checked before returning.
fn replace_board(conn: &Connection, image: &BoardImage) -> Result<(), HistoryError> {
    conn.pragma_update(None, "defer_foreign_keys", true)?;

    remove_absent_rows(
        conn,
        "edges",
        image.edges.iter().map(|edge| edge.id.as_str()),
    )?;
    remove_absent_rows(
        conn,
        "notes",
        image.notes.iter().map(|note| note.id.as_str()),
    )?;
    remove_absent_rows(
        conn,
        "groups",
        image.groups.iter().map(|group| group.id.as_str()),
    )?;

    for group in &image.groups {
        upsert_group(conn, group)?;
    }
    for note in &image.notes {
        upsert_note(conn, note)?;
    }
    for edge in &image.edges {
        upsert_edge(conn, edge)?;
    }

    let violations: i64 =
        conn.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if violations != 0 {
        return Err(HistoryError::Board {
            detail: format!("the image would leave {violations} foreign-key violation(s)"),
        });
    }
    Ok(())
}

fn revision_of(image: &BoardImage) -> Result<String, HistoryError> {
    Ok(sha256_hex(serde_json::to_string(image)?.as_bytes()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn load_snapshot(conn: &Connection, id: &str) -> Result<(Option<BatchId>, String), HistoryError> {
    let mut statement = conn.prepare("SELECT batch_id, data FROM snapshots WHERE id = ?1")?;
    let mut rows = statement.query_map([id], |row| {
        Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?))
    })?;
    match rows.next() {
        Some(row) => Ok(row?),
        None => Err(HistoryError::SnapshotNotFound { id: id.to_owned() }),
    }
}

fn insert_snapshot(
    conn: &Connection,
    data: &Value,
    reason: &str,
    batch_id: Option<&BatchId>,
    now: i64,
) -> Result<SnapshotId, HistoryError> {
    if reason.trim().is_empty() {
        return Err(HistoryError::Invalid {
            message: "snapshot reason must not be empty".to_owned(),
        });
    }
    let id = new_id();
    let data = serde_json::to_string(data)?;
    conn.execute(
        "INSERT INTO snapshots (id, batch_id, at_utc_ms, data, reason)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, batch_id, now, data, reason],
    )?;
    Ok(id)
}

fn remove_absent_rows<'a>(
    conn: &Connection,
    table: &'static str,
    keep: impl Iterator<Item = &'a str>,
) -> Result<(), HistoryError> {
    let keep: HashSet<&str> = keep.collect();
    let present = {
        let sql = format!("SELECT id FROM {table}");
        let mut statement = conn.prepare(&sql)?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut present = Vec::new();
        for row in rows {
            present.push(row?);
        }
        present
    };
    let sql = format!("DELETE FROM {table} WHERE id = ?1");
    for id in present {
        if !keep.contains(id.as_str()) {
            conn.execute(&sql, [id])?;
        }
    }
    Ok(())
}

fn upsert_group(conn: &Connection, group: &Group) -> Result<(), HistoryError> {
    match model::load_group(conn, &group.id)? {
        Some(current) if current == *group => Ok(()),
        Some(_) => write_group(conn, group, WriteMode::Replace),
        None => write_group(conn, group, WriteMode::Insert),
    }
}

fn upsert_note(conn: &Connection, note: &Note) -> Result<(), HistoryError> {
    match model::load_note(conn, &note.id)? {
        Some(current) if current == *note => Ok(()),
        Some(_) => write_note(conn, note, WriteMode::Replace),
        None => write_note(conn, note, WriteMode::Insert),
    }
}

fn upsert_edge(conn: &Connection, edge: &Edge) -> Result<(), HistoryError> {
    match model::load_edge(conn, &edge.id)? {
        Some(current) if current == *edge => Ok(()),
        Some(_) => write_edge(conn, edge, WriteMode::Replace),
        None => write_edge(conn, edge, WriteMode::Insert),
    }
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{Command, audit_counts};
    use crate::model::Priority;
    use crate::store::{StorePaths, schema, scratch_dir};

    fn store(name: &str) -> Store {
        let paths = StorePaths::from_home(scratch_dir(name));
        schema::open_board(&paths).expect("board opens")
    }

    fn add_note(text: &str) -> Command {
        Command::AddNote {
            text: text.to_owned(),
            group: None,
            parent: None,
            due: None,
            remind: None,
            priority: None,
            source: "test".to_owned(),
        }
    }

    fn set_text(id: &str, text: &str) -> Command {
        Command::SetText {
            id: id.to_owned(),
            text: text.to_owned(),
        }
    }

    fn set_priority(id: &str, priority: Priority) -> Command {
        Command::SetPriority {
            id: id.to_owned(),
            priority: Some(priority),
        }
    }

    fn apply(store: &mut Store, commands: &[Command]) -> BatchId {
        store
            .apply(commands, Actor::Human, Some("test"))
            .expect("apply")
            .batch_id
    }

    fn apply_as(store: &mut Store, actor: Actor, commands: &[Command]) -> BatchId {
        store
            .apply(commands, actor, Some("test"))
            .expect("apply")
            .batch_id
    }

    fn add(store: &mut Store, text: &str) -> (BatchId, String) {
        let applied = store
            .apply(&[add_note(text)], Actor::Human, Some("test"))
            .expect("add note");
        (applied.batch_id, applied.new_ids.notes[0].clone())
    }

    fn note(store: &Store, id: &str) -> Option<Note> {
        model::load_note(store.connection(), id).expect("read note")
    }

    fn visible_board(store: &Store) -> model::Board {
        let mut board = model::Board::load(store.connection()).expect("load board");
        for note in &mut board.notes {
            note.version = 0;
            note.updated_at_utc_ms = 0;
        }
        for group in &mut board.groups {
            group.version = 0;
            group.updated_at_utc_ms = 0;
        }
        for edge in &mut board.edges {
            edge.version = 0;
            edge.updated_at_utc_ms = 0;
        }
        board
    }

    fn batch_kind(store: &Store, id: &str) -> BatchKind {
        let meta = batch_meta(store.connection(), id)
            .expect("batch meta")
            .expect("batch exists");
        BatchKind::parse(&meta.kind).expect("known kind")
    }

    fn counts(store: &Store) -> (i64, i64, i64, i64) {
        audit_counts(store.connection()).expect("audit counts")
    }

    fn assert_counts_only_grow(store: &Store, previous: &mut (i64, i64, i64, i64)) {
        let now = counts(store);
        assert!(
            now.0 >= previous.0
                && now.1 >= previous.1
                && now.2 >= previous.2
                && now.3 >= previous.3,
            "audit rows only grow: {previous:?} -> {now:?}"
        );
        *previous = now;
    }

    fn insert_batch(store: &Store, id: &str, actor: &str, kind: &str, created_at: i64) {
        store
            .connection()
            .execute(
                "INSERT INTO batches (id, created_at_utc_ms, actor, actor_ref, label, kind)
                 VALUES (?1, ?2, ?3, NULL, 'synthetic', ?4)",
                params![id, created_at, actor, kind],
            )
            .expect("synthetic batch");
    }

    fn newest_undo_batch(store: &Store) -> BatchId {
        batches_between(store.connection(), 0, now_utc_ms() + 1)
            .expect("batches")
            .into_iter()
            .rfind(|batch| batch.kind == BatchKind::Undo)
            .expect("an undo batch")
            .id
    }

    #[test]
    fn an_empty_history_reports_nothing_to_undo_or_redo() {
        let mut store = store("history-empty");
        assert!(matches!(
            undo_last(&mut store).expect_err("nothing to undo"),
            HistoryError::NothingToUndo
        ));
        assert!(matches!(
            redo_last(&mut store).expect_err("nothing to redo"),
            HistoryError::NothingToRedo
        ));
        assert!(matches!(
            undo_batches(&mut store, &[]).expect_err("nothing to undo"),
            HistoryError::NothingToUndo
        ));
        assert_eq!(counts(&store), (0, 0, 0, 0), "failed calls write nothing");
    }

    #[test]
    fn undo_of_an_add_soft_deletes_the_note_and_its_group() {
        let mut store = store("history-undo-add");
        let applied = store
            .apply(
                &[Command::AddNote {
                    text: "buy milk".to_owned(),
                    group: Some("Home".to_owned()),
                    parent: None,
                    due: None,
                    remind: None,
                    priority: None,
                    source: "test".to_owned(),
                }],
                Actor::Human,
                Some("test"),
            )
            .expect("add");
        let note_id = applied.new_ids.notes[0].clone();
        let group_id = applied.new_ids.groups[0].clone();

        let outcome = undo_last(&mut store).expect("undo");
        assert_eq!(outcome.kind, BatchKind::Undo);
        assert_eq!(outcome.targets, vec![applied.batch_id.clone()]);
        assert_eq!(outcome.events, 2, "the note and the group it created");
        assert_eq!(outcome.skipped, 0);

        assert!(visible_board(&store).notes.is_empty());
        assert!(visible_board(&store).groups.is_empty());
        let row = note(&store, &note_id).expect("the row is kept");
        assert!(row.deleted_at_utc_ms.is_some());
        assert!(row.version > 1, "the triggers bumped the version");
        let group = model::load_group(store.connection(), &group_id)
            .expect("read group")
            .expect("group row is kept");
        assert!(group.deleted_at_utc_ms.is_some());

        assert_eq!(
            undone_apply_batches(store.connection()).expect("undone"),
            vec![applied.batch_id.clone()]
        );
        assert_eq!(batch_kind(&store, &outcome.batch_id), BatchKind::Undo);
    }

    #[test]
    fn undo_of_set_text_restores_the_previous_text() {
        let mut store = store("history-undo-text");
        let (_, id) = add(&mut store, "first");
        let first = note(&store, &id).expect("note");
        let batch = apply(&mut store, &[set_text(&id, "second")]);
        assert_eq!(note(&store, &id).expect("note").text, "second");

        let outcome = undo_last(&mut store).expect("undo");
        assert_eq!(outcome.targets, vec![batch]);
        let restored = note(&store, &id).expect("note");
        assert_eq!(restored.text, "first");
        assert!(restored.deleted_at_utc_ms.is_none());
        assert!(restored.version > first.version);
        assert!(restored.updated_at_utc_ms > first.updated_at_utc_ms);
        assert_eq!(
            undone_apply_batches(store.connection()).expect("undone"),
            vec![outcome.targets[0].clone()]
        );
    }

    #[test]
    fn undo_of_a_subtree_delete_restores_notes_and_edges() {
        let mut store = store("history-undo-delete");
        let (_, parent) = add(&mut store, "parent");
        let child_applied = store
            .apply(
                &[Command::AddNote {
                    text: "child".to_owned(),
                    group: None,
                    parent: Some(parent.clone()),
                    due: None,
                    remind: None,
                    priority: None,
                    source: "test".to_owned(),
                }],
                Actor::Human,
                None,
            )
            .expect("child");
        let child = child_applied.new_ids.notes[0].clone();
        let (_, other) = add(&mut store, "other");
        let link = store
            .apply(
                &[Command::Link {
                    from: parent.clone(),
                    to: other.clone(),
                    kind: None,
                }],
                Actor::Human,
                None,
            )
            .expect("link");
        let edge = link.new_ids.edges[0].clone();
        let before = visible_board(&store);

        let delete_batch = apply(&mut store, &[Command::Delete { id: parent.clone() }]);
        let after_delete = visible_board(&store);
        assert_eq!(
            after_delete.notes.len(),
            1,
            "only the unrelated root is left"
        );
        assert_eq!(after_delete.notes[0].id, other);
        assert!(after_delete.edges.is_empty());

        let outcome = undo_last(&mut store).expect("undo");
        assert_eq!(outcome.targets, vec![delete_batch]);
        assert_eq!(outcome.events, 3, "two notes and one edge");
        assert_eq!(visible_board(&store), before, "the whole subtree is back");
        assert!(note(&store, &child).expect("child").is_active());
        let restored_edge = model::load_edge(store.connection(), &edge)
            .expect("read edge")
            .expect("edge exists");
        assert!(restored_edge.deleted_at_utc_ms.is_none());
    }

    #[test]
    fn undo_of_complete_snooze_and_reparent_restores_each_field() {
        let mut store = store("history-undo-fields");
        let (_, id) = add(&mut store, "task");
        let (_, parent) = add(&mut store, "parent");

        apply(&mut store, &[Command::Complete { id: id.clone() }]);
        assert_eq!(note(&store, &id).expect("note").status, model::Status::Done);
        undo_last(&mut store).expect("undo complete");
        assert_eq!(note(&store, &id).expect("note").status, model::Status::Open);
        assert!(
            note(&store, &id)
                .expect("note")
                .completed_at_utc_ms
                .is_none()
        );

        apply(
            &mut store,
            &[Command::Snooze {
                id: id.clone(),
                until: TimeSpec {
                    local: "2026-12-01T08:00:00".to_owned(),
                    tz: "Europe/London".to_owned(),
                },
            }],
        );
        assert!(note(&store, &id).expect("note").snooze.is_some());
        undo_last(&mut store).expect("undo snooze");
        assert!(note(&store, &id).expect("note").snooze.is_none());

        apply(
            &mut store,
            &[Command::SetParent {
                id: id.clone(),
                parent: Some(parent.clone()),
            }],
        );
        assert_eq!(
            note(&store, &id).expect("note").parent,
            Some(parent.clone())
        );
        undo_last(&mut store).expect("undo reparent");
        assert_eq!(note(&store, &id).expect("note").parent, None);
        assert_eq!(visible_board(&store).notes.len(), 2);
    }

    #[test]
    fn redo_re_applies_the_undone_batch() {
        let mut store = store("history-redo");
        let (_, id) = add(&mut store, "first");
        let text_batch = apply(&mut store, &[set_text(&id, "second")]);
        let applied_state = visible_board(&store);

        undo_last(&mut store).expect("undo");
        assert_eq!(note(&store, &id).expect("note").text, "first");
        assert!(
            undone_apply_batches(store.connection())
                .expect("undone")
                .contains(&text_batch)
        );

        let outcome = redo_last(&mut store).expect("redo");
        assert_eq!(outcome.kind, BatchKind::Redo);
        assert_eq!(outcome.events, 1);
        assert_eq!(
            batch_kind(&store, &outcome.targets[0]),
            BatchKind::Undo,
            "a redo targets the undo batch"
        );
        assert_eq!(note(&store, &id).expect("note").text, "second");
        assert_eq!(visible_board(&store), applied_state);
        assert!(
            undone_apply_batches(store.connection())
                .expect("undone")
                .is_empty()
        );
    }

    #[test]
    fn many_undos_then_redos_walk_the_history() {
        let mut store = store("history-walk");
        let (add_batch, id) = add(&mut store, "v0");
        let mut batches = vec![add_batch];
        for text in ["v1", "v2", "v3"] {
            batches.push(apply(&mut store, &[set_text(&id, text)]));
        }
        let top = visible_board(&store);

        for expected in ["v2", "v1", "v0"] {
            undo_last(&mut store).expect("undo");
            assert_eq!(note(&store, &id).expect("note").text, expected);
        }
        undo_last(&mut store).expect("undo the add itself");
        assert!(visible_board(&store).notes.is_empty());
        assert!(note(&store, &id).expect("row").deleted_at_utc_ms.is_some());
        assert_eq!(
            undone_apply_batches(store.connection()).expect("undone"),
            batches
        );

        for _ in 0..4 {
            redo_last(&mut store).expect("redo");
        }
        assert_eq!(visible_board(&store), top);
        assert_eq!(note(&store, &id).expect("note").text, "v3");
        assert!(
            undone_apply_batches(store.connection())
                .expect("undone")
                .is_empty()
        );
    }

    #[test]
    fn a_later_batch_on_the_same_row_is_a_conflict() {
        let mut store = store("history-conflict");
        let (_, id) = add(&mut store, "first");
        let first_batch = apply(&mut store, &[set_text(&id, "second")]);
        let second_batch = apply(&mut store, &[set_text(&id, "third")]);
        let before = counts(&store);

        let error =
            undo_batches(&mut store, std::slice::from_ref(&first_batch)).expect_err("conflict");
        match &error {
            HistoryError::Conflict {
                batch_id,
                entity,
                entity_id,
            } => {
                assert_eq!(batch_id, &first_batch);
                assert_eq!(entity, "note");
                assert_eq!(entity_id, &id);
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        assert_eq!(note(&store, &id).expect("note").text, "third");
        assert_eq!(counts(&store), before, "a conflict writes nothing");
        assert!(matches!(
            undo_batches(&mut store, &[first_batch]).expect_err("still a conflict"),
            HistoryError::Conflict { .. }
        ));

        let outcome = undo_last(&mut store).expect("undo the newest batch");
        assert_eq!(outcome.targets, vec![second_batch]);
        assert_eq!(note(&store, &id).expect("note").text, "second");
    }

    #[test]
    fn undoing_a_turn_range_reverses_several_batches_in_order() {
        let mut store = store("history-range");
        let (_, id) = add(&mut store, "v0");
        let (_, other) = add(&mut store, "other");
        let first = apply(&mut store, &[set_text(&id, "v1")]);
        let second = apply(
            &mut store,
            &[set_text(&id, "v2"), set_priority(&other, Priority::High)],
        );

        let outcome =
            undo_batches(&mut store, &[first.clone(), second.clone()]).expect("undo range");
        assert_eq!(outcome.targets, vec![first.clone(), second.clone()]);
        assert_eq!(outcome.events, 3, "two text events and the priority event");
        assert_eq!(note(&store, &id).expect("note").text, "v0");
        assert_eq!(note(&store, &other).expect("other").priority, None);
        assert!(
            undone_apply_batches(store.connection())
                .expect("undone")
                .contains(&first)
        );

        // Redo the range: both batches are re-applied oldest first.
        redo_last(&mut store).expect("redo range");
        assert_eq!(note(&store, &id).expect("note").text, "v2");
        assert_eq!(
            note(&store, &other).expect("other").priority,
            Some(Priority::High)
        );
    }

    #[test]
    fn one_batch_that_changes_a_row_twice_undoes_cleanly() {
        let mut store = store("history-multi");
        let (_, id) = add(&mut store, "text");
        let before = note(&store, &id).expect("note");
        let batch = apply(
            &mut store,
            &[
                set_text(&id, "new"),
                set_priority(&id, Priority::High),
                Command::Complete { id: id.clone() },
            ],
        );
        let outcome = undo_last(&mut store).expect("undo");
        assert_eq!(outcome.targets, vec![batch]);
        assert_eq!(outcome.events, 3);
        let after = note(&store, &id).expect("note");
        assert_eq!(after.text, "text");
        assert_eq!(after.priority, None);
        assert_eq!(after.status, model::Status::Open);
        assert!(after.completed_at_utc_ms.is_none());
        assert!(after.version > before.version);
    }

    #[test]
    fn undo_again_after_a_redo_is_not_a_conflict() {
        let mut store = store("history-redo-undo");
        let (_, id) = add(&mut store, "first");
        apply(&mut store, &[set_text(&id, "second")]);
        undo_last(&mut store).expect("undo");
        assert_eq!(note(&store, &id).expect("note").text, "first");
        redo_last(&mut store).expect("redo");
        assert_eq!(note(&store, &id).expect("note").text, "second");
        undo_last(&mut store).expect("undo again");
        assert_eq!(note(&store, &id).expect("note").text, "first");
        assert_eq!(
            undone_apply_batches(store.connection())
                .expect("undone")
                .len(),
            1,
            "only the text batch is undone; the add is still applied"
        );
    }

    #[test]
    fn redo_after_another_batch_touched_the_row_is_a_conflict() {
        let mut store = store("history-redo-conflict");
        let (_, id) = add(&mut store, "first");
        apply(&mut store, &[set_text(&id, "second")]);
        undo_last(&mut store).expect("undo");
        apply(&mut store, &[set_priority(&id, Priority::Low)]);
        let before = counts(&store);

        let error = redo_last(&mut store).expect_err("conflict");
        assert!(matches!(error, HistoryError::Conflict { .. }));
        assert_eq!(note(&store, &id).expect("note").text, "first");
        assert_eq!(
            note(&store, &id).expect("note").priority,
            Some(Priority::Low)
        );
        assert_eq!(counts(&store), before);
    }

    #[test]
    fn undo_does_not_resurrect_a_row_another_batch_changed() {
        let mut store = store("history-no-resurrect");
        let (_, id) = add(&mut store, "task");
        let delete_batch = apply(&mut store, &[Command::Delete { id: id.clone() }]);
        apply(&mut store, &[Command::Restore { id: id.clone() }]);
        let before = counts(&store);

        let error = undo_batches(&mut store, &[delete_batch]).expect_err("conflict");
        assert!(matches!(error, HistoryError::Conflict { .. }));
        assert!(
            note(&store, &id).expect("note").is_active(),
            "the later restore is not clobbered"
        );
        assert_eq!(counts(&store), before);
    }

    #[test]
    fn undo_skips_system_batches() {
        let mut store = store("history-system");
        let (_, first) = add(&mut store, "first");
        let system_batch = apply_as(
            &mut store,
            Actor::System,
            &[set_priority(&first, Priority::High)],
        );
        assert_eq!(batch_kind(&store, &system_batch), BatchKind::Apply);
        let (human_batch, newest) = add(&mut store, "newest");

        let outcome = undo_last(&mut store).expect("undo");
        assert_eq!(
            outcome.targets,
            vec![human_batch],
            "the newest human batch is chosen, not the system one"
        );
        assert!(
            note(&store, &newest)
                .expect("note")
                .deleted_at_utc_ms
                .is_some(),
            "the human add was undone"
        );
        assert!(note(&store, &first).expect("note").is_active());
        assert_eq!(
            note(&store, &first).expect("note").priority,
            Some(Priority::High),
            "the system change is left alone"
        );
        assert!(
            !undone_apply_batches(store.connection())
                .expect("undone")
                .contains(&system_batch)
        );
    }

    #[test]
    fn a_system_only_history_has_nothing_to_undo() {
        let mut store = store("history-system-only");
        let applied = store
            .apply(&[add_note("reminder marker")], Actor::System, None)
            .expect("system add");
        assert!(matches!(
            undo_last(&mut store).expect_err("nothing to undo"),
            HistoryError::NothingToUndo
        ));
        assert_eq!(counts(&store), (1, 0, 1, 0));
        assert_eq!(applied.effects.len(), 1);
    }

    #[test]
    fn undo_batches_skips_already_undone_and_system_batches() {
        let mut store = store("history-skip");
        let (_, a) = add(&mut store, "a");
        let (_, b) = add(&mut store, "b");
        let text_a = apply(&mut store, &[set_text(&a, "a2")]);
        undo_last(&mut store).expect("undo a2");
        let system = apply_as(
            &mut store,
            Actor::System,
            &[set_priority(&b, Priority::Low)],
        );
        let (_, c) = add(&mut store, "c");
        let text_c = apply(&mut store, &[set_text(&c, "c2")]);

        let outcome = undo_batches(&mut store, &[text_a, system, text_c.clone()])
            .expect("undo the remaining batch");
        assert_eq!(outcome.targets, vec![text_c]);
        assert_eq!(outcome.events, 1);
        assert_eq!(note(&store, &c).expect("c").text, "c");
    }

    #[test]
    fn undo_batches_reports_unknown_and_non_apply_batches() {
        let mut store = store("history-bad-targets");
        assert!(matches!(
            undo_batches(&mut store, &["missing".to_owned()]).expect_err("missing"),
            HistoryError::BatchNotFound { .. }
        ));
        let (_, id) = add(&mut store, "task");
        undo_last(&mut store).expect("undo");
        let undo_batch = newest_undo_batch(&store);
        assert!(matches!(
            undo_batches(&mut store, &[undo_batch]).expect_err("not reversible"),
            HistoryError::NotReversible { .. }
        ));
        assert!(note(&store, &id).expect("note").deleted_at_utc_ms.is_some());
    }

    #[test]
    fn unknown_entities_are_skipped_and_bad_images_are_typed_errors() {
        let mut store = store("history-malformed");
        insert_batch(&store, "bwidget", "human", "apply", 1);
        store
            .connection()
            .execute(
                "INSERT INTO events
                     (batch_id, seq, at_utc_ms, actor, command, entity, entity_id, payload_json)
                 VALUES ('bwidget', 0, 1, 'human', 'widget', 'widget', 'w1', '{}')",
                [],
            )
            .expect("widget event");

        let outcome = undo_last(&mut store).expect("unknown entities are skipped");
        assert_eq!(outcome.skipped, 1);
        assert_eq!(outcome.events, 0);
        assert_eq!(outcome.targets, vec!["bwidget".to_owned()]);
        assert!(
            undone_apply_batches(store.connection())
                .expect("undone")
                .contains(&"bwidget".to_owned())
        );

        insert_batch(&store, "bbad", "human", "apply", 2);
        store
            .connection()
            .execute(
                "INSERT INTO events
                     (batch_id, seq, at_utc_ms, actor, command, entity, entity_id,
                      payload_json, before_json, after_json)
                 VALUES ('bbad', 0, 2, 'human', 'set_text', 'note', 'n1', '{}',
                         '{\"id\":\"n1\",\"text\":\"x\"}', '{\"id\":\"n1\"}')",
                [],
            )
            .expect("bad event");
        let before = counts(&store);
        let error = undo_last(&mut store).expect_err("malformed image");
        assert!(matches!(error, HistoryError::MalformedEvent { .. }));
        assert_eq!(
            counts(&store),
            before,
            "a malformed event rolls the whole reversal back"
        );
    }

    #[test]
    fn history_returns_every_event_for_a_note_in_order() {
        let mut store = store("history-list");
        let (add_batch, id) = add(&mut store, "first");
        let text_batch = apply(&mut store, &[set_text(&id, "second")]);
        let undo = undo_last(&mut store).expect("undo");

        let entries = history(store.connection(), &id).expect("history");
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].batch_id, add_batch);
        assert_eq!(entries[0].batch_kind, BatchKind::Apply);
        assert_eq!(entries[0].command, "add_note");
        assert!(entries[0].before.is_none());
        assert_eq!(text_of(&entries[0].after), Some("first"));
        assert_eq!(entries[1].batch_id, text_batch);
        assert_eq!(text_of(&entries[1].before), Some("first"));
        assert_eq!(text_of(&entries[1].after), Some("second"));
        assert_eq!(entries[2].batch_id, undo.batch_id);
        assert_eq!(entries[2].batch_kind, BatchKind::Undo);
        assert_eq!(entries[2].command, "set_text");
        assert_eq!(text_of(&entries[2].before), Some("second"));
        assert_eq!(text_of(&entries[2].after), Some("first"));
        assert!(
            entries
                .windows(2)
                .all(|pair| pair[0].event_id < pair[1].event_id),
            "ordered by events.id"
        );
        assert!(
            history(store.connection(), "missing")
                .expect("history")
                .is_empty()
        );
    }

    fn text_of(image: &Option<Value>) -> Option<&str> {
        image
            .as_ref()
            .and_then(|value| value.get("text"))
            .and_then(Value::as_str)
    }

    #[test]
    fn batches_between_lists_a_turn_range() {
        let mut store = store("history-between");
        let start = now_utc_ms() - 1;
        let (_, id) = add(&mut store, "task");
        let text_batch = apply(&mut store, &[set_text(&id, "next")]);
        let undo = undo_last(&mut store).expect("undo");
        let end = now_utc_ms() + 1;

        let batches = batches_between(store.connection(), start, end).expect("batches");
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].kind, BatchKind::Apply);
        assert_eq!(batches[0].actor, Actor::Human);
        assert_eq!(batches[0].events, 1);
        assert!(batches[0].targets.is_empty());
        assert_eq!(batches[1].id, text_batch);
        assert_eq!(batches[2].kind, BatchKind::Undo);
        assert_eq!(batches[2].id, undo.batch_id);
        assert_eq!(batches[2].targets, vec![text_batch]);
        assert!(
            batches[2]
                .label
                .as_deref()
                .is_some_and(|label| label.starts_with("undo "))
        );
        assert!(
            batches_between(store.connection(), end, start)
                .expect("empty range")
                .is_empty()
        );
    }

    #[test]
    fn board_revision_is_stable_and_tracks_changes() {
        let mut store = store("history-revision");
        let empty = board_revision(store.connection()).expect("revision");
        assert_eq!(
            empty,
            board_revision(store.connection()).expect("revision"),
            "an unchanged board has a stable revision"
        );
        let (_, id) = add(&mut store, "one");
        let after_add = board_revision(store.connection()).expect("revision");
        assert_ne!(after_add, empty);
        undo_last(&mut store).expect("undo");
        assert_eq!(
            board_revision(store.connection()).expect("revision"),
            empty,
            "an undone add leaves an empty board"
        );
        redo_last(&mut store).expect("redo");
        assert_eq!(note(&store, &id).expect("note").text, "one");
        let after_redo = board_revision(store.connection()).expect("revision");
        assert_ne!(after_redo, empty, "the redone note is on the board again");
        assert_eq!(
            after_redo,
            board_revision(store.connection()).expect("revision"),
            "the revision is stable between reads"
        );
        assert_ne!(
            after_redo, after_add,
            "a redo bumps the version, so the revision moves on"
        );
    }

    #[test]
    fn snapshot_restore_round_trips_and_checks_the_revision() {
        let mut store = store("history-snapshot");
        let (_, keep) = add(&mut store, "keep me");
        let (_, dropped) = add(&mut store, "drop me");
        store
            .apply(
                &[Command::CreateGroup {
                    name: "Home".to_owned(),
                    color: 1,
                }],
                Actor::Human,
                None,
            )
            .expect("group");
        let home = visible_board(&store).groups[0].id.clone();
        store
            .apply(
                &[Command::SetGroup {
                    id: keep.clone(),
                    group: Some("Home".to_owned()),
                }],
                Actor::Human,
                None,
            )
            .expect("set group");
        let link = store
            .apply(
                &[Command::Link {
                    from: keep.clone(),
                    to: dropped.clone(),
                    kind: None,
                }],
                Actor::Human,
                None,
            )
            .expect("link");
        let edge = link.new_ids.edges[0].clone();

        let image = capture_board(store.connection()).expect("capture");
        let revision = board_revision(store.connection()).expect("revision");
        let snapshot = record_snapshot(
            store.connection(),
            &image,
            "board_sql",
            Some(&link.batch_id),
        )
        .expect("record snapshot");

        // Change the board after the snapshot.
        apply(&mut store, &[set_text(&keep, "changed")]);
        apply(
            &mut store,
            &[Command::Delete {
                id: dropped.clone(),
            }],
        );
        let later = store
            .apply(
                &[Command::CreateGroup {
                    name: "Later".to_owned(),
                    color: 0,
                }],
                Actor::Human,
                None,
            )
            .expect("later group")
            .new_ids
            .groups[0]
            .clone();
        assert_ne!(
            board_revision(store.connection()).expect("revision"),
            revision
        );

        let error = restore_snapshot(&mut store, &snapshot, &revision).expect_err("stale revision");
        assert!(matches!(error, HistoryError::RevisionConflict { .. }));
        assert_eq!(note(&store, &keep).expect("keep").text, "changed");

        let current = board_revision(store.connection()).expect("revision");
        restore_snapshot(&mut store, &snapshot, &current).expect("restore");
        assert_eq!(capture_board(store.connection()).expect("capture"), image);
        assert_eq!(
            board_revision(store.connection()).expect("revision"),
            revision
        );
        assert_eq!(note(&store, &keep).expect("keep").text, "keep me");
        assert_eq!(note(&store, &keep).expect("keep").group, Some(home.clone()));
        assert!(note(&store, &dropped).expect("dropped").is_active());
        assert!(visible_board(&store).edge(&edge).is_some());
        assert!(
            model::load_group(store.connection(), &later)
                .expect("read group")
                .is_none(),
            "rows outside the snapshot are hard-deleted"
        );
        assert_eq!(
            counts(&store).3,
            2,
            "the recorded snapshot and the pre-restore image"
        );
        let pre: String = store
            .connection()
            .query_row(
                "SELECT data FROM snapshots
                  WHERE reason LIKE 'pre-restore %'
                  ORDER BY rowid DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .expect("pre-restore image");
        assert!(
            pre.contains("changed"),
            "the pre-restore image is the board as it was"
        );
        let restore_batch = batches_between(store.connection(), 0, now_utc_ms() + 1)
            .expect("batches")
            .into_iter()
            .find(|batch| {
                batch
                    .label
                    .as_deref()
                    .is_some_and(|label| label.starts_with("restore snapshot "))
            })
            .expect("restore batch");
        assert_eq!(restore_batch.kind, BatchKind::Redo);
        assert_eq!(restore_batch.targets, vec![link.batch_id.clone()]);
        assert_eq!(restore_batch.events, 0, "the snapshot is the audit record");
    }

    #[test]
    fn a_snapshot_without_a_batch_restores_without_a_batch() {
        let mut store = store("history-snapshot-loose");
        let (_, id) = add(&mut store, "one");
        let image = capture_board(store.connection()).expect("capture");
        let snapshot = record_snapshot(store.connection(), &image, "manual", None).expect("record");
        let before = counts(&store);
        apply(&mut store, &[set_text(&id, "two")]);
        let current = board_revision(store.connection()).expect("revision");
        restore_snapshot(&mut store, &snapshot, &current).expect("restore");
        assert_eq!(note(&store, &id).expect("note").text, "one");
        let after = counts(&store);
        assert_eq!(after.0, before.0 + 1, "the set_text batch only");
        assert_eq!(
            after.1, before.1,
            "no undo_targets without a snapshot batch"
        );
        assert_eq!(after.3, before.3 + 1, "one pre-restore image");
    }

    #[test]
    fn a_missing_snapshot_or_a_bad_one_is_a_typed_error() {
        let mut store = store("history-snapshot-bad");
        assert!(matches!(
            restore_snapshot(&mut store, "missing", "any").expect_err("missing snapshot"),
            HistoryError::SnapshotNotFound { .. }
        ));
        let bad = record_snapshot(
            store.connection(),
            &serde_json::json!({"notes": "not a list"}),
            "bad",
            None,
        )
        .expect("record bad snapshot");
        let revision = board_revision(store.connection()).expect("revision");
        let error = restore_snapshot(&mut store, &bad, &revision).expect_err("bad snapshot");
        assert!(matches!(error, HistoryError::MalformedSnapshot { .. }));
    }

    #[test]
    fn record_snapshot_rejects_a_blank_reason() {
        let store = store("history-snapshot-reason");
        let image = capture_board(store.connection()).expect("capture");
        assert!(matches!(
            record_snapshot(store.connection(), &image, "   ", None).expect_err("blank reason"),
            HistoryError::Invalid { .. }
        ));
    }

    #[test]
    fn audit_tables_only_grow() {
        let mut store = store("history-growth");
        let mut previous = counts(&store);
        let (_, id) = add(&mut store, "task");
        assert_counts_only_grow(&store, &mut previous);
        let text = apply(&mut store, &[set_text(&id, "next")]);
        assert_counts_only_grow(&store, &mut previous);
        apply(&mut store, &[Command::Complete { id: id.clone() }]);
        assert_counts_only_grow(&store, &mut previous);
        apply(&mut store, &[Command::Delete { id: id.clone() }]);
        assert_counts_only_grow(&store, &mut previous);
        undo_last(&mut store).expect("undo the delete");
        assert_counts_only_grow(&store, &mut previous);
        undo_last(&mut store).expect("undo the complete");
        assert_counts_only_grow(&store, &mut previous);
        redo_last(&mut store).expect("redo");
        assert_counts_only_grow(&store, &mut previous);

        let image = capture_board(store.connection()).expect("capture");
        let snapshot = record_snapshot(store.connection(), &image, "test", None).expect("record");
        assert_counts_only_grow(&store, &mut previous);
        let revision = board_revision(store.connection()).expect("revision");
        restore_snapshot(&mut store, &snapshot, &revision).expect("restore");
        assert_counts_only_grow(&store, &mut previous);

        let conflict = apply(&mut store, &[set_text(&id, "first try")]);
        apply(&mut store, &[set_text(&id, "second try")]);
        let before = counts(&store);
        let error = undo_batches(&mut store, &[conflict]).expect_err("conflict");
        assert!(matches!(error, HistoryError::Conflict { .. }));
        assert_eq!(counts(&store), before, "a conflict writes nothing");

        assert!(previous.0 > 0 && previous.1 > 0 && previous.2 > 0 && previous.3 > 0);
        store.check_integrity().expect("integrity check passes");
        let text_entry = history(store.connection(), &id).expect("history");
        assert!(text_entry.iter().any(|entry| entry.batch_id == text));
    }

    #[test]
    fn a_mixed_sequence_round_trips_through_undo_and_redo() {
        let mut store = store("history-mixed");
        let (_, parent) = add(&mut store, "parent");
        let mut states = vec![visible_board(&store)];

        let applied = store
            .apply(
                &[Command::AddNote {
                    text: "task".to_owned(),
                    group: Some("Home".to_owned()),
                    parent: None,
                    due: Some(TimeSpec {
                        local: "2026-12-01T09:00:00".to_owned(),
                        tz: "Europe/London".to_owned(),
                    }),
                    remind: None,
                    priority: Some(Priority::High),
                    source: "capture".to_owned(),
                }],
                Actor::Human,
                Some("capture"),
            )
            .expect("add");
        let id = applied.new_ids.notes[0].clone();
        states.push(visible_board(&store));

        for command in [
            set_text(&id, "renamed"),
            Command::Complete { id: id.clone() },
            Command::Snooze {
                id: id.clone(),
                until: TimeSpec {
                    local: "2026-12-02T07:30:00".to_owned(),
                    tz: "Europe/London".to_owned(),
                },
            },
            Command::SetParent {
                id: id.clone(),
                parent: Some(parent.clone()),
            },
            Command::Pin {
                id: parent.clone(),
                x: 12.0,
                y: 34.0,
            },
        ] {
            store
                .apply(&[command], Actor::Human, Some("test"))
                .expect("apply");
            states.push(visible_board(&store));
        }
        assert_eq!(states.len(), 7);

        for step in (0..6).rev() {
            undo_last(&mut store).expect("undo");
            assert_eq!(
                visible_board(&store),
                states[step],
                "undo walks back to step {step}"
            );
        }
        for (step, expected) in states.iter().enumerate().skip(1) {
            redo_last(&mut store).expect("redo");
            assert_eq!(
                visible_board(&store),
                *expected,
                "redo walks forward to step {step}"
            );
        }
    }

    #[test]
    fn undo_of_due_pin_and_unlink_restores_times_and_positions() {
        let mut store = store("history-undo-misc");
        let (_, id) = add(&mut store, "task");
        let (_, other) = add(&mut store, "other");

        apply(
            &mut store,
            &[Command::SetDue {
                id: id.clone(),
                due: Some(TimeSpec {
                    local: "2026-12-01T09:00:00".to_owned(),
                    tz: "Europe/London".to_owned(),
                }),
            }],
        );
        assert!(note(&store, &id).expect("note").due.is_some());
        undo_last(&mut store).expect("undo due");
        assert!(note(&store, &id).expect("note").due.is_none());

        apply(
            &mut store,
            &[Command::Pin {
                id: id.clone(),
                x: 10.0,
                y: 20.0,
            }],
        );
        assert_eq!(
            note(&store, &id).expect("note").layout,
            model::LayoutMode::Pinned
        );
        undo_last(&mut store).expect("undo pin");
        let unpinned = note(&store, &id).expect("note");
        assert_eq!(unpinned.layout, model::LayoutMode::Flow);
        assert!(unpinned.pos.is_none());

        let link = store
            .apply(
                &[Command::Link {
                    from: id.clone(),
                    to: other.clone(),
                    kind: None,
                }],
                Actor::Human,
                None,
            )
            .expect("link");
        let edge = link.new_ids.edges[0].clone();
        apply(
            &mut store,
            &[Command::Unlink {
                edge_id: edge.clone(),
            }],
        );
        assert!(visible_board(&store).edge(&edge).is_none());
        undo_last(&mut store).expect("undo unlink");
        assert!(visible_board(&store).edge(&edge).is_some());
    }

    #[test]
    fn a_snapshot_that_breaks_foreign_keys_rolls_back() {
        let mut store = store("history-snapshot-fk");
        let (_, id) = add(&mut store, "task");
        store
            .apply(
                &[Command::CreateGroup {
                    name: "Home".to_owned(),
                    color: 0,
                }],
                Actor::Human,
                None,
            )
            .expect("group");
        store
            .apply(
                &[Command::SetGroup {
                    id: id.clone(),
                    group: Some("Home".to_owned()),
                }],
                Actor::Human,
                None,
            )
            .expect("set group");
        let image = capture_board(store.connection()).expect("capture");

        let mut broken = image.clone();
        broken["groups"] = serde_json::json!([]);
        let snapshot =
            record_snapshot(store.connection(), &broken, "broken", None).expect("record snapshot");
        let revision = board_revision(store.connection()).expect("revision");
        let before = counts(&store);
        let error = restore_snapshot(&mut store, &snapshot, &revision).expect_err("foreign key");
        assert!(
            matches!(error, HistoryError::Restore { .. }),
            "expected a restore error, got {error:?}"
        );
        assert_eq!(
            counts(&store),
            before,
            "the pre-restore image rolled back with the rest"
        );
        assert_eq!(capture_board(store.connection()).expect("capture"), image);
    }

    /// Emulate board_sql (task 110): one apply batch with a whole-board event.
    fn insert_board_event(
        store: &Store,
        batch_id: &str,
        actor: &str,
        created_at: i64,
        before: &Value,
        after: &Value,
    ) {
        insert_batch(store, batch_id, actor, "apply", created_at);
        let payload = serde_json::json!({"command": "board_sql", "sql": "UPDATE notes SET text = 'after sql'"});
        store
            .connection()
            .execute(
                "INSERT INTO events
                     (batch_id, seq, at_utc_ms, actor, command, entity, entity_id,
                      payload_json, before_json, after_json)
                 VALUES (?1, 0, ?2, ?3, 'board_sql', 'board', ?4, ?5, ?6, ?7)",
                params![
                    batch_id,
                    created_at,
                    actor,
                    batch_id,
                    payload.to_string(),
                    serde_json::to_string(before).expect("before image"),
                    serde_json::to_string(after).expect("after image"),
                ],
            )
            .expect("board event");
    }

    #[test]
    fn undo_of_a_board_sql_batch_restores_its_snapshot() {
        let mut store = store("history-board");
        let (_, id) = add(&mut store, "task");
        let before_image = capture_board(store.connection()).expect("capture");
        let before_revision = board_revision(store.connection()).expect("revision");

        // The user SQL runs, then board_sql records one board event and a
        // snapshot tied to the batch that holds the pre-image.
        store
            .connection()
            .execute("UPDATE notes SET text = 'after sql' WHERE id = ?1", [&id])
            .expect("raw sql update");
        let after_image = capture_board(store.connection()).expect("capture");
        let after_revision = board_revision(store.connection()).expect("revision");
        let batch_id = "bsql-1".to_owned();
        insert_board_event(
            &store,
            &batch_id,
            "agent",
            now_utc_ms() + 10_000,
            &before_image,
            &after_image,
        );
        record_snapshot(
            store.connection(),
            &before_image,
            "board_sql",
            Some(&batch_id),
        )
        .expect("linked snapshot");

        // A later change makes the batch stale: report it, never clobber it.
        let priority_batch = apply(&mut store, &[set_priority(&id, Priority::Low)]);
        let before_try = counts(&store);
        let error = undo_last(&mut store).expect_err("conflict");
        match &error {
            HistoryError::Conflict {
                batch_id: conflicting,
                entity,
                ..
            } => {
                assert_eq!(conflicting, &batch_id);
                assert_eq!(entity, "board");
            }
            other => panic!("expected a board conflict, got {other:?}"),
        }
        assert_eq!(counts(&store), before_try);
        assert_eq!(note(&store, &id).expect("note").text, "after sql");

        // Undo the later change, then undo the SQL batch.
        undo_batches(&mut store, std::slice::from_ref(&priority_batch)).expect("undo priority");
        assert_eq!(note(&store, &id).expect("note").text, "after sql");
        let outcome = undo_last(&mut store).expect("undo the sql batch");
        assert_eq!(outcome.targets, vec![batch_id.clone()]);
        assert_eq!(outcome.events, 1, "one whole-board event");
        assert_eq!(note(&store, &id).expect("note").text, "task");
        assert_eq!(
            board_revision(store.connection()).expect("revision"),
            before_revision
        );
        assert_eq!(
            capture_board(store.connection()).expect("capture"),
            before_image
        );
        let undone = undone_apply_batches(store.connection()).expect("undone");
        assert!(undone.contains(&batch_id));
        assert_eq!(
            undone.len(),
            2,
            "the earlier priority undo is still pending"
        );
        let board_events: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE entity = 'board'",
                [],
                |row| row.get(0),
            )
            .expect("count board events");
        assert_eq!(board_events, 2, "the SQL event and the undo event");

        // Redo re-applies the SQL result from the event's after image.
        redo_last(&mut store).expect("redo the sql batch");
        assert_eq!(note(&store, &id).expect("note").text, "after sql");
        assert_eq!(
            board_revision(store.connection()).expect("revision"),
            after_revision
        );
        assert_eq!(
            capture_board(store.connection()).expect("capture"),
            after_image
        );
        assert!(
            !undone_apply_batches(store.connection())
                .expect("undone")
                .is_empty(),
            "the earlier priority undo is still pending"
        );
    }

    #[test]
    fn a_board_event_without_a_snapshot_uses_the_event_images() {
        let mut store = store("history-board-fallback");
        let (_, id) = add(&mut store, "task");
        let before_image = capture_board(store.connection()).expect("capture");
        let before_revision = board_revision(store.connection()).expect("revision");
        store
            .connection()
            .execute("UPDATE notes SET text = 'sql result' WHERE id = ?1", [&id])
            .expect("raw sql update");
        let after_image = capture_board(store.connection()).expect("capture");
        insert_board_event(
            &store,
            "bboard",
            "agent",
            now_utc_ms() + 10_000,
            &before_image,
            &after_image,
        );

        let outcome = undo_last(&mut store).expect("undo from the event images");
        assert_eq!(outcome.targets, vec!["bboard".to_owned()]);
        assert_eq!(note(&store, &id).expect("note").text, "task");
        assert_eq!(
            board_revision(store.connection()).expect("revision"),
            before_revision
        );

        redo_last(&mut store).expect("redo");
        assert_eq!(note(&store, &id).expect("note").text, "sql result");
        assert_eq!(
            capture_board(store.connection()).expect("capture"),
            after_image
        );
    }
}

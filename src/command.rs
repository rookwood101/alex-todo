//! The single typed mutation path (task 040).
//!
//! Every mutation the UI, the CLI and the agent make goes through [Command]
//! and [Store::apply]. One call is one transaction: it writes a batch row,
//! appends one event per changed entity with before/after JSON, and either
//! commits all commands or rolls everything back and reports the failing
//! command index.
//!
//! Nothing here renders or schedules. The event payloads are the contract that
//! history (task 050) and the MCP server (task 110) rely on.

use std::collections::HashSet;

use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{
    self, BatchId, Edge, EdgeId, Group, GroupId, LayoutMode, Note, NoteId, Priority, Status,
};
use crate::store::Store;
use crate::time::{TimeError, TimeSpec};

/// Who performed a batch. System batches (reminder markers) are skipped by
/// user undo.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Actor {
    Human,
    Agent,
    System,
}

impl Actor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
            Self::System => "system",
        }
    }
}

/// One mutation. The serde tag is the command name, so the CLI, MCP and chat
/// all speak the same JSON.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    AddNote {
        text: String,
        /// Group name; a new group is created when the name is unknown.
        #[serde(default)]
        group: Option<String>,
        #[serde(default)]
        parent: Option<NoteId>,
        #[serde(default)]
        due: Option<TimeSpec>,
        #[serde(default)]
        remind: Option<TimeSpec>,
        #[serde(default)]
        priority: Option<Priority>,
        #[serde(default = "default_source")]
        source: String,
    },
    SetText {
        id: NoteId,
        text: String,
    },
    /// Move a note into a group (by name) or back to the Inbox with null.
    SetGroup {
        id: NoteId,
        #[serde(default)]
        group: Option<String>,
    },
    SetParent {
        id: NoteId,
        #[serde(default)]
        parent: Option<NoteId>,
    },
    SetPriority {
        id: NoteId,
        #[serde(default)]
        priority: Option<Priority>,
    },
    SetDue {
        id: NoteId,
        #[serde(default)]
        due: Option<TimeSpec>,
    },
    SetRemind {
        id: NoteId,
        #[serde(default)]
        remind: Option<TimeSpec>,
    },
    Snooze {
        id: NoteId,
        until: TimeSpec,
    },
    Unsnooze {
        id: NoteId,
    },
    Complete {
        id: NoteId,
    },
    Reopen {
        id: NoteId,
    },
    Delete {
        id: NoteId,
    },
    Restore {
        id: NoteId,
    },
    Pin {
        id: NoteId,
        x: f32,
        y: f32,
    },
    Unpin {
        id: NoteId,
    },
    CreateGroup {
        name: String,
        #[serde(default)]
        color: i64,
    },
    RenameGroup {
        id: GroupId,
        name: String,
    },
    Link {
        from: NoteId,
        to: NoteId,
        #[serde(default)]
        kind: Option<String>,
    },
    Unlink {
        edge_id: EdgeId,
    },
}

fn default_source() -> String {
    "human".to_owned()
}

impl Command {
    /// Stable name used in events and effects.
    pub fn name(&self) -> &'static str {
        match self {
            Self::AddNote { .. } => "add_note",
            Self::SetText { .. } => "set_text",
            Self::SetGroup { .. } => "set_group",
            Self::SetParent { .. } => "set_parent",
            Self::SetPriority { .. } => "set_priority",
            Self::SetDue { .. } => "set_due",
            Self::SetRemind { .. } => "set_remind",
            Self::Snooze { .. } => "snooze",
            Self::Unsnooze { .. } => "unsnooze",
            Self::Complete { .. } => "complete",
            Self::Reopen { .. } => "reopen",
            Self::Delete { .. } => "delete",
            Self::Restore { .. } => "restore",
            Self::Pin { .. } => "pin",
            Self::Unpin { .. } => "unpin",
            Self::CreateGroup { .. } => "create_group",
            Self::RenameGroup { .. } => "rename_group",
            Self::Link { .. } => "link",
            Self::Unlink { .. } => "unlink",
        }
    }
}

/// One command's summary, in the order the commands were applied.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Effect {
    pub command: String,
    pub entity: String,
    pub entity_id: String,
    /// Number of rows written for this command (a subtree delete is many).
    pub affected: usize,
}

/// Ids minted by this batch, for the caller to focus or render.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NewIds {
    pub notes: Vec<NoteId>,
    pub groups: Vec<GroupId>,
    pub edges: Vec<EdgeId>,
}

/// The result of a successful [Store::apply].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Applied {
    pub batch_id: BatchId,
    pub effects: Vec<Effect>,
    pub new_ids: NewIds,
}

/// Errors from validation or execution. The index of the failing command is
/// attached by [Store::apply].
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("command {index}: {source}")]
    At {
        index: usize,
        #[source]
        source: Box<CommandError>,
    },

    #[error("invalid input: {message}")]
    Invalid { message: String },

    #[error("{entity} {id:?} not found")]
    NotFound { entity: &'static str, id: String },

    #[error("group {name:?} already exists")]
    DuplicateGroup { name: String },

    #[error("a note cannot be its own ancestor")]
    Cycle,

    #[error("there are no commands to apply")]
    Empty,

    #[error("the board changed since this command was read")]
    Conflict,

    #[error(transparent)]
    Model(#[from] model::ModelError),

    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),

    #[error("cannot encode a row: {0}")]
    Json(#[from] serde_json::Error),
}

impl CommandError {
    fn at(self, index: usize) -> Self {
        match self {
            Self::At { .. } => self,
            other => Self::At {
                index,
                source: Box::new(other),
            },
        }
    }

    /// The failing command index, when known.
    pub fn index(&self) -> Option<usize> {
        match self {
            Self::At { index, .. } => Some(*index),
            _ => None,
        }
    }
}

/// Reverses a command for undo (used by task 050's tests and the CLI).
pub fn inverse_hint(command: &Command) -> Option<&'static str> {
    Some(match command {
        Command::AddNote { .. } => "delete",
        Command::Delete { .. } => "restore",
        Command::Complete { .. } => "reopen",
        Command::Reopen { .. } => "complete",
        Command::Snooze { .. } | Command::Unsnooze { .. } => "set_snooze",
        Command::Link { .. } => "unlink",
        _ => return None,
    })
}

impl Store {
    /// Apply commands as one batch and one transaction.
    pub fn apply(
        &mut self,
        commands: &[Command],
        actor: Actor,
        label: Option<&str>,
    ) -> Result<Applied, CommandError> {
        self.apply_with(commands, actor, None, label)
    }

    /// Apply commands with an optional actor reference (Pi turn or tool call).
    pub fn apply_with(
        &mut self,
        commands: &[Command],
        actor: Actor,
        actor_ref: Option<&str>,
        label: Option<&str>,
    ) -> Result<Applied, CommandError> {
        if commands.is_empty() {
            return Err(CommandError::Empty);
        }

        let now = crate::store::now_utc_ms();
        let batch_id = new_id();
        let mut new_ids = NewIds::default();
        let mut effects = Vec::with_capacity(commands.len());

        {
            let tx = self
                .connection_mut()
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute(
                "INSERT INTO batches (id, created_at_utc_ms, actor, actor_ref, label, kind)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'apply')",
                params![batch_id, now, actor.as_str(), actor_ref, label],
            )?;

            let mut ctx = Ctx {
                now,
                batch_id: &batch_id,
                actor,
                seq: 0,
                new_ids: &mut new_ids,
            };

            for (index, command) in commands.iter().enumerate() {
                let effect = apply_one(&mut ctx, &tx, command).map_err(|error| error.at(index))?;
                effects.push(effect);
            }

            tx.commit()?;
        }

        Ok(Applied {
            batch_id,
            effects,
            new_ids,
        })
    }
}

/// Per-batch state shared by the handlers.
struct Ctx<'a> {
    now: i64,
    batch_id: &'a str,
    actor: Actor,
    seq: i64,
    new_ids: &'a mut NewIds,
}

impl Ctx<'_> {
    fn event(
        &mut self,
        tx: &Transaction<'_>,
        command: &Command,
        entity: &str,
        entity_id: &str,
        before: Option<Value>,
        after: Option<Value>,
    ) -> Result<(), CommandError> {
        let payload = serde_json::to_string(command)?;
        let before = before
            .map(|value| serde_json::to_string(&value))
            .transpose()?;
        let after = after
            .map(|value| serde_json::to_string(&value))
            .transpose()?;
        tx.execute(
            "INSERT INTO events
                 (batch_id, seq, at_utc_ms, actor, command, entity, entity_id,
                  payload_json, before_json, after_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                self.batch_id,
                self.seq,
                self.now,
                self.actor.as_str(),
                command.name(),
                entity,
                entity_id,
                payload,
                before,
                after
            ],
        )?;
        self.seq += 1;
        Ok(())
    }
}

fn effect_for(command: &Command, entity: &'static str, id: &str, affected: usize) -> Effect {
    Effect {
        command: command.name().to_owned(),
        entity: entity.to_owned(),
        entity_id: id.to_owned(),
        affected,
    }
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn invalid(message: impl Into<String>) -> CommandError {
    CommandError::Invalid {
        message: message.into(),
    }
}

fn time_error(error: TimeError) -> CommandError {
    invalid(format!("invalid time: {error}"))
}

fn to_value<T: Serialize>(value: &T) -> Result<Value, CommandError> {
    Ok(serde_json::to_value(value)?)
}

fn load_note_any(tx: &Transaction<'_>, id: &str) -> Result<Option<Note>, CommandError> {
    Ok(model::load_note(tx, id)?)
}

fn ensure_active_note(tx: &Transaction<'_>, id: &str) -> Result<Note, CommandError> {
    let note = load_note_any(tx, id)?.ok_or_else(|| CommandError::NotFound {
        entity: "note",
        id: id.to_owned(),
    })?;
    if !note.is_active() {
        return Err(CommandError::NotFound {
            entity: "note",
            id: id.to_owned(),
        });
    }
    Ok(note)
}

fn reload_note(tx: &Transaction<'_>, id: &str) -> Result<Note, CommandError> {
    load_note_any(tx, id)?.ok_or_else(|| CommandError::NotFound {
        entity: "note",
        id: id.to_owned(),
    })
}

fn emit_note(
    ctx: &mut Ctx<'_>,
    tx: &Transaction<'_>,
    command: &Command,
    before: &Note,
    after: &Note,
) -> Result<(), CommandError> {
    ctx.event(
        tx,
        command,
        "note",
        &after.id,
        Some(to_value(before)?),
        Some(to_value(after)?),
    )
}

fn load_edge_any(tx: &Transaction<'_>, id: &str) -> Result<Option<Edge>, CommandError> {
    Ok(model::load_edge(tx, id)?)
}

fn next_group_sort(tx: &Transaction<'_>) -> Result<i64, CommandError> {
    let sort = tx.query_row(
        "SELECT COALESCE(MAX(sort_order), -1) + 1 FROM groups WHERE deleted_at_utc_ms IS NULL",
        [],
        |row| row.get(0),
    )?;
    Ok(sort)
}

fn next_note_sort(
    tx: &Transaction<'_>,
    parent: Option<&str>,
    group: Option<&str>,
) -> Result<i64, CommandError> {
    let sort = tx.query_row(
        "SELECT COALESCE(MAX(sort_order), -1) + 1 FROM notes
          WHERE deleted_at_utc_ms IS NULL AND parent_id IS ?1 AND group_id IS ?2",
        params![parent, group],
        |row| row.get(0),
    )?;
    Ok(sort)
}

/// Finds an active group by name or creates one, returning its id.
fn resolve_or_create_group(
    ctx: &mut Ctx<'_>,
    tx: &Transaction<'_>,
    command: &Command,
    name: &str,
) -> Result<GroupId, CommandError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(invalid("group name is empty"));
    }
    if let Some(id) = model::find_group_by_name(tx, name)? {
        return Ok(id);
    }

    let id = new_id();
    let sort = next_group_sort(tx)?;
    tx.execute(
        "INSERT INTO groups (id, name, color, sort_order, created_at_utc_ms, updated_at_utc_ms)
         VALUES (?1, ?2, 0, ?3, ?4, ?4)",
        params![id, name, sort, ctx.now],
    )?;
    let group = model::load_group(tx, &id)?.ok_or_else(|| CommandError::NotFound {
        entity: "group",
        id: id.clone(),
    })?;
    ctx.event(tx, command, "group", &id, None, Some(to_value(&group)?))?;
    ctx.new_ids.groups.push(id.clone());
    Ok(id)
}

fn ensure_no_cycle(
    tx: &Transaction<'_>,
    note_id: &str,
    parent_id: &str,
) -> Result<(), CommandError> {
    let creates_cycle: i64 = tx.query_row(
        "WITH RECURSIVE chain(id, depth) AS (
             SELECT id, 0 FROM notes WHERE id = ?1
             UNION ALL
             SELECT n.parent_id, chain.depth + 1
               FROM notes n JOIN chain ON n.id = chain.id
              WHERE n.parent_id IS NOT NULL AND chain.depth < 64
         )
         SELECT EXISTS(SELECT 1 FROM chain WHERE id = ?2)",
        params![parent_id, note_id],
        |row| row.get(0),
    )?;
    if creates_cycle != 0 {
        return Err(CommandError::Cycle);
    }
    Ok(())
}

/// Ids of a note's subtree, active or deleted depending on the flag.
fn collect_subtree(
    tx: &Transaction<'_>,
    root: &str,
    include_deleted: bool,
) -> Result<Vec<String>, CommandError> {
    let filter = if include_deleted {
        "deleted_at_utc_ms IS NOT NULL"
    } else {
        "deleted_at_utc_ms IS NULL"
    };
    let sql = format!(
        "WITH RECURSIVE subtree(id) AS (
             SELECT id FROM notes WHERE id = ?1 AND {filter}
             UNION
             SELECT n.id FROM notes n JOIN subtree s ON n.parent_id = s.id WHERE n.{filter}
         )
         SELECT id FROM subtree"
    );
    let mut statement = tx.prepare(&sql)?;
    let rows = statement.query_map([root], |row| row.get::<_, String>(0))?;
    let mut ids = Vec::new();
    for row in rows {
        ids.push(row?);
    }
    Ok(ids)
}

fn touching_edge_ids(
    tx: &Transaction<'_>,
    note_ids: &[String],
    deleted_at: Option<i64>,
) -> Result<Vec<String>, CommandError> {
    let mut ids = HashSet::new();
    let mut statement = tx.prepare(
        "SELECT id FROM edges
          WHERE deleted_at_utc_ms IS ?1 AND (\"from\" = ?2 OR \"to\" = ?2)",
    )?;
    for note_id in note_ids {
        let rows =
            statement.query_map(params![deleted_at, note_id], |row| row.get::<_, String>(0))?;
        for row in rows {
            ids.insert(row?);
        }
    }
    let mut ids: Vec<String> = ids.into_iter().collect();
    ids.sort();
    Ok(ids)
}

fn apply_one(
    ctx: &mut Ctx<'_>,
    tx: &Transaction<'_>,
    command: &Command,
) -> Result<Effect, CommandError> {
    match command {
        Command::AddNote { .. } => add_note(ctx, tx, command),
        Command::SetText { id, text } => {
            let before = ensure_active_note(tx, id)?;
            tx.execute(
                "UPDATE notes SET text = ?1 WHERE id = ?2 AND deleted_at_utc_ms IS NULL",
                params![text.trim(), id],
            )?;
            let after = reload_note(tx, id)?;
            emit_note(ctx, tx, command, &before, &after)?;
            Ok(effect_for(command, "note", id, 1))
        }
        Command::SetGroup { id, group } => {
            let before = ensure_active_note(tx, id)?;
            let group_id = match group {
                Some(name) => {
                    let name = name.trim();
                    if name.is_empty() {
                        return Err(invalid("group name is empty"));
                    }
                    Some(model::find_group_by_name(tx, name)?.ok_or_else(|| {
                        CommandError::NotFound {
                            entity: "group",
                            id: name.to_owned(),
                        }
                    })?)
                }
                None => None,
            };
            tx.execute(
                "UPDATE notes SET group_id = ?1 WHERE id = ?2 AND deleted_at_utc_ms IS NULL",
                params![group_id, id],
            )?;
            let after = reload_note(tx, id)?;
            emit_note(ctx, tx, command, &before, &after)?;
            Ok(effect_for(command, "note", id, 1))
        }
        Command::SetParent { id, parent } => {
            let before = ensure_active_note(tx, id)?;
            if let Some(parent_id) = parent {
                if parent_id == id {
                    return Err(CommandError::Cycle);
                }
                ensure_active_note(tx, parent_id)?;
                ensure_no_cycle(tx, id, parent_id)?;
            }
            // A child cannot stay pinned: pinned positions belong to roots.
            let unpin = parent.is_some() && before.layout == LayoutMode::Pinned;
            if unpin {
                tx.execute(
                    "UPDATE notes SET parent_id = ?1, layout_mode = 'flow', pos_x = NULL, pos_y = NULL
                      WHERE id = ?2 AND deleted_at_utc_ms IS NULL",
                    params![parent, id],
                )?;
            } else {
                tx.execute(
                    "UPDATE notes SET parent_id = ?1 WHERE id = ?2 AND deleted_at_utc_ms IS NULL",
                    params![parent, id],
                )?;
            }
            let after = reload_note(tx, id)?;
            emit_note(ctx, tx, command, &before, &after)?;
            Ok(effect_for(command, "note", id, 1))
        }
        Command::SetPriority { id, priority } => {
            let before = ensure_active_note(tx, id)?;
            tx.execute(
                "UPDATE notes SET priority = ?1 WHERE id = ?2 AND deleted_at_utc_ms IS NULL",
                params![priority.map(Priority::as_str), id],
            )?;
            let after = reload_note(tx, id)?;
            emit_note(ctx, tx, command, &before, &after)?;
            Ok(effect_for(command, "note", id, 1))
        }
        Command::SetDue { id, due } => {
            let before = ensure_active_note(tx, id)?;
            let parts = due
                .as_ref()
                .map(TimeSpec::to_sql_parts)
                .transpose()
                .map_err(time_error)?;
            let (local, tz, utc_ms) = split_parts(parts);
            tx.execute(
                "UPDATE notes SET due_local = ?1, due_tz = ?2, due_utc_ms = ?3
                  WHERE id = ?4 AND deleted_at_utc_ms IS NULL",
                params![local, tz, utc_ms, id],
            )?;
            let after = reload_note(tx, id)?;
            emit_note(ctx, tx, command, &before, &after)?;
            Ok(effect_for(command, "note", id, 1))
        }
        Command::SetRemind { id, remind } => {
            let before = ensure_active_note(tx, id)?;
            let parts = remind
                .as_ref()
                .map(TimeSpec::to_sql_parts)
                .transpose()
                .map_err(time_error)?;
            let (local, tz, utc_ms) = split_parts(parts);
            // Clearing a reminder must also clear the fired marker: the schema
            // refuses a marker without a reminder.
            tx.execute(
                "UPDATE notes
                    SET remind_local = ?1, remind_tz = ?2, remind_utc_ms = ?3,
                        reminded_at_utc_ms = CASE WHEN ?3 IS NULL THEN NULL ELSE reminded_at_utc_ms END
                  WHERE id = ?4 AND deleted_at_utc_ms IS NULL",
                params![local, tz, utc_ms, id],
            )?;
            let after = reload_note(tx, id)?;
            emit_note(ctx, tx, command, &before, &after)?;
            Ok(effect_for(command, "note", id, 1))
        }
        Command::Snooze { id, until } => {
            let before = ensure_active_note(tx, id)?;
            let parts = until.to_sql_parts().map_err(time_error)?;
            tx.execute(
                "UPDATE notes SET snooze_local = ?1, snooze_tz = ?2, snooze_utc_ms = ?3
                  WHERE id = ?4 AND deleted_at_utc_ms IS NULL",
                params![parts.local, parts.tz, parts.utc_ms, id],
            )?;
            let after = reload_note(tx, id)?;
            emit_note(ctx, tx, command, &before, &after)?;
            Ok(effect_for(command, "note", id, 1))
        }
        Command::Unsnooze { id } => {
            let before = ensure_active_note(tx, id)?;
            tx.execute(
                "UPDATE notes SET snooze_local = NULL, snooze_tz = NULL, snooze_utc_ms = NULL
                  WHERE id = ?1 AND deleted_at_utc_ms IS NULL",
                [id],
            )?;
            let after = reload_note(tx, id)?;
            emit_note(ctx, tx, command, &before, &after)?;
            Ok(effect_for(command, "note", id, 1))
        }
        Command::Complete { id } => {
            let before = ensure_active_note(tx, id)?;
            tx.execute(
                "UPDATE notes SET status = 'done', completed_at_utc_ms = ?1
                  WHERE id = ?2 AND deleted_at_utc_ms IS NULL",
                params![ctx.now, id],
            )?;
            let after = reload_note(tx, id)?;
            emit_note(ctx, tx, command, &before, &after)?;
            Ok(effect_for(command, "note", id, 1))
        }
        Command::Reopen { id } => {
            let before = ensure_active_note(tx, id)?;
            tx.execute(
                "UPDATE notes
                    SET status = 'open', completed_at_utc_ms = NULL, reminded_at_utc_ms = NULL
                  WHERE id = ?1 AND deleted_at_utc_ms IS NULL",
                [id],
            )?;
            let after = reload_note(tx, id)?;
            emit_note(ctx, tx, command, &before, &after)?;
            Ok(effect_for(command, "note", id, 1))
        }
        Command::Delete { id } => delete_note(ctx, tx, command, id),
        Command::Restore { id } => restore_note(ctx, tx, command, id),
        Command::Pin { id, x, y } => {
            let before = ensure_active_note(tx, id)?;
            if before.parent.is_some() {
                return Err(invalid("only root notes can be pinned"));
            }
            if !x.is_finite() || !y.is_finite() {
                return Err(invalid("pin position must be finite"));
            }
            tx.execute(
                "UPDATE notes SET layout_mode = 'pinned', pos_x = ?1, pos_y = ?2
                  WHERE id = ?3 AND deleted_at_utc_ms IS NULL",
                params![f64::from(*x), f64::from(*y), id],
            )?;
            let after = reload_note(tx, id)?;
            emit_note(ctx, tx, command, &before, &after)?;
            Ok(effect_for(command, "note", id, 1))
        }
        Command::Unpin { id } => {
            let before = ensure_active_note(tx, id)?;
            tx.execute(
                "UPDATE notes SET layout_mode = 'flow', pos_x = NULL, pos_y = NULL
                  WHERE id = ?1 AND deleted_at_utc_ms IS NULL",
                [id],
            )?;
            let after = reload_note(tx, id)?;
            emit_note(ctx, tx, command, &before, &after)?;
            Ok(effect_for(command, "note", id, 1))
        }
        Command::CreateGroup { name, color } => create_group(ctx, tx, command, name, *color),
        Command::RenameGroup { id, name } => rename_group(ctx, tx, command, id, name),
        Command::Link { from, to, kind } => link(ctx, tx, command, from, to, kind.as_deref()),
        Command::Unlink { edge_id } => unlink(ctx, tx, command, edge_id),
    }
}

fn split_parts(
    parts: Option<crate::time::TimeParts>,
) -> (Option<String>, Option<String>, Option<i64>) {
    match parts {
        Some(parts) => (Some(parts.local), Some(parts.tz), Some(parts.utc_ms)),
        None => (None, None, None),
    }
}

fn add_note(
    ctx: &mut Ctx<'_>,
    tx: &Transaction<'_>,
    command: &Command,
) -> Result<Effect, CommandError> {
    let Command::AddNote {
        text,
        group,
        parent,
        due,
        remind,
        priority,
        source,
    } = command
    else {
        return Err(invalid("internal: add_note received another command"));
    };

    let group_id = match group {
        Some(name) => Some(resolve_or_create_group(ctx, tx, command, name)?),
        None => None,
    };
    if let Some(parent_id) = parent {
        ensure_active_note(tx, parent_id)?;
    }

    let due_parts = due
        .as_ref()
        .map(TimeSpec::to_sql_parts)
        .transpose()
        .map_err(time_error)?;
    let remind_parts = remind
        .as_ref()
        .map(TimeSpec::to_sql_parts)
        .transpose()
        .map_err(time_error)?;
    let (due_local, due_tz, due_utc_ms) = split_parts(due_parts);
    let (remind_local, remind_tz, remind_utc_ms) = split_parts(remind_parts);
    let source = source.trim();
    if source.is_empty() {
        return Err(invalid("source is empty"));
    }

    let id = new_id();
    let sort = next_note_sort(tx, parent.as_deref(), group_id.as_deref())?;
    tx.execute(
        "INSERT INTO notes
             (id, text, group_id, parent_id, layout_mode, sort_order, color, priority,
              status, source, created_at_utc_ms, updated_at_utc_ms,
              due_local, due_tz, due_utc_ms, remind_local, remind_tz, remind_utc_ms)
         VALUES (?1, ?2, ?3, ?4, 'flow', ?5, 0, ?6, 'open', ?7, ?8, ?8,
                 ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            id,
            text.trim(),
            group_id,
            parent,
            sort,
            priority.map(Priority::as_str),
            source,
            ctx.now,
            due_local,
            due_tz,
            due_utc_ms,
            remind_local,
            remind_tz,
            remind_utc_ms
        ],
    )?;

    let note = reload_note(tx, &id)?;
    ctx.event(tx, command, "note", &id, None, Some(to_value(&note)?))?;
    ctx.new_ids.notes.push(id.clone());
    Ok(effect_for(command, "note", &id, 1))
}

fn delete_note(
    ctx: &mut Ctx<'_>,
    tx: &Transaction<'_>,
    command: &Command,
    id: &str,
) -> Result<Effect, CommandError> {
    let root = ensure_active_note(tx, id)?;
    let _ = root;
    let note_ids = collect_subtree(tx, id, false)?;
    let edge_ids = touching_edge_ids(tx, &note_ids, None)?;
    let mut affected = 0;

    for note_id in &note_ids {
        let before = reload_note(tx, note_id)?;
        tx.execute(
            "UPDATE notes SET deleted_at_utc_ms = ?1
              WHERE id = ?2 AND deleted_at_utc_ms IS NULL",
            params![ctx.now, note_id],
        )?;
        let after = reload_note(tx, note_id)?;
        emit_note(ctx, tx, command, &before, &after)?;
        affected += 1;
    }

    for edge_id in &edge_ids {
        let before = load_edge_any(tx, edge_id)?.ok_or_else(|| CommandError::NotFound {
            entity: "edge",
            id: edge_id.clone(),
        })?;
        tx.execute(
            "UPDATE edges SET deleted_at_utc_ms = ?1
              WHERE id = ?2 AND deleted_at_utc_ms IS NULL",
            params![ctx.now, edge_id],
        )?;
        let after = load_edge_any(tx, edge_id)?.ok_or_else(|| CommandError::NotFound {
            entity: "edge",
            id: edge_id.clone(),
        })?;
        ctx.event(
            tx,
            command,
            "edge",
            edge_id,
            Some(to_value(&before)?),
            Some(to_value(&after)?),
        )?;
        affected += 1;
    }

    Ok(effect_for(command, "note", id, affected))
}

fn restore_note(
    ctx: &mut Ctx<'_>,
    tx: &Transaction<'_>,
    command: &Command,
    id: &str,
) -> Result<Effect, CommandError> {
    let root = load_note_any(tx, id)?.ok_or_else(|| CommandError::NotFound {
        entity: "note",
        id: id.to_owned(),
    })?;
    let Some(deleted_at) = root.deleted_at_utc_ms else {
        // Already active: nothing to restore, and no event to undo.
        return Ok(effect_for(command, "note", id, 0));
    };

    let note_ids = collect_subtree(tx, id, true)?;
    let edge_ids = touching_edge_ids(tx, &note_ids, Some(deleted_at))?;
    let mut affected = 0;

    for note_id in &note_ids {
        let before = reload_note(tx, note_id)?;
        tx.execute(
            "UPDATE notes SET deleted_at_utc_ms = NULL
              WHERE id = ?1 AND deleted_at_utc_ms IS NOT NULL",
            [note_id],
        )?;
        let after = reload_note(tx, note_id)?;
        emit_note(ctx, tx, command, &before, &after)?;
        affected += 1;
    }

    for edge_id in &edge_ids {
        let before = load_edge_any(tx, edge_id)?.ok_or_else(|| CommandError::NotFound {
            entity: "edge",
            id: edge_id.clone(),
        })?;
        tx.execute(
            "UPDATE edges SET deleted_at_utc_ms = NULL
              WHERE id = ?1 AND deleted_at_utc_ms IS NOT NULL",
            [edge_id],
        )?;
        let after = load_edge_any(tx, edge_id)?.ok_or_else(|| CommandError::NotFound {
            entity: "edge",
            id: edge_id.clone(),
        })?;
        ctx.event(
            tx,
            command,
            "edge",
            edge_id,
            Some(to_value(&before)?),
            Some(to_value(&after)?),
        )?;
        affected += 1;
    }

    Ok(effect_for(command, "note", id, affected))
}

fn create_group(
    ctx: &mut Ctx<'_>,
    tx: &Transaction<'_>,
    command: &Command,
    name: &str,
    color: i64,
) -> Result<Effect, CommandError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(invalid("group name is empty"));
    }
    if color < 0 {
        return Err(invalid("group colour must not be negative"));
    }
    if model::find_group_by_name(tx, name)?.is_some() {
        return Err(CommandError::DuplicateGroup {
            name: name.to_owned(),
        });
    }
    let id = new_id();
    let sort = next_group_sort(tx)?;
    tx.execute(
        "INSERT INTO groups (id, name, color, sort_order, created_at_utc_ms, updated_at_utc_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        params![id, name, color, sort, ctx.now],
    )?;
    let group = model::load_group(tx, &id)?.ok_or_else(|| CommandError::NotFound {
        entity: "group",
        id: id.clone(),
    })?;
    ctx.event(tx, command, "group", &id, None, Some(to_value(&group)?))?;
    ctx.new_ids.groups.push(id.clone());
    Ok(effect_for(command, "group", &id, 1))
}

fn rename_group(
    ctx: &mut Ctx<'_>,
    tx: &Transaction<'_>,
    command: &Command,
    id: &str,
    name: &str,
) -> Result<Effect, CommandError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(invalid("group name is empty"));
    }
    let before = model::load_group(tx, id)?.ok_or_else(|| CommandError::NotFound {
        entity: "group",
        id: id.to_owned(),
    })?;
    if before.deleted_at_utc_ms.is_some() {
        return Err(CommandError::NotFound {
            entity: "group",
            id: id.to_owned(),
        });
    }
    let duplicate: i64 = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM groups
                        WHERE deleted_at_utc_ms IS NULL
                          AND lower(name) = lower(?1) AND id <> ?2)",
        params![name, id],
        |row| row.get(0),
    )?;
    if duplicate != 0 {
        return Err(CommandError::DuplicateGroup {
            name: name.to_owned(),
        });
    }
    tx.execute(
        "UPDATE groups SET name = ?1 WHERE id = ?2",
        params![name, id],
    )?;
    let after = model::load_group(tx, id)?.ok_or_else(|| CommandError::NotFound {
        entity: "group",
        id: id.to_owned(),
    })?;
    ctx.event(
        tx,
        command,
        "group",
        id,
        Some(to_value(&before)?),
        Some(to_value(&after)?),
    )?;
    Ok(effect_for(command, "group", id, 1))
}

fn link(
    ctx: &mut Ctx<'_>,
    tx: &Transaction<'_>,
    command: &Command,
    from: &str,
    to: &str,
    kind: Option<&str>,
) -> Result<Effect, CommandError> {
    if from == to {
        return Err(invalid("an edge cannot connect a note to itself"));
    }
    ensure_active_note(tx, from)?;
    ensure_active_note(tx, to)?;
    let kind = kind.unwrap_or("link").trim();
    if kind.is_empty() {
        return Err(invalid("edge kind is empty"));
    }
    let duplicate: i64 = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM edges
                        WHERE deleted_at_utc_ms IS NULL
                          AND \"from\" = ?1 AND \"to\" = ?2 AND kind = ?3)",
        params![from, to, kind],
        |row| row.get(0),
    )?;
    if duplicate != 0 {
        return Err(invalid("that link already exists"));
    }

    let id = new_id();
    tx.execute(
        "INSERT INTO edges (id, \"from\", \"to\", kind, created_at_utc_ms, updated_at_utc_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        params![id, from, to, kind, ctx.now],
    )?;
    let edge = load_edge_any(tx, &id)?.ok_or_else(|| CommandError::NotFound {
        entity: "edge",
        id: id.clone(),
    })?;
    ctx.event(tx, command, "edge", &id, None, Some(to_value(&edge)?))?;
    ctx.new_ids.edges.push(id.clone());
    Ok(effect_for(command, "edge", &id, 1))
}

fn unlink(
    ctx: &mut Ctx<'_>,
    tx: &Transaction<'_>,
    command: &Command,
    edge_id: &str,
) -> Result<Effect, CommandError> {
    let before = load_edge_any(tx, edge_id)?.ok_or_else(|| CommandError::NotFound {
        entity: "edge",
        id: edge_id.to_owned(),
    })?;
    if before.deleted_at_utc_ms.is_some() {
        return Err(CommandError::NotFound {
            entity: "edge",
            id: edge_id.to_owned(),
        });
    }
    tx.execute(
        "UPDATE edges SET deleted_at_utc_ms = ?1
          WHERE id = ?2 AND deleted_at_utc_ms IS NULL",
        params![ctx.now, edge_id],
    )?;
    let after = load_edge_any(tx, edge_id)?.ok_or_else(|| CommandError::NotFound {
        entity: "edge",
        id: edge_id.to_owned(),
    })?;
    ctx.event(
        tx,
        command,
        "edge",
        edge_id,
        Some(to_value(&before)?),
        Some(to_value(&after)?),
    )?;
    Ok(effect_for(command, "edge", edge_id, 1))
}

/// Loads the active board for a store (used by the CLI and the MCP server).
pub fn load_board(store: &Store) -> Result<model::Board, CommandError> {
    Ok(model::Board::load(store.connection())?)
}

/// Counts audit rows; used by tests that prove nothing is ever deleted.
pub fn audit_counts(conn: &Connection) -> Result<(i64, i64, i64, i64), CommandError> {
    let batches = conn.query_row("SELECT COUNT(*) FROM batches", [], |row| row.get(0))?;
    let undo = conn.query_row("SELECT COUNT(*) FROM undo_targets", [], |row| row.get(0))?;
    let events = conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
    let snapshots = conn.query_row("SELECT COUNT(*) FROM snapshots", [], |row| row.get(0))?;
    Ok((batches, undo, events, snapshots))
}

/// The statuses a UI may render; kept here so the CLI and MCP agree.
pub fn open_notes(board: &model::Board) -> Vec<&Note> {
    board
        .notes
        .iter()
        .filter(|note| note.status == Status::Open)
        .collect()
}

/// A group with its active notes, ready for the canvas.
pub fn group_notes<'a>(board: &'a model::Board, group: Option<&Group>) -> Vec<&'a Note> {
    let group_id = group.map(|group| group.id.as_str());
    board
        .notes
        .iter()
        .filter(|note| note.group.as_deref() == group_id && note.parent.is_none())
        .collect()
}

/// A group created in this batch, if any (helper for tests and the UI).
pub fn created_group(applied: &Applied) -> Option<&GroupId> {
    applied.new_ids.groups.first()
}

/// The batch id as a string (helper for callers that store it).
pub fn batch_id(applied: &Applied) -> &str {
    applied.batch_id.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{StorePaths, schema, scratch_dir};

    fn store(name: &str) -> Store {
        let paths = StorePaths::from_home(scratch_dir(name));
        schema::open_board(&paths).expect("board opens")
    }

    fn add(store: &mut Store, text: &str) -> NoteId {
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
                Some("test"),
            )
            .expect("add note");
        applied.new_ids.notes[0].clone()
    }

    fn note(store: &Store, id: &str) -> Note {
        model::load_note(store.connection(), id)
            .expect("read note")
            .expect("note exists")
    }

    fn board(store: &Store) -> model::Board {
        model::Board::load(store.connection()).expect("load board")
    }

    #[test]
    fn add_note_writes_one_batch_and_one_event() {
        let mut store = store("cmd-add");
        let applied = store
            .apply(
                &[Command::AddNote {
                    text: "buy milk".to_owned(),
                    group: Some("Home".to_owned()),
                    parent: None,
                    due: Some(TimeSpec {
                        local: "2026-11-23T09:00:00".to_owned(),
                        tz: "Europe/London".to_owned(),
                    }),
                    remind: None,
                    priority: Some(Priority::High),
                    source: "capture".to_owned(),
                }],
                Actor::Human,
                Some("capture"),
            )
            .expect("apply");

        assert_eq!(applied.effects.len(), 1);
        assert_eq!(applied.effects[0].command, "add_note");
        assert_eq!(applied.new_ids.notes.len(), 1);
        assert_eq!(applied.new_ids.groups.len(), 1);
        assert!(model::batch_exists(store.connection(), &applied.batch_id).unwrap());

        let note = note(&store, &applied.new_ids.notes[0]);
        assert_eq!(note.text, "buy milk");
        assert_eq!(note.priority, Some(Priority::High));
        assert_eq!(note.source, "capture");
        assert_eq!(note.group, Some(applied.new_ids.groups[0].clone()));
        assert_eq!(note.due.expect("due").local, "2026-11-23T09:00:00");
        assert_eq!(
            store
                .connection()
                .query_row("SELECT COUNT(*) FROM events", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2,
            "one event for the note and one for the created group"
        );
        assert_eq!(board(&store).groups[0].name, "Home");
    }

    #[test]
    fn a_known_group_is_reused() {
        let mut store = store("cmd-group-reuse");
        store
            .apply(
                &[Command::CreateGroup {
                    name: "Work".to_owned(),
                    color: 2,
                }],
                Actor::Human,
                None,
            )
            .expect("create group");
        let applied = store
            .apply(
                &[Command::AddNote {
                    text: "ship it".to_owned(),
                    group: Some("work".to_owned()),
                    parent: None,
                    due: None,
                    remind: None,
                    priority: None,
                    source: "human".to_owned(),
                }],
                Actor::Human,
                None,
            )
            .expect("add");
        assert!(applied.new_ids.groups.is_empty(), "no duplicate group");
        assert_eq!(board(&store).groups.len(), 1);
    }

    #[test]
    fn set_text_and_priority_round_trip() {
        let mut store = store("cmd-set");
        let id = add(&mut store, "first");
        store
            .apply(
                &[
                    Command::SetText {
                        id: id.clone(),
                        text: "second".to_owned(),
                    },
                    Command::SetPriority {
                        id: id.clone(),
                        priority: Some(Priority::Low),
                    },
                ],
                Actor::Human,
                None,
            )
            .expect("apply");
        let note = note(&store, &id);
        assert_eq!(note.text, "second");
        assert_eq!(note.priority, Some(Priority::Low));
        assert_eq!(note.version, 3, "initial insert plus two updates");
    }

    #[test]
    fn a_cycle_is_rejected_and_nothing_is_written() {
        let mut store = store("cmd-cycle");
        let a = add(&mut store, "a");
        let b = add(&mut store, "b");
        store
            .apply(
                &[Command::SetParent {
                    id: b.clone(),
                    parent: Some(a.clone()),
                }],
                Actor::Human,
                None,
            )
            .expect("b under a");
        let before = audit_counts(store.connection()).expect("counts");

        let error = store
            .apply(
                &[Command::SetParent {
                    id: a.clone(),
                    parent: Some(b.clone()),
                }],
                Actor::Human,
                None,
            )
            .expect_err("cycle");
        assert_eq!(error.index(), Some(0));
        assert!(matches!(
            error,
            CommandError::At { source, .. } if matches!(*source, CommandError::Cycle)
        ));
        let after = audit_counts(store.connection()).expect("counts");
        assert_eq!(before, after, "a failed batch writes nothing");
        assert_eq!(board(&store).notes.len(), 2);
    }

    #[test]
    fn delete_removes_the_subtree_and_its_edges_and_restore_brings_them_back() {
        let mut store = store("cmd-delete");
        let parent = add(&mut store, "parent");
        let child = store
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
            .expect("add child")
            .new_ids
            .notes[0]
            .clone();
        let other = add(&mut store, "other");
        let edge = store
            .apply(
                &[Command::Link {
                    from: parent.clone(),
                    to: other.clone(),
                    kind: None,
                }],
                Actor::Human,
                None,
            )
            .expect("link")
            .new_ids
            .edges[0]
            .clone();

        let applied = store
            .apply(
                &[Command::Delete { id: parent.clone() }],
                Actor::Human,
                None,
            )
            .expect("delete");
        assert_eq!(applied.effects[0].affected, 3, "two notes and one edge");
        assert!(board(&store).notes.iter().all(|note| note.id != parent));
        assert!(board(&store).edges.is_empty());

        store
            .apply(
                &[Command::Restore { id: parent.clone() }],
                Actor::Human,
                None,
            )
            .expect("restore");
        let board = board(&store);
        assert!(board.note(&parent).is_some());
        assert!(board.note(&child).is_some());
        assert!(board.edge(&edge).is_some());
    }

    #[test]
    fn a_failure_rolls_back_the_whole_batch() {
        let mut store = store("cmd-rollback");
        let error = store
            .apply(
                &[
                    Command::AddNote {
                        text: "kept only if the batch commits".to_owned(),
                        group: None,
                        parent: None,
                        due: None,
                        remind: None,
                        priority: None,
                        source: "test".to_owned(),
                    },
                    Command::SetText {
                        id: "missing".to_owned(),
                        text: "nope".to_owned(),
                    },
                ],
                Actor::Human,
                None,
            )
            .expect_err("second command fails");
        assert_eq!(error.index(), Some(1));
        let (batches, _, events, _) = audit_counts(store.connection()).expect("counts");
        assert_eq!(batches, 0);
        assert_eq!(events, 0);
        assert!(board(&store).notes.is_empty());
    }

    #[test]
    fn complete_reopen_and_snooze_manage_their_fields() {
        let mut store = store("cmd-status");
        let id = add(&mut store, "task");
        store
            .apply(&[Command::Complete { id: id.clone() }], Actor::Human, None)
            .expect("complete");
        let done = note(&store, &id);
        assert_eq!(done.status, Status::Done);
        assert!(done.completed_at_utc_ms.is_some());

        store
            .apply(&[Command::Reopen { id: id.clone() }], Actor::Human, None)
            .expect("reopen");
        let open = note(&store, &id);
        assert_eq!(open.status, Status::Open);
        assert!(open.completed_at_utc_ms.is_none());
        assert!(
            open.reminded_at_utc_ms.is_none(),
            "reopen re-arms reminders"
        );

        store
            .apply(
                &[Command::Snooze {
                    id: id.clone(),
                    until: TimeSpec {
                        local: "2026-12-01T08:00:00".to_owned(),
                        tz: "Europe/London".to_owned(),
                    },
                }],
                Actor::Human,
                None,
            )
            .expect("snooze");
        assert!(note(&store, &id).snooze.is_some());
        store
            .apply(&[Command::Unsnooze { id: id.clone() }], Actor::Human, None)
            .expect("unsnooze");
        assert!(note(&store, &id).snooze.is_none());
    }

    #[test]
    fn only_roots_can_be_pinned() {
        let mut store = store("cmd-pin");
        let parent = add(&mut store, "parent");
        let child = store
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
            .expect("add child")
            .new_ids
            .notes[0]
            .clone();

        let error = store
            .apply(
                &[Command::Pin {
                    id: child.clone(),
                    x: 10.0,
                    y: 20.0,
                }],
                Actor::Human,
                None,
            )
            .expect_err("child cannot be pinned");
        assert!(error.to_string().contains("root notes"));

        store
            .apply(
                &[Command::Pin {
                    id: parent.clone(),
                    x: 10.0,
                    y: 20.0,
                }],
                Actor::Human,
                None,
            )
            .expect("pin root");
        let pinned = note(&store, &parent);
        assert_eq!(pinned.layout, LayoutMode::Pinned);
        assert!(pinned.pos.is_some());

        // Reparenting a pinned note clears the position.
        store
            .apply(
                &[Command::SetParent {
                    id: parent.clone(),
                    parent: Some(child.clone()),
                }],
                Actor::Human,
                None,
            )
            .expect_err("cycle");
        let other = add(&mut store, "other");
        store
            .apply(
                &[Command::SetParent {
                    id: parent.clone(),
                    parent: Some(other.clone()),
                }],
                Actor::Human,
                None,
            )
            .expect("reparent");
        let moved = note(&store, &parent);
        assert_eq!(moved.layout, LayoutMode::Flow);
        assert!(moved.pos.is_none());
    }

    #[test]
    fn links_reject_self_duplicates_and_unknown_ends() {
        let mut store = store("cmd-link");
        let a = add(&mut store, "a");
        let b = add(&mut store, "b");

        for command in [
            Command::Link {
                from: a.clone(),
                to: a.clone(),
                kind: None,
            },
            Command::Link {
                from: a.clone(),
                to: "missing".to_owned(),
                kind: None,
            },
        ] {
            assert!(
                store.apply(&[command], Actor::Human, None).is_err(),
                "invalid link must fail"
            );
        }

        let applied = store
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
        let edge_id = applied.new_ids.edges[0].clone();
        assert!(
            store
                .apply(
                    &[Command::Link {
                        from: a.clone(),
                        to: b.clone(),
                        kind: None,
                    }],
                    Actor::Human,
                    None
                )
                .is_err(),
            "duplicate link"
        );

        store
            .apply(
                &[Command::Unlink {
                    edge_id: edge_id.clone(),
                }],
                Actor::Human,
                None,
            )
            .expect("unlink");
        assert!(board(&store).edges.is_empty());
        // Re-linking after an unlink is allowed.
        store
            .apply(
                &[Command::Link {
                    from: a,
                    to: b,
                    kind: None,
                }],
                Actor::Human,
                None,
            )
            .expect("relink");
    }

    #[test]
    fn groups_validate_names() {
        let mut store = store("cmd-groups");
        store
            .apply(
                &[Command::CreateGroup {
                    name: "Home".to_owned(),
                    color: 0,
                }],
                Actor::Human,
                None,
            )
            .expect("create");
        assert!(
            store
                .apply(
                    &[Command::CreateGroup {
                        name: "home".to_owned(),
                        color: 0
                    }],
                    Actor::Human,
                    None
                )
                .is_err(),
            "duplicate names are rejected case-insensitively"
        );
        let id = board(&store).groups[0].id.clone();
        store
            .apply(
                &[Command::RenameGroup {
                    id: id.clone(),
                    name: "House".to_owned(),
                }],
                Actor::Human,
                None,
            )
            .expect("rename");
        assert_eq!(board(&store).groups[0].name, "House");
    }

    #[test]
    fn every_command_round_trips_through_json() {
        let commands = vec![
            Command::AddNote {
                text: "x".to_owned(),
                group: None,
                parent: None,
                due: None,
                remind: None,
                priority: None,
                source: "human".to_owned(),
            },
            Command::SetText {
                id: "n".to_owned(),
                text: "y".to_owned(),
            },
            Command::Snooze {
                id: "n".to_owned(),
                until: TimeSpec {
                    local: "2026-12-01T08:00:00".to_owned(),
                    tz: "Europe/London".to_owned(),
                },
            },
        ];
        for command in commands {
            let json = serde_json::to_string(&command).expect("encode");
            let decoded: Command = serde_json::from_str(&json).expect("decode");
            assert_eq!(decoded, command);
        }
        let tagged: Command =
            serde_json::from_str(r#"{"command":"add_note","text":"a"}"#).expect("tagged");
        assert!(matches!(tagged, Command::AddNote { ref text, .. } if text == "a"));
    }

    #[test]
    fn deleting_and_restoring_are_single_event_per_row() {
        let mut store = store("cmd-events");
        let parent = add(&mut store, "parent");
        let child = store
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
            .expect("add child")
            .new_ids
            .notes[0]
            .clone();
        let before = audit_counts(store.connection()).expect("counts");
        store
            .apply(
                &[Command::Delete { id: parent.clone() }],
                Actor::Human,
                None,
            )
            .expect("delete");
        let after_delete = audit_counts(store.connection()).expect("counts");
        assert_eq!(after_delete.2 - before.2, 2, "one event per note");
        store
            .apply(
                &[Command::Restore { id: parent.clone() }],
                Actor::Human,
                None,
            )
            .expect("restore");
        let after_restore = audit_counts(store.connection()).expect("counts");
        assert_eq!(after_restore.2 - after_delete.2, 2);
        assert!(board(&store).note(&child).is_some());
    }

    #[test]
    fn restore_of_an_active_note_is_harmless() {
        let mut store = store("cmd-restore-active");
        let id = add(&mut store, "active");
        let applied = store
            .apply(&[Command::Restore { id }], Actor::Human, None)
            .expect("restore active");
        assert_eq!(applied.effects[0].affected, 0);
    }
}

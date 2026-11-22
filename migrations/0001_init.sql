-- 0001_init.sql - Rustboard board schema (v2).
--
-- Conventions from docs/tasks/000-index.md:
--   * ids are opaque TEXT values minted by the command layer;
--   * instants are epoch milliseconds UTC and end in _utc_ms;
--   * deadlines, reminders and snoozes keep civil wall time + IANA zone and a
--     resolved UTC cache: all three parts are present, or all three are NULL;
--   * deletes are soft (deleted_at_utc_ms); every read filters it out;
--   * version is the optimistic-concurrency token, owned by a trigger;
--   * batches/undo_targets/events/snapshots/schema_migrations are append-only.
--
-- Every board table is STRICT: a wrong value type fails instead of being
-- silently coerced, which keeps malformed agent input from becoming a row.
--
-- The five audit tables are protected by BEFORE UPDATE / BEFORE DELETE
-- triggers at the bottom of this file. History is always appended, never
-- rewritten; undo is a new batch that points at the batch it reverses.

-- ---------------------------------------------------------------------------
-- Board tables
-- ---------------------------------------------------------------------------

-- A group is a root column on the canvas. Notes point at a group with
-- notes.group_id; group columns are ordered by sort_order then id (task 070).
CREATE TABLE groups (
    id                TEXT    NOT NULL PRIMARY KEY,
    name              TEXT    NOT NULL CHECK (length(trim(name)) > 0),
    -- Palette index into theme::NOTE_PALETTE, matching the v1 note colours.
    color             INTEGER NOT NULL DEFAULT 0 CHECK (color >= 0),
    sort_order        INTEGER NOT NULL DEFAULT 0,
    created_at_utc_ms INTEGER NOT NULL,
    updated_at_utc_ms INTEGER NOT NULL,
    deleted_at_utc_ms INTEGER,
    version           INTEGER NOT NULL DEFAULT 1 CHECK (version >= 1)
) STRICT;

-- A note is both a sticky and, when other notes have parent_id = note.id, a
-- container. parent_id is a self-reference; the check keeps a note from being
-- its own parent (deeper cycles are rejected by the command layer, which has
-- to walk the graph anyway).
--
-- layout_mode='flow' means the layout engine places the note; 'pinned' uses
-- pos_x/pos_y (root notes only, task 070).
CREATE TABLE notes (
    id                  TEXT    NOT NULL PRIMARY KEY,
    text                TEXT    NOT NULL DEFAULT '',
    group_id            TEXT    REFERENCES groups(id) ON DELETE RESTRICT,
    parent_id           TEXT    REFERENCES notes(id) ON DELETE RESTRICT,
    layout_mode         TEXT    NOT NULL DEFAULT 'flow'
                                CHECK (layout_mode IN ('flow', 'pinned')),
    pos_x               REAL,
    pos_y               REAL,
    sort_order          INTEGER NOT NULL DEFAULT 0,
    color               INTEGER NOT NULL DEFAULT 0 CHECK (color >= 0),
    -- NULL = no priority; capture maps !high/!med/!low (task 080).
    priority            TEXT    CHECK (priority IS NULL
                                       OR priority IN ('low', 'med', 'high')),
    status              TEXT    NOT NULL DEFAULT 'open'
                                CHECK (status IN ('open', 'done')),
    -- Where the note came from; kept free-form so new entry points do not
    -- need a migration.
    source              TEXT    NOT NULL DEFAULT 'human'
                                CHECK (length(trim(source)) > 0),
    created_at_utc_ms   INTEGER NOT NULL,
    updated_at_utc_ms   INTEGER NOT NULL,
    deleted_at_utc_ms   INTEGER,
    version             INTEGER NOT NULL DEFAULT 1 CHECK (version >= 1),
    -- Deadline / reminder / snooze: civil triple + resolved cache.
    due_local           TEXT,
    due_tz              TEXT,
    due_utc_ms          INTEGER,
    remind_local        TEXT,
    remind_tz           TEXT,
    remind_utc_ms       INTEGER,
    -- Set once a reminder has fired; reopen clears it so the reminder re-arms.
    reminded_at_utc_ms  INTEGER,
    snooze_local        TEXT,
    snooze_tz           TEXT,
    snooze_utc_ms       INTEGER,
    completed_at_utc_ms INTEGER,
    CHECK (parent_id IS NULL OR parent_id <> id),
    CHECK (layout_mode <> 'pinned'
           OR (pos_x IS NOT NULL AND pos_y IS NOT NULL)),
    CHECK ((due_local IS NULL) = (due_tz IS NULL)
           AND (due_local IS NULL) = (due_utc_ms IS NULL)),
    CHECK ((remind_local IS NULL) = (remind_tz IS NULL)
           AND (remind_local IS NULL) = (remind_utc_ms IS NULL)),
    CHECK ((snooze_local IS NULL) = (snooze_tz IS NULL)
           AND (snooze_local IS NULL) = (snooze_utc_ms IS NULL)),
    CHECK (reminded_at_utc_ms IS NULL OR remind_utc_ms IS NOT NULL),
    CHECK ((status = 'done') = (completed_at_utc_ms IS NOT NULL))
) STRICT;

-- A directed arrow between two notes. "from" and "to" are SQL keywords, so
-- the identifiers are quoted everywhere; the column names are kept because
-- the command and JSON contracts use from/to (tasks 040/070).
--
-- Edges are soft-deleted like notes (unlink keeps the row for undo). A plain
-- table UNIQUE("from", "to", kind) would therefore make an unlinked edge block
-- a fresh link forever, so the uniqueness is enforced among *live* edges by
-- the partial index below. History still keeps one row per link episode.
CREATE TABLE edges (
    id                TEXT    NOT NULL PRIMARY KEY,
    "from"            TEXT    NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    "to"              TEXT    NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    kind              TEXT    NOT NULL DEFAULT 'link'
                              CHECK (length(trim(kind)) > 0),
    created_at_utc_ms INTEGER NOT NULL,
    updated_at_utc_ms INTEGER NOT NULL,
    deleted_at_utc_ms INTEGER,
    version           INTEGER NOT NULL DEFAULT 1 CHECK (version >= 1),
    CHECK ("from" <> "to")
) STRICT;

-- ---------------------------------------------------------------------------
-- Append-only audit tables
-- ---------------------------------------------------------------------------

-- One batch per command-layer transaction (task 040) and per undo/redo
-- (task 050). kind drives history filtering; actor='system' batches (reminder
-- markers) are skipped by user undo.
CREATE TABLE batches (
    id                TEXT    NOT NULL PRIMARY KEY,
    created_at_utc_ms INTEGER NOT NULL,
    actor             TEXT    NOT NULL
                              CHECK (actor IN ('human', 'agent', 'system')),
    -- Session / Pi turn / tool call reference, when there is one.
    actor_ref         TEXT,
    label             TEXT,
    kind              TEXT    NOT NULL CHECK (kind IN ('apply', 'undo', 'redo'))
) STRICT;

-- Links a reversing (undo/redo) batch to the batch(es) it reverses. A batch
-- may target several earlier batches (a whole agent turn).
CREATE TABLE undo_targets (
    undo_batch_id   TEXT NOT NULL REFERENCES batches(id) ON DELETE RESTRICT,
    target_batch_id TEXT NOT NULL REFERENCES batches(id) ON DELETE RESTRICT,
    PRIMARY KEY (undo_batch_id, target_batch_id),
    CHECK (undo_batch_id <> target_batch_id)
) STRICT;

-- One event per entity changed by a batch, in application order (seq).
--
-- id is an INTEGER rowid alias on purpose: task 050 reads a note's history
-- ordered by id, so ids must be monotonic. Rows are never deleted, so SQLite
-- cannot reuse an id even without AUTOINCREMENT.
CREATE TABLE events (
    id           INTEGER PRIMARY KEY,
    batch_id     TEXT    NOT NULL REFERENCES batches(id) ON DELETE RESTRICT,
    seq          INTEGER NOT NULL CHECK (seq >= 0),
    at_utc_ms    INTEGER NOT NULL,
    actor        TEXT    NOT NULL
                         CHECK (actor IN ('human', 'agent', 'system')),
    command      TEXT    NOT NULL CHECK (length(command) > 0),
    entity       TEXT    NOT NULL CHECK (length(entity) > 0),
    entity_id    TEXT    NOT NULL,
    payload_json TEXT    NOT NULL CHECK (json_valid(payload_json)),
    before_json  TEXT    CHECK (before_json IS NULL OR json_valid(before_json)),
    after_json   TEXT    CHECK (after_json IS NULL OR json_valid(after_json)),
    UNIQUE (batch_id, seq)
) STRICT;

-- Point-in-time image of the board for board_sql / manual restore (tasks
-- 050/110). batch_id may be NULL for a snapshot not tied to a history batch
-- (for example an automatic pre-restore image).
CREATE TABLE snapshots (
    id        TEXT    NOT NULL PRIMARY KEY,
    batch_id  TEXT    REFERENCES batches(id) ON DELETE RESTRICT,
    at_utc_ms INTEGER NOT NULL,
    data      TEXT    NOT NULL CHECK (json_valid(data)),
    reason    TEXT    NOT NULL CHECK (length(trim(reason)) > 0)
) STRICT;

-- ---------------------------------------------------------------------------
-- Indexes
-- ---------------------------------------------------------------------------

-- Active-note reads: the layout engine walks a group or a parent, ordered.
CREATE INDEX notes_active_by_group
    ON notes (group_id, sort_order)
    WHERE deleted_at_utc_ms IS NULL;
CREATE INDEX notes_active_by_parent
    ON notes (parent_id, sort_order)
    WHERE deleted_at_utc_ms IS NULL;

-- Restore / deleted-subtree walks (task 040) and soft-delete audits.
CREATE INDEX notes_deleted_parent
    ON notes (deleted_at_utc_ms, parent_id);
CREATE INDEX notes_due
    ON notes (due_utc_ms);

-- Reminder scan (task 140): open, not deleted, not yet fired.
CREATE INDEX notes_reminders_due
    ON notes (remind_utc_ms)
    WHERE deleted_at_utc_ms IS NULL
      AND status = 'open'
      AND reminded_at_utc_ms IS NULL;

CREATE INDEX groups_active_order
    ON groups (sort_order)
    WHERE deleted_at_utc_ms IS NULL;

CREATE INDEX edges_from ON edges ("from");
CREATE INDEX edges_to ON edges ("to");
-- Uniqueness among live links only; see the comment on the edges table.
CREATE UNIQUE INDEX edges_live_unique
    ON edges ("from", "to", kind)
    WHERE deleted_at_utc_ms IS NULL;

CREATE INDEX events_batch ON events (batch_id);
CREATE INDEX events_entity ON events (entity, entity_id);
CREATE INDEX batches_created ON batches (created_at_utc_ms);
CREATE INDEX undo_targets_target ON undo_targets (target_batch_id);

-- ---------------------------------------------------------------------------
-- Touch triggers: updated_at_utc_ms and version are owned by the schema
--
-- SQLite cannot assign NEW.* in a BEFORE trigger, but a BEFORE trigger may
-- update the same row; without recursive_triggers the trigger is not re-entered
-- (the WHEN guard also stops recursion if the pragma is ever enabled). Columns
-- named by the caller's UPDATE are written afterwards; version and updated_at
-- are not among them, so the values set here survive. Callers must not write
-- either column themselves.
-- ---------------------------------------------------------------------------

CREATE TRIGGER groups_touch
BEFORE UPDATE ON groups
FOR EACH ROW
WHEN NEW.version = OLD.version
 AND NEW.updated_at_utc_ms = OLD.updated_at_utc_ms
BEGIN
    UPDATE groups
       SET updated_at_utc_ms = max(CAST(unixepoch('subsec') * 1000 AS INTEGER),
                                   OLD.updated_at_utc_ms + 1),
           version = OLD.version + 1
     WHERE id = OLD.id;
END;

CREATE TRIGGER notes_touch
BEFORE UPDATE ON notes
FOR EACH ROW
WHEN NEW.version = OLD.version
 AND NEW.updated_at_utc_ms = OLD.updated_at_utc_ms
BEGIN
    UPDATE notes
       SET updated_at_utc_ms = max(CAST(unixepoch('subsec') * 1000 AS INTEGER),
                                   OLD.updated_at_utc_ms + 1),
           version = OLD.version + 1
     WHERE id = OLD.id;
END;

CREATE TRIGGER edges_touch
BEFORE UPDATE ON edges
FOR EACH ROW
WHEN NEW.version = OLD.version
 AND NEW.updated_at_utc_ms = OLD.updated_at_utc_ms
BEGIN
    UPDATE edges
       SET updated_at_utc_ms = max(CAST(unixepoch('subsec') * 1000 AS INTEGER),
                                   OLD.updated_at_utc_ms + 1),
           version = OLD.version + 1
     WHERE id = OLD.id;
END;

-- ---------------------------------------------------------------------------
-- Append-only guards
--
-- Every audit table rejects UPDATE and DELETE. INSERT is the only mutation, and
-- the command layer plus the board_sql authorizer (task 110) rely on this.
-- schema_migrations was created by the migration runner; the guards are added
-- here so a later migration's history row can still be inserted.
-- ---------------------------------------------------------------------------

CREATE TRIGGER batches_no_update BEFORE UPDATE ON batches FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'batches is append-only');
END;
CREATE TRIGGER batches_no_delete BEFORE DELETE ON batches FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'batches is append-only');
END;

CREATE TRIGGER undo_targets_no_update BEFORE UPDATE ON undo_targets FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'undo_targets is append-only');
END;
CREATE TRIGGER undo_targets_no_delete BEFORE DELETE ON undo_targets FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'undo_targets is append-only');
END;

CREATE TRIGGER events_no_update BEFORE UPDATE ON events FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'events is append-only');
END;
CREATE TRIGGER events_no_delete BEFORE DELETE ON events FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'events is append-only');
END;

CREATE TRIGGER snapshots_no_update BEFORE UPDATE ON snapshots FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'snapshots is append-only');
END;
CREATE TRIGGER snapshots_no_delete BEFORE DELETE ON snapshots FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'snapshots is append-only');
END;

CREATE TRIGGER schema_migrations_no_update BEFORE UPDATE ON schema_migrations FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'schema_migrations is append-only');
END;
CREATE TRIGGER schema_migrations_no_delete BEFORE DELETE ON schema_migrations FOR EACH ROW
BEGIN
    SELECT RAISE(ABORT, 'schema_migrations is append-only');
END;

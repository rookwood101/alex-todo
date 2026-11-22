-- 0002_fts.sql - full-text search over note text.
--
-- The search table is contentless: the note text already lives in notes, and
-- duplicating it in FTS shadow tables would double the file size for no gain.
-- contentless_delete=1 (SQLite 3.43+, bundled by rusqlite's bundled-full) lets
-- the triggers below remove a row with an ordinary DELETE, so a soft-deleted
-- note really leaves the index. Without it a contentless table could only be
-- fed the special 'delete' command with the old text, and a plain DELETE would
-- fail.
--
-- notes is a rowid table: its TEXT id is the public identity, while SQLite's
-- implicit integer rowid is what FTS5 indexes and returns. Never equate
-- notes.id with notes_fts.rowid.
--
-- Triggers fire on INSERT, DELETE and UPDATE, and they mirror the soft-delete
-- rule: only rows with deleted_at_utc_ms IS NULL are indexed. The command layer
-- never hard-deletes notes, but the DELETE trigger keeps the index honest if a
-- snapshot restore or maintenance code does.
--
-- Query note ids with store::schema::NOTE_SEARCH_SQL (or any join on
-- notes.rowid) and keep the additional deleted_at_utc_ms IS NULL filter: it is
-- cheap defence in depth if a trigger is ever disabled by maintenance tooling.

CREATE VIRTUAL TABLE notes_fts USING fts5(
    text,
    content = '',
    contentless_delete = 1,
    tokenize = 'unicode61'
);

-- A fresh board has no notes yet, so this is normally a no-op. It keeps the
-- migration correct if 0002 is ever applied to a database that already holds
-- notes (for example a development board created before FTS shipped).
INSERT INTO notes_fts (rowid, text)
    SELECT rowid, text FROM notes WHERE deleted_at_utc_ms IS NULL;

-- A live note enters the index on insert.
CREATE TRIGGER notes_fts_insert
AFTER INSERT ON notes
FOR EACH ROW
WHEN NEW.deleted_at_utc_ms IS NULL
BEGIN
    INSERT INTO notes_fts (rowid, text) VALUES (NEW.rowid, NEW.text);
END;

-- A hard-deleted live note leaves the index.
CREATE TRIGGER notes_fts_delete
AFTER DELETE ON notes
FOR EACH ROW
WHEN OLD.deleted_at_utc_ms IS NULL
BEGIN
    DELETE FROM notes_fts WHERE rowid = OLD.rowid;
END;

-- Text edits re-index; changing only deleted_at_utc_ms adds or removes the row.
-- The WHEN clause also stops the version/updated_at touch trigger's internal
-- UPDATE from doing useless FTS work.
CREATE TRIGGER notes_fts_update
AFTER UPDATE ON notes
FOR EACH ROW
WHEN NEW.text <> OLD.text
  OR (NEW.deleted_at_utc_ms IS NULL) <> (OLD.deleted_at_utc_ms IS NULL)
BEGIN
    DELETE FROM notes_fts WHERE rowid = OLD.rowid;
    INSERT INTO notes_fts (rowid, text)
        SELECT NEW.rowid, NEW.text
        WHERE NEW.deleted_at_utc_ms IS NULL;
END;

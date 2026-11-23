-- 0003_containers.sql - groups become container notes; priority becomes a
-- binary importance flag; the Inbox becomes a note.
--
-- After this migration:
--   * a "group" is just a root note with children (a container);
--   * notes.role = 'inbox' marks the one Inbox container (at most one live);
--   * notes.important replaces notes.priority ('high' -> 1, others -> 0);
--   * groups, notes.group_id and notes.priority stay in the schema only so old
--     history images still restore; nothing writes them any more.
--
-- Each live group becomes a root note with the same id, name, colour and sort
-- order. Its flow root notes move inside it. Pinned notes stay pinned at the
-- root. Loose (ungrouped, unpinned) root notes move into a new Inbox note.

ALTER TABLE notes ADD COLUMN important INTEGER NOT NULL DEFAULT 0
    CHECK (important IN (0, 1));
ALTER TABLE notes ADD COLUMN role TEXT
    CHECK (role IS NULL OR role = 'inbox');

CREATE UNIQUE INDEX notes_one_live_inbox
    ON notes (role)
    WHERE role = 'inbox' AND deleted_at_utc_ms IS NULL;

UPDATE notes SET important = 1 WHERE priority = 'high';
UPDATE notes SET priority = NULL WHERE priority IS NOT NULL;

INSERT INTO notes (id, text, sort_order, color, source,
                   created_at_utc_ms, updated_at_utc_ms)
SELECT id, name, sort_order, color, 'migration',
       created_at_utc_ms, updated_at_utc_ms
  FROM groups
 WHERE deleted_at_utc_ms IS NULL;

UPDATE notes
   SET parent_id = group_id
 WHERE group_id IS NOT NULL
   AND parent_id IS NULL
   AND layout_mode = 'flow'
   AND group_id IN (SELECT id FROM groups WHERE deleted_at_utc_ms IS NULL);

UPDATE notes SET group_id = NULL WHERE group_id IS NOT NULL;

UPDATE groups
   SET deleted_at_utc_ms = CAST(unixepoch('subsec') * 1000 AS INTEGER)
 WHERE deleted_at_utc_ms IS NULL;

-- The Inbox is created only when there are loose notes to hold. A fresh board
-- gets its Inbox from the command layer on the first capture.
INSERT INTO notes (id, text, sort_order, source, role,
                   created_at_utc_ms, updated_at_utc_ms)
SELECT lower(hex(randomblob(16))), 'Inbox',
       (SELECT COALESCE(MIN(sort_order), 0) - 1 FROM notes WHERE parent_id IS NULL),
       'system', 'inbox',
       CAST(unixepoch('subsec') * 1000 AS INTEGER),
       CAST(unixepoch('subsec') * 1000 AS INTEGER)
 WHERE EXISTS (
        SELECT 1 FROM notes
         WHERE parent_id IS NULL
           AND layout_mode = 'flow'
           AND deleted_at_utc_ms IS NULL
           AND source <> 'migration'
           AND id NOT IN (SELECT DISTINCT parent_id FROM notes
                           WHERE parent_id IS NOT NULL AND deleted_at_utc_ms IS NULL));

UPDATE notes
   SET parent_id = (SELECT id FROM notes
                     WHERE role = 'inbox' AND deleted_at_utc_ms IS NULL)
 WHERE parent_id IS NULL
   AND layout_mode = 'flow'
   AND deleted_at_utc_ms IS NULL
   AND role IS NULL
   AND source <> 'migration'
   AND id NOT IN (SELECT DISTINCT parent_id FROM notes
                   WHERE parent_id IS NOT NULL AND deleted_at_utc_ms IS NULL)
   AND EXISTS (SELECT 1 FROM notes WHERE role = 'inbox' AND deleted_at_utc_ms IS NULL);

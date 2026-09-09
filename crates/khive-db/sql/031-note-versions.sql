ALTER TABLE notes ADD COLUMN version INTEGER NOT NULL DEFAULT 1;

CREATE TRIGGER IF NOT EXISTS bump_note_version
AFTER UPDATE ON notes
WHEN NEW.version = OLD.version
BEGIN
    UPDATE notes SET version = OLD.version + 1 WHERE id = NEW.id;
END;

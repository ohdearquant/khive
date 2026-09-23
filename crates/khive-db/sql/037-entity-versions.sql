ALTER TABLE entities ADD COLUMN version INTEGER NOT NULL DEFAULT 1;

-- The row version is independent of updated_at and cannot be caller-selected.
CREATE TRIGGER IF NOT EXISTS entities_version_insert_guard
BEFORE INSERT ON entities
WHEN typeof(NEW.version) != 'integer' OR NEW.version != 1
BEGIN
    SELECT RAISE(ABORT, 'entity insert version must be 1');
END;

CREATE TRIGGER IF NOT EXISTS entities_version_update_guard
BEFORE UPDATE ON entities
WHEN OLD.version = 9223372036854775807
  OR typeof(NEW.version) != 'integer'
  OR NEW.version != OLD.version + 1
BEGIN
    SELECT RAISE(ABORT, 'entity update version must advance by exactly one');
END;

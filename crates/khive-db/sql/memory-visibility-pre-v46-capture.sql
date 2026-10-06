-- This runs only inside the transaction that first applies V46.
CREATE TABLE IF NOT EXISTS memory_visibility_pre_v46 (
    note_id TEXT NOT NULL PRIMARY KEY REFERENCES notes(id) ON DELETE CASCADE
);

INSERT INTO memory_visibility_pre_v46 (note_id)
SELECT id FROM notes WHERE kind = 'memory' AND key IS NOT NULL
ON CONFLICT(note_id) DO NOTHING;

INSERT INTO session_mirror_cursor (file_path, session_id, byte_offset, updated_at)
WITH RECURSIVE numbers(value) AS (
    VALUES (1)
    UNION ALL
    SELECT value + 1 FROM numbers WHERE value < 256
)
SELECT 'fixture/' || printf('%04d', value) || hex(zeroblob(1024)), NULL, 0, 0
FROM numbers;

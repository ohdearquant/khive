CREATE TABLE payloads (
    id INTEGER PRIMARY KEY,
    generation INTEGER NOT NULL,
    bytes BLOB NOT NULL
);
INSERT INTO payloads VALUES (1, 0, zeroblob(1048576));

CREATE TABLE tool_source_mounts (
    name TEXT PRIMARY KEY NOT NULL,
    generation INTEGER NOT NULL CHECK (generation > 0),
    tools TEXT NOT NULL CHECK (json_valid(tools) AND json_type(tools) = 'array')
);

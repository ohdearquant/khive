CREATE TABLE IF NOT EXISTS comm_channel_cursor (
    channel_kind TEXT NOT NULL CHECK (length(trim(channel_kind)) > 0),
    channel_slug TEXT NOT NULL CHECK (length(trim(channel_slug)) > 0),
    source TEXT NOT NULL CHECK (length(trim(source)) > 0),
    generation INTEGER NOT NULL CHECK (generation > 0),
    high_water INTEGER CHECK (high_water IS NULL OR high_water > 0),
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (channel_kind, channel_slug)
)

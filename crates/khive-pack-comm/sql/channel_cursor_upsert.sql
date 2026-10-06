INSERT INTO comm_channel_cursor(channel_kind, channel_slug, source, generation, high_water, updated_at)
VALUES(?1, ?2, ?3, ?4, ?5, ?6)
ON CONFLICT(channel_kind, channel_slug) DO UPDATE SET
  source=excluded.source,
  generation=excluded.generation,
  high_water=excluded.high_water,
  updated_at=excluded.updated_at

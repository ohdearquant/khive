SELECT source, generation, high_water, updated_at FROM comm_channel_cursor
WHERE channel_kind = ?1 AND channel_slug = ?2

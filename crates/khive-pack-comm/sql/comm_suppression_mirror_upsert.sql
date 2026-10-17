INSERT INTO comm_suppression_mirror(mirror, refreshed_at, remote_as_of, remote_count)
VALUES('one_click', ?1, ?2, ?3)
ON CONFLICT(mirror) DO UPDATE SET
  refreshed_at=excluded.refreshed_at,
  remote_as_of=excluded.remote_as_of,
  remote_count=excluded.remote_count

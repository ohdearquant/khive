SELECT id, version FROM notes WHERE namespace=?1 AND kind=?2 AND key=?3 AND deleted_at IS NULL

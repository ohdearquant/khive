SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1)

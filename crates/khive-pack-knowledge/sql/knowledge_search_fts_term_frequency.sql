SELECT count(*) AS frequency FROM ( SELECT rowid FROM fts_knowledge WHERE fts_knowledge MATCH ?1 ORDER BY rowid LIMIT ?2 )

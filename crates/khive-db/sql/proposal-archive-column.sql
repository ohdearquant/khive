SELECT type, "notnull", dflt_value, pk, hidden
FROM pragma_table_xinfo('proposals_open')
WHERE name = 'archived_at' COLLATE NOCASE;

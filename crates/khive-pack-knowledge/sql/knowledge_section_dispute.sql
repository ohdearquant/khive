UPDATE knowledge_sections SET status='disputed' WHERE atom_id=?1 AND section_type=?2 AND content_hash=?3 AND status NOT IN ('disputed','deprecated')

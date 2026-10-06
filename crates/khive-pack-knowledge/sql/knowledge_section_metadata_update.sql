UPDATE knowledge_sections SET section_type=?1, heading=?2, tokens=?3, sort_order=?4, embedding=CASE WHEN heading = ?2 THEN embedding ELSE NULL END, updated_at=?5 WHERE id=?6

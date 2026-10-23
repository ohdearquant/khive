SELECT event_id,epoch_id,action_id,record_kind,record_id FROM authority_changes WHERE epoch_id=?1 AND action_id=?2 AND event_id>?3 ORDER BY event_id LIMIT ?4;

SELECT transition_id,epoch_id,record_id,terminal_state,action_id,event_id,created_at,body FROM authority_transitions WHERE epoch_id=?1 AND record_id=?2;

SELECT note_id,disposition,recipient_agent_id,recipient_actor FROM comm_recipient_replay WHERE sender_agent_id=?1 AND logical_message_id=?2

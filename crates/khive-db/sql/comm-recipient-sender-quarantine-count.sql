SELECT count(*) FROM comm_recipient_quarantine WHERE recipient_agent_id=?1 AND sender_agent_id=?2 AND reason='policy_rejected'

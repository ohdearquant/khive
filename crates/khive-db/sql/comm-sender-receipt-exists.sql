SELECT EXISTS(SELECT 1 FROM comm_sender_transport WHERE logical_message_id=?1 AND receipt IS NOT NULL)

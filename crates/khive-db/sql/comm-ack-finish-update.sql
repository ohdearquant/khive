UPDATE comm_ack_work SET state='acknowledged',updated_at=?2 WHERE delivery_attempt_id=?1 AND state='pending'

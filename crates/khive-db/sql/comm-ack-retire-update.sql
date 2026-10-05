UPDATE comm_ack_work SET state='retired',retirement_reason=?2,updated_at=?3 WHERE delivery_attempt_id=?1 AND state='pending'

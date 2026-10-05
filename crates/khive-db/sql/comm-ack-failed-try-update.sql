UPDATE comm_ack_work SET attempt_count=attempt_count+1,not_before=?2,updated_at=?3 WHERE delivery_attempt_id=?1 AND state='pending'

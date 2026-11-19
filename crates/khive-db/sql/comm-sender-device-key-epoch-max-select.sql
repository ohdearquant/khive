SELECT MAX(recipient_key_epoch) FROM comm_sender_transport WHERE logical_message_id=?1 AND recipient_device_id=?2

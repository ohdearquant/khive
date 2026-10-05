UPDATE comm_sender_transport SET hold_reason=?4,updated_at=?5,policy_mode=?6,policy_revision=?7 WHERE logical_message_id=?1 AND recipient_device_id=?2 AND recipient_key_epoch=?3

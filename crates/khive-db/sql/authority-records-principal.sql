SELECT r.record_id,r.epoch_id,r.record_kind,r.principal,r.namespace,
    (SELECT json_group_array(right_type) FROM (
        SELECT right_type FROM authority_record_right_types AS rt
        WHERE rt.epoch_id=r.epoch_id AND rt.record_id=r.record_id
        ORDER BY right_type LIMIT 257
    )) AS right_types,
    r.parent_id,r.bootstrap_slot,r.root_anchor_id,r.created_at,r.body
FROM authority_records AS r
WHERE r.epoch_id=?1 AND r.principal=?2 AND r.record_id>?3 ORDER BY r.record_id LIMIT ?4;

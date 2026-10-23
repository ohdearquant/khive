INSERT INTO scheduled_definitions_v3
(store_id,schedule_id,definition_revision,namespace,definition_digest,canonical_definition,created_at_us)
SELECT ?1,?2,?3,?4,?5,?6,?7
WHERE EXISTS (SELECT 1 FROM notes n WHERE n.id=?2 AND n.namespace=?4
AND n.kind='scheduled_event' AND n.deleted_at IS NULL AND n.properties=?8
AND n.content=?9 AND n.updated_at=?10
AND json_extract(n.properties,'$.receipt_version')=3
AND json_extract(n.properties,'$.status')='pending')
AND (SELECT COUNT(*) FROM events e WHERE e.target_id=?2 AND e.namespace=?4
AND e.verb='schedule.creator_provenance' AND e.actor=?11
AND json_extract(e.payload,'$.provenance')='schedule_creator_v1'
AND json_extract(e.payload,'$.event_type')=?12)=1;

INSERT INTO scheduled_occurrences_v3
(id,store_id,schedule_id,definition_revision,namespace,scheduled_seconds,scheduled_nanos,
 materialized_at_us,selection_at_us,effective_max_lateness_s)
VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10);

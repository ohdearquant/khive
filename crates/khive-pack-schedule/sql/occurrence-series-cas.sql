UPDATE scheduled_series_v3 SET
 revision=revision+1,state=?4,next_seconds=?5,next_nanos=?6,
 window_source_seconds=?7,window_source_nanos=?8,horizon_seconds=?9,horizon_nanos=?10,
 selected_seconds=?11,selected_nanos=?12,selection_at_us=?13,effective_max_lateness_s=?14,
 active_occurrence_id=?15,active_invocation_id=?16,suspended_invocation_id=?17,
 cancellation_cut_us=?18,accounting_blocked_reason=?19
WHERE store_id=?1 AND schedule_id=?2 AND revision=?3;

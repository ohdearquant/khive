use super::*;

/// Refresh accepted dependency/implementation edges for physical producers whose
/// files were reused without parsing. This runs only after the complete L2
/// walk and re-resolution, so a target is refreshed only when this invocation
/// proved both endpoints current. An edge must also have been current at its
/// source project's previous language sweep. A changed endpoint can be current
/// while its impl producer is reused; removed producer relations stay historical.
pub(super) async fn refresh_unchanged_l2_edges(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    sweep_time: DateTime<Utc>,
    previous_l2_sweep_stamps: &PreviousL2SweepStamps,
    state: &mut L2SweepState,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    if state.unchanged_declarations.is_empty() && state.current_natural_edge_ids.is_empty() {
        return Ok(());
    }
    let graph = rt.graph(token)?;
    let sources: Vec<Uuid> = state.unchanged_declarations.iter().copied().collect();
    let edge_ids = &state.current_natural_edge_ids;
    let edges = graph
        .get_edges(
            &edge_ids
                .iter()
                .copied()
                .map(LinkId::from)
                .collect::<Vec<_>>(),
        )
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    for edge in edges {
        let Some(source_owner) = state.current_declarations.get(&edge.source_id).cloned() else {
            continue;
        };
        if state.current_declarations.get(&edge.target_id) != Some(&source_owner) {
            continue;
        }
        let Some(previous_observation) = previous_l2_sweep_stamps
            .get(&source_owner)
            .and_then(Option::as_ref)
        else {
            continue;
        };
        if !edge
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("l2_derived"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || !observation_matches(edge.metadata.as_ref(), previous_observation.run_id)
        {
            continue;
        }
        let edge_id = Uuid::from(edge.id);
        let outcome = mutate_edge(rt, token, edge_id, |current| {
            let mut edge = current?.clone();
            let mut metadata = edge
                .metadata
                .clone()
                .and_then(|value| value.as_object().cloned())
                .unwrap_or_default();
            if !metadata
                .get("l2_derived")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || !observation_matches(edge.metadata.as_ref(), previous_observation.run_id)
            {
                return None;
            }
            metadata.insert("language".into(), json!(source_owner.language));
            metadata.insert("last_seen_at".into(), json!(sweep_time.to_rfc3339()));
            metadata.insert(
                "l2_observed_run_id".into(),
                json!(previous_l2_sweep_stamps.run_id.to_string()),
            );
            edge.updated_at = sweep_time;
            edge.metadata = Some(Value::Object(metadata));
            Some(edge)
        })
        .await?;
        if outcome.wrote() {
            state.stamped_edge_ids.insert(edge_id);
            report.edges_updated += 1;
        }
    }

    let hits = graph
        .batch_neighbors(
            &sources,
            NeighborQuery {
                direction: Direction::In,
                relations: Some(vec![EdgeRelation::Contains]),
                limit: None,
                min_weight: None,
            },
        )
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    let edge_ids: BTreeSet<Uuid> = hits.into_iter().map(|(_, hit)| hit.edge_id).collect();
    let edges = graph
        .get_edges(&edge_ids.into_iter().map(LinkId::from).collect::<Vec<_>>())
        .await
        .map_err(|e| CodeSourceIngestError::Storage(e.to_string()))?;
    for edge in edges {
        let Some(target_owner) = state.current_declarations.get(&edge.target_id).cloned() else {
            continue;
        };
        let source_owner = state
            .current_declarations
            .get(&edge.source_id)
            .or_else(|| state.current_modules.get(&edge.source_id));
        if !state.unchanged_declarations.contains(&edge.target_id)
            || source_owner != Some(&target_owner)
        {
            continue;
        }
        if !edge
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("l2_derived"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let edge_id = Uuid::from(edge.id);
        let outcome = mutate_edge(rt, token, edge_id, |current| {
            let mut edge = current?.clone();
            let mut metadata = edge
                .metadata
                .clone()
                .and_then(|value| value.as_object().cloned())
                .unwrap_or_default();
            if !metadata
                .get("l2_derived")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return None;
            }
            metadata.insert("language".into(), json!(target_owner.language));
            metadata.insert("last_seen_at".into(), json!(sweep_time.to_rfc3339()));
            metadata.insert(
                "l2_observed_run_id".into(),
                json!(previous_l2_sweep_stamps.run_id.to_string()),
            );
            edge.updated_at = sweep_time;
            edge.metadata = Some(Value::Object(metadata));
            Some(edge)
        })
        .await?;
        if outcome.wrote() {
            report.edges_updated += 1;
        }
    }
    Ok(())
}

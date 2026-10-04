//! Invocation observations and natural L2 edge writes.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct L2Observation {
    pub(super) run_id: Uuid,
    pub(super) sweep_time: String,
}

fn parse_l2_run_id(id: &str) -> Option<Uuid> {
    let uuid = Uuid::parse_str(id).ok()?;
    (uuid.get_version() == Some(uuid::Version::Random)
        && uuid.get_variant() == uuid::Variant::RFC4122
        && uuid.to_string() == id)
        .then_some(uuid)
}

pub(super) fn observation_matches(properties: Option<&Value>, run_id: Uuid) -> bool {
    properties
        .and_then(|value| value.get("l2_observed_run_id"))
        .and_then(Value::as_str)
        .and_then(parse_l2_run_id)
        == Some(run_id)
}

pub(super) struct PreviousL2SweepStamps {
    pub(super) run_id: Uuid,
    pub(super) stamps: HashMap<L2OwnerKey, Option<L2Observation>>,
}

impl PreviousL2SweepStamps {
    pub(super) fn new() -> Self {
        Self {
            run_id: Uuid::new_v4(),
            stamps: HashMap::new(),
        }
    }

    pub(super) fn get(&self, owner: &L2OwnerKey) -> Option<&Option<L2Observation>> {
        self.stamps.get(owner)
    }
}

fn valid_l2_run_marker(marker: &Value) -> bool {
    let Some(fields) = marker.as_object() else {
        return false;
    };
    if fields.len() != 2 || fields.get("sweep_time").and_then(Value::as_str).is_none() {
        return false;
    }
    fields
        .get("run_id")
        .and_then(Value::as_str)
        .and_then(parse_l2_run_id)
        .is_some()
}

pub(super) fn valid_l2_sweep_entry(entry: &Value) -> bool {
    entry.as_object().is_some_and(|fields| {
        fields.len() == 3
            && fields.get("version").and_then(Value::as_u64) == Some(1)
            && fields.get("attempted").is_some_and(valid_l2_run_marker)
            && fields
                .get("completed")
                .is_some_and(|marker| marker.is_null() || valid_l2_run_marker(marker))
    })
}

pub(super) fn completed_l2_observation(
    properties: &Value,
    language: &str,
) -> Option<L2Observation> {
    let entry = properties
        .get("l2_sweep_runs")?
        .as_object()?
        .get(language)?;
    if !valid_l2_sweep_entry(entry) || entry.get("completed")? != entry.get("attempted")? {
        return None;
    }
    let stamp = entry.get("completed")?.get("sweep_time")?.as_str()?;
    let observation = L2Observation {
        run_id: parse_l2_run_id(entry.get("completed")?.get("run_id")?.as_str()?)?,
        sweep_time: stamp.to_string(),
    };
    (properties.get("sweep_clock")?.get(language)?.as_str()? == observation.sweep_time)
        .then_some(observation)
}

/// Sorted-set-union evidence merge for one L2 `depends_on` edge — repeated
/// evidence (e.g. the same call observed on re-ingest) folds onto the
/// existing array rather than duplicating it, mirroring
/// `merge_dependency_metadata`'s established pattern for L1 edges.
fn merge_l2_evidence(
    existing_metadata: Option<&Value>,
    new_evidence: &str,
    language: &str,
    now: DateTime<Utc>,
    run_id: Uuid,
) -> Value {
    let mut evidence: BTreeSet<String> = existing_metadata
        .and_then(|m| m.get("l2_evidence"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    evidence.insert(new_evidence.to_string());
    json!({
        "l2_derived": true,
        "l2_evidence": evidence.into_iter().collect::<Vec<_>>(),
        "language": language,
        "last_seen_at": now.to_rfc3339(),
        "l2_observed_run_id": run_id.to_string(),
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn upsert_l2_depends_on(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    source_id: Uuid,
    target_id: Uuid,
    evidence: &str,
    language: &str,
    now: DateTime<Utc>,
    state: &mut L2SweepState,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    let edge_id = edge_uuid(EdgeRelation::DependsOn, source_id, target_id);
    let outcome = mutate_edge(rt, token, edge_id, |current| {
        let metadata = merge_l2_evidence(
            current.and_then(|edge| edge.metadata.as_ref()),
            evidence,
            language,
            now,
            state.run_id,
        );
        Some(Edge {
            id: LinkId::from(edge_id),
            namespace: token.namespace().as_str().to_string(),
            source_id,
            target_id,
            relation: EdgeRelation::DependsOn,
            weight: 1.0,
            created_at: current.map(|edge| edge.created_at).unwrap_or(now),
            updated_at: now,
            deleted_at: None,
            metadata: Some(metadata),
            target_backend: current.and_then(|edge| edge.target_backend.clone()),
        })
    })
    .await?;
    state.stamped_edge_ids.insert(edge_id);
    if outcome != RowMutationOutcome::Blocked {
        state.observed_natural_edge_ids.push(edge_id);
    }
    match outcome {
        RowMutationOutcome::Created => report.edges_created += 1,
        RowMutationOutcome::Updated => report.edges_updated += 1,
        RowMutationOutcome::Unchanged | RowMutationOutcome::Blocked => {}
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn upsert_l2_implements(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    type_id: Uuid,
    trait_id: Uuid,
    language: &str,
    now: DateTime<Utc>,
    state: &mut L2SweepState,
    report: &mut CodeSourceIngestReport,
) -> Result<(), CodeSourceIngestError> {
    let edge_id = edge_uuid(EdgeRelation::Implements, type_id, trait_id);
    let outcome = mutate_edge(rt, token, edge_id, |current| {
        let mut metadata = current
            .and_then(|edge| edge.metadata.as_ref())
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        metadata.insert("l2_derived".into(), json!(true));
        metadata.insert("language".into(), json!(language));
        metadata.insert("last_seen_at".into(), json!(now.to_rfc3339()));
        metadata.insert("l2_observed_run_id".into(), json!(state.run_id.to_string()));
        Some(Edge {
            id: LinkId::from(edge_id),
            namespace: token.namespace().as_str().to_string(),
            source_id: type_id,
            target_id: trait_id,
            relation: EdgeRelation::Implements,
            weight: 1.0,
            created_at: current.map(|edge| edge.created_at).unwrap_or(now),
            updated_at: now,
            deleted_at: None,
            metadata: Some(Value::Object(metadata)),
            target_backend: current.and_then(|edge| edge.target_backend.clone()),
        })
    })
    .await?;
    state.stamped_edge_ids.insert(edge_id);
    if outcome != RowMutationOutcome::Blocked {
        state.observed_natural_edge_ids.push(edge_id);
    }
    match outcome {
        RowMutationOutcome::Created => report.edges_created += 1,
        RowMutationOutcome::Updated => report.edges_updated += 1,
        RowMutationOutcome::Unchanged | RowMutationOutcome::Blocked => {}
    }
    Ok(())
}

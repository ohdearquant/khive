use chrono::{DateTime, Utc};
use khive_runtime::{KhiveRuntime, NamespaceToken};
use khive_storage::LinkId;
use khive_types::EdgeRelation;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::extractor::DeclKind;

use super::{
    get_entity_opt, mutate_entity, observation_matches, ts, CodeSourceIngestError,
    CodeSourceIngestReport, L2Observation, RowMutationOutcome,
};

#[allow(clippy::too_many_arguments)]
pub(super) async fn refresh_l2_declarations(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    source_project: &str,
    language: &str,
    source_path: &str,
    source_revision: &str,
    sweep_time: DateTime<Utc>,
    run_id: Uuid,
    file_label: &str,
    declaration_ids: &[Uuid],
    natural_edge_ids: &[Uuid],
    previous_observation: Option<&L2Observation>,
    report: &mut CodeSourceIngestReport,
) -> Result<bool, CodeSourceIngestError> {
    let Some(previous_observation) = previous_observation else {
        return Ok(false);
    };
    // Check all retained observations before refreshing any declaration.
    // A different owner sweep can cover another root with the same identity.
    let mut declarations = Vec::with_capacity(declaration_ids.len());
    for id in declaration_ids {
        let Some(entity) = get_entity_opt(rt, token, *id).await? else {
            return Ok(false);
        };
        let canonical_kind = entity
            .entity_type
            .as_deref()
            .and_then(DeclKind::from_code_token);
        let properties = entity.properties.as_ref();
        let matches_owner = properties
            .and_then(|value| value.get("source_project"))
            .and_then(Value::as_str)
            == Some(source_project)
            && properties
                .and_then(|value| value.get("language"))
                .and_then(Value::as_str)
                == Some(language);
        let observed_at_predecessor = observation_matches(properties, previous_observation.run_id);
        if canonical_kind.is_none() || !matches_owner || !observed_at_predecessor {
            return Ok(false);
        }
        declarations.push(entity);
    }
    if !retained_natural_edges_match(rt, token, natural_edge_ids, previous_observation.run_id)
        .await?
    {
        return Ok(false);
    }

    for declaration in declarations {
        let id = declaration.id;
        let mut current_valid = true;
        let outcome = mutate_entity(rt, token, id, file_label, report, |current| {
            let Some(mut declaration) = current.cloned() else {
                current_valid = false;
                return None;
            };
            let properties = declaration.properties.as_ref();
            current_valid = declaration
                .entity_type
                .as_deref()
                .and_then(DeclKind::from_code_token)
                .is_some()
                && properties
                    .and_then(|value| value.get("source_project"))
                    .and_then(Value::as_str)
                    == Some(source_project)
                && properties
                    .and_then(|value| value.get("language"))
                    .and_then(Value::as_str)
                    == Some(language)
                && observation_matches(properties, previous_observation.run_id);
            if !current_valid {
                return None;
            }
            let mut properties = declaration
                .properties
                .clone()
                .and_then(|value| value.as_object().cloned())
                .unwrap_or_default();
            properties.insert("source_path".into(), json!(source_path));
            properties.insert("source_revision".into(), json!(source_revision));
            properties.insert("last_seen_at".into(), json!(sweep_time.to_rfc3339()));
            properties.insert("l2_observed_run_id".into(), json!(run_id.to_string()));
            declaration.properties = Some(Value::Object(properties));
            declaration.updated_at = ts(sweep_time);
            Some(declaration)
        })
        .await?;
        if !current_valid || outcome == RowMutationOutcome::Blocked {
            return Ok(false);
        }
        if outcome.wrote() {
            if let Some(l2) = report.l2.as_mut() {
                l2.symbols_updated += 1;
            }
        }
    }
    // These fresh edge reads gate reuse after guarded declaration rebases.
    // They are separate observations, not a transaction across entity/edge rows.
    retained_natural_edges_match(rt, token, natural_edge_ids, previous_observation.run_id).await
}

async fn retained_natural_edges_match(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    natural_edge_ids: &[Uuid],
    previous_run_id: Uuid,
) -> Result<bool, CodeSourceIngestError> {
    if natural_edge_ids.is_empty() {
        return Ok(true);
    }
    let graph = rt.graph(token)?;
    let edges = graph
        .get_edges(
            &natural_edge_ids
                .iter()
                .copied()
                .map(LinkId::from)
                .collect::<Vec<_>>(),
        )
        .await
        .map_err(|error| CodeSourceIngestError::Storage(error.to_string()))?;
    if edges.len() != natural_edge_ids.len() {
        return Ok(false);
    }
    Ok(edges.iter().all(|edge| {
        let metadata = edge.metadata.as_ref();
        matches!(
            edge.relation,
            EdgeRelation::DependsOn | EdgeRelation::Implements
        ) && metadata
            .and_then(|value| value.get("l2_derived"))
            .and_then(Value::as_bool)
            == Some(true)
            && observation_matches(metadata, previous_run_id)
    }))
}

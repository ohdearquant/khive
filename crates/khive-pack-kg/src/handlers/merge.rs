//! `merge` verb handler.

use std::collections::HashSet;

use serde_json::Value;

use khive_runtime::{NamespaceToken, RuntimeError, VerbRegistry};
use khive_storage::Entity;
use khive_types::{Details, KhiveError};

use super::common::{
    deser, ensure_entity_kind, ensure_note_kind, immutable_event_error, parse_content_strategy,
    parse_entity_policy, resolve_kind_spec, resolve_uuid_unfiltered, to_json, KindSpec,
    MergeParams,
};
use crate::KgPack;

#[derive(Clone, Copy)]
enum EntityMergeGuard {
    EntityKind,
    NameSimilarity,
    ProjectCompatibility,
}

impl EntityMergeGuard {
    fn as_str(self) -> &'static str {
        match self {
            Self::EntityKind => "entity_kind",
            Self::NameSimilarity => "name_similarity",
            Self::ProjectCompatibility => "project_compatibility",
        }
    }
}

fn merge_guard_error(guard: EntityMergeGuard) -> RuntimeError {
    RuntimeError::Khive(
        KhiveError::conflict(format!(
            "entity merge refused by {} guard; use force=true only when the caller accepts responsibility",
            guard.as_str()
        ))
        .with_details(Details::new([
            ("guard", guard.as_str()),
            ("override", "force=true"),
        ])),
    )
}

fn validate_entity_merge_floor(into: &Entity, from: &Entity) -> Result<(), RuntimeError> {
    if into.kind != from.kind {
        return Err(merge_guard_error(EntityMergeGuard::EntityKind));
    }
    if !names_are_similar(&into.name, &from.name) {
        return Err(merge_guard_error(EntityMergeGuard::NameSimilarity));
    }
    if projects_are_disjoint(into, from) {
        return Err(merge_guard_error(EntityMergeGuard::ProjectCompatibility));
    }
    Ok(())
}

fn names_are_similar(left: &str, right: &str) -> bool {
    let left = normalize_name(left);
    let right = normalize_name(right);
    if left.is_empty() || right.is_empty() {
        return false;
    }
    if left == right {
        return true;
    }

    let shorter_len = left.chars().count().min(right.chars().count());
    if shorter_len >= 3 && (left.starts_with(&right) || right.starts_with(&left)) {
        return true;
    }

    let left_trigrams = trigrams(&left);
    let right_trigrams = trigrams(&right);
    if left_trigrams.is_empty() || right_trigrams.is_empty() {
        return false;
    }
    let overlap = left_trigrams.intersection(&right_trigrams).count();
    overlap.saturating_mul(4) >= left_trigrams.len().saturating_add(right_trigrams.len())
}

fn normalize_name(name: &str) -> String {
    name.chars()
        .flat_map(char::to_lowercase)
        .filter(|ch| ch.is_alphanumeric())
        .collect()
}

fn trigrams(value: &str) -> HashSet<[char; 3]> {
    let chars: Vec<char> = value.chars().collect();
    chars
        .windows(3)
        .map(|window| [window[0], window[1], window[2]])
        .collect()
}

fn projects_are_disjoint(into: &Entity, from: &Entity) -> bool {
    let Some(into_projects) = into
        .properties
        .as_ref()
        .and_then(|properties| properties.get("projects"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    let Some(from_projects) = from
        .properties
        .as_ref()
        .and_then(|properties| properties.get("projects"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    !into_projects.is_empty()
        && !from_projects.is_empty()
        && !into_projects.iter().any(|left| {
            from_projects
                .iter()
                .any(|right| project_values_match(left, right))
        })
}

fn project_values_match(left: &Value, right: &Value) -> bool {
    match (left.as_str(), right.as_str()) {
        (Some(left), Some(right)) => left.trim().eq_ignore_ascii_case(right.trim()),
        _ => left == right,
    }
}

impl KgPack {
    pub(crate) async fn handle_merge(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: MergeParams = deser(params)?;
        // By-ID resolution (including the hex-prefix form) is namespace-agnostic
        // (ADR-007 Rev 6 / #391 §3) — the Gate is the authz seam, not this lookup.
        let into_id = resolve_uuid_unfiltered(&p.into_id, &self.runtime, token).await?;
        let from_id = resolve_uuid_unfiltered(&p.from_id, &self.runtime, token).await?;
        let raw_kind = p.kind.as_deref().unwrap_or("entity");
        let spec = resolve_kind_spec(raw_kind, registry)?;
        let policy = parse_entity_policy(p.strategy.as_deref().unwrap_or("prefer_into"))?;
        let content_strategy =
            parse_content_strategy(p.content_strategy.as_deref().unwrap_or("append"))?;
        let dry_run = p.dry_run.unwrap_or(false);
        let force = p.force.unwrap_or(false);
        let reason = p.reason.clone();

        let summary = match spec {
            KindSpec::Entity { specific } => {
                ensure_entity_kind(&self.runtime, token, into_id, specific.as_deref()).await?;
                ensure_entity_kind(&self.runtime, token, from_id, specific.as_deref()).await?;
                let into_entity = self.runtime.get_entity(token, into_id).await?;
                let from_entity = self.runtime.get_entity(token, from_id).await?;
                if !force {
                    validate_entity_merge_floor(&into_entity, &from_entity)?;
                }
                self.runtime
                    .merge_entity_with_reason_and_force(
                        token,
                        into_id,
                        from_id,
                        policy,
                        content_strategy,
                        dry_run,
                        reason,
                        force,
                    )
                    .await?
            }
            KindSpec::Note { specific } => {
                ensure_note_kind(&self.runtime, token, into_id, specific.as_deref()).await?;
                ensure_note_kind(&self.runtime, token, from_id, specific.as_deref()).await?;
                self.runtime
                    .merge_note_with_reason(
                        token,
                        into_id,
                        from_id,
                        policy,
                        content_strategy,
                        dry_run,
                        reason,
                    )
                    .await?
            }
            KindSpec::Edge => {
                return Err(RuntimeError::InvalidInput(
                    "merge(kind=\"edge\") is unsupported".into(),
                ))
            }
            KindSpec::Event => return Err(immutable_event_error()),
            KindSpec::Proposal => {
                return Err(RuntimeError::InvalidInput(
                    "proposal events are immutable and cannot be merged".into(),
                ))
            }
        };
        to_json(&summary)
    }
}

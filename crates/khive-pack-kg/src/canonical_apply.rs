use khive_changeset::{ChangeSet, CreateTarget, Op};
use khive_runtime::atomic_prepare::apply_post_commit_effects_with_report;
use khive_runtime::retrieval::EmbeddingTruncationReport;
use khive_runtime::{
    AtomicOpFailure, AtomicRunOutcome, KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry,
};
use khive_types::event::ProposalPreimageStatus;
use khive_types::{Details, EntityDraft, KhiveError, NoteDraft, ProposalChangeset};
use uuid::Uuid;

pub(crate) fn decode_delta(
    delta: &str,
    outer_version: Option<u32>,
) -> Result<ChangeSet, RuntimeError> {
    let changeset = khive_changeset::from_ndjson(delta)
        .map_err(|error| RuntimeError::InvalidInput(format!("invalid canonical delta: {error}")))?;
    if outer_version.is_some_and(|version| version != changeset.envelope.schema_version) {
        return Err(RuntimeError::InvalidInput(
            "canonical schema_version differs from the delta envelope".into(),
        ));
    }
    let max = khive_types::pack::ATOMIC_MAX_OPS_DEFAULT;
    if changeset.ops.len() > max {
        return Err(RuntimeError::InvalidInput(format!(
            "canonical change-set has {} operations; maximum is {max}",
            changeset.ops.len()
        )));
    }
    if let Some(index) = changeset
        .ops
        .iter()
        .position(|op| matches!(op, Op::Merge(_)))
    {
        return Err(
            KhiveError::invalid_input("merge is KnownUnimplemented for canonical apply")
                .with_details(Details::new_owned([
                    ("op_index", index.to_string()),
                    ("reason", "KnownUnimplemented".into()),
                ]))
                .into(),
        );
    }
    Ok(changeset)
}

pub(crate) fn preimage_status(changeset: &ChangeSet) -> ProposalPreimageStatus {
    if changeset
        .ops
        .iter()
        .any(|op| matches!(op, Op::Update(_) | Op::Delete(_)))
    {
        ProposalPreimageStatus::Captured
    } else {
        ProposalPreimageStatus::None
    }
}

pub(crate) fn validate_canonical_admission(
    changeset: &ChangeSet,
    registry: &VerbRegistry,
) -> Result<(), RuntimeError> {
    for op in &changeset.ops {
        let note_maps = match op {
            Op::Create(create) => match &create.target {
                CreateTarget::Note(fields) => vec![&fields.properties],
                _ => Vec::new(),
            },
            Op::Update(update) => match (update.patch(), update.preimage()) {
                (
                    khive_changeset::UpdatePatch::Note(patch),
                    khive_changeset::UpdatePreimage::Note(preimage),
                ) => patch
                    .properties
                    .iter()
                    .chain(preimage.properties.iter())
                    .collect(),
                _ => Vec::new(),
            },
            Op::Delete(delete) => match &delete.preimage {
                khive_changeset::DeletePreimage::Note(note) => vec![&note.properties],
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        if note_maps
            .iter()
            .any(|properties| properties.contains_key("tags"))
        {
            return Err(RuntimeError::InvalidInput(
                "canonical note properties must not contain tags; use the independent tags field"
                    .into(),
            ));
        }
        // Reserved property keys on updates and links are decided by the runtime against the
        // row each operation meets in the writer (ADR-115 §4): an update may echo the stored
        // value, and a link's new edge has none to echo.
        if let Op::Create(create) = op {
            match &create.target {
                CreateTarget::Entity(fields) => {
                    let kind = crate::handlers::canonical_entity_kind(
                        fields.entity_kind.name(),
                        registry,
                    )?;
                    let properties = Some(
                        serde_json::to_value(&fields.properties)
                            .map_err(|error| RuntimeError::Internal(error.to_string()))?,
                    );
                    khive_runtime::secret_gate::reject_reserved_secret_gate_property(
                        properties.as_ref(),
                    )?;
                    if let Some(hook) = registry.find_kind_hook(&kind) {
                        hook.validate_proposal_entity(&EntityDraft {
                            kind,
                            name: fields.name.clone(),
                            description: fields.description.clone(),
                            properties,
                            tags: fields.tags.clone(),
                        })?;
                    }
                }
                CreateTarget::Note(fields) => {
                    let kind = crate::handlers::canonical_note_kind(&fields.note_kind, registry)?;
                    let properties = Some(
                        serde_json::to_value(&fields.properties)
                            .map_err(|error| RuntimeError::Internal(error.to_string()))?,
                    );
                    khive_runtime::secret_gate::reject_reserved_secret_gate_property(
                        properties.as_ref(),
                    )?;
                    if let Some(hook) = registry.find_kind_hook(&kind) {
                        hook.validate_proposal_note(&NoteDraft {
                            kind,
                            content: fields.content.clone(),
                            name: None,
                            properties,
                        })?;
                    }
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_proposal(
    changeset: &ProposalChangeset,
    registry: &VerbRegistry,
) -> Result<(), RuntimeError> {
    match changeset {
        ProposalChangeset::Canonical {
            schema_version,
            delta,
        } => validate_canonical_admission(&decode_delta(delta, Some(*schema_version))?, registry),
        ProposalChangeset::Compound { steps } => {
            for step in steps {
                reject_nested_canonical(step)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn reject_nested_canonical(changeset: &ProposalChangeset) -> Result<(), RuntimeError> {
    match changeset {
        ProposalChangeset::Canonical { .. } => Err(RuntimeError::InvalidInput(
            "Canonical must be the top-level proposal changeset".into(),
        )),
        ProposalChangeset::Compound { steps } => {
            for step in steps {
                reject_nested_canonical(step)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

pub(crate) enum CanonicalApplyOutcome {
    Committed {
        created_records: Vec<Uuid>,
        embedding_truncation: EmbeddingTruncationReport,
        reconciliation: Option<RuntimeError>,
    },
    RolledBack {
        error: RuntimeError,
    },
    OutcomeUnknown {
        error: RuntimeError,
    },
}

pub(crate) async fn apply_delta(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    token: &NamespaceToken,
    delta: &str,
    outer_version: Option<u32>,
) -> Result<CanonicalApplyOutcome, RuntimeError> {
    let changeset = decode_delta(delta, outer_version)?;
    validate_canonical_admission(&changeset, registry)?;
    let plans =
        khive_runtime::prepare_canonical_changeset(runtime, registry, token, &changeset).await?;
    let outcome = match khive_runtime::run_atomic_unit(runtime.sql().as_ref(), plans).await {
        Ok(outcome) => outcome,
        Err(error) => {
            return Ok(CanonicalApplyOutcome::OutcomeUnknown {
                error: RuntimeError::Storage(error.0),
            })
        }
    };
    match outcome {
        AtomicRunOutcome::RolledBack {
            failed_op_index,
            failure,
        } => {
            let mut details = vec![("op_index", failed_op_index.to_string())];
            let message = match failure {
                AtomicOpFailure::PreimageMismatch {
                    target_id,
                    field_path,
                } => {
                    details.push(("id", target_id.to_string()));
                    details.push(("field", field_path));
                    "captured preimage no longer matches"
                }
                AtomicOpFailure::OwnerSnapshotConflict { target_id, kind } => {
                    details.push(("id", target_id.to_string()));
                    details.push(("kind", kind));
                    details.push(("reason", "owner_snapshot_conflict".into()));
                    "owning kind validation snapshot changed"
                }
                _ => "canonical operation refused inside the writer transaction",
            };
            Ok(CanonicalApplyOutcome::RolledBack {
                error: KhiveError::conflict(message)
                    .with_details(Details::new_owned(details))
                    .into(),
            })
        }
        AtomicRunOutcome::Committed { post_commit } => {
            let created_records = changeset
                .ops
                .iter()
                .filter_map(|op| match op {
                    Op::Create(op) => Some(Uuid::from_bytes(*op.id.as_bytes())),
                    Op::Link(op) => Some(Uuid::from_bytes(*op.id.as_bytes())),
                    _ => None,
                })
                .collect();
            let mut embedding_truncation = EmbeddingTruncationReport::default();
            let reconciliation =
                match apply_post_commit_effects_with_report(runtime, token, post_commit).await {
                    Ok(outcomes) => {
                        for outcome in outcomes {
                            embedding_truncation.merge(outcome.truncation);
                        }
                        None
                    }
                    Err(error) => Some(error),
                };
            Ok(CanonicalApplyOutcome::Committed {
                created_records,
                embedding_truncation,
                reconciliation,
            })
        }
    }
}

#[cfg(test)]
#[path = "canonical_apply_tests.rs"]
mod tests;

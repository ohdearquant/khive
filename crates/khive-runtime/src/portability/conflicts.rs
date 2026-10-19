//! Explicit CLI import policies. Legacy library entry points remain in the parent module.

use std::collections::{HashMap, HashSet};

use khive_storage::types::SqlRow;
use khive_storage::{Entity, SqlStatement, SqlValue};
use khive_types::ImportKindPolicy;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{ExportedEdge, ExportedEntity, ImportValidation, KgArchive};
use crate::atomic_plan::PostCommitEffect;
use crate::atomic_prepare::{
    apply_post_commit_effects_with_report, canonical_refuse_registry_tags,
    prepare_import_entity_record, prepare_update_entity_record, validate_import_entity_record,
};
use crate::atomic_runner::{
    apply_plan_preserving_storage, run_prepared_atomic_unit, AtomicOpPlan, PlanExecutionFailure,
    PreparedAtomicError, PreparedAtomicOp, PreparedAtomicOutcome,
};
use crate::curation::EntityPatch;
use crate::operations::{canonical_edge_endpoints, Resolved};
use crate::{KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult};

// Bound both the number of synchronous writer operations and their materialized input.
const MAX_IMPORT_RECORDS: usize = 10_000;
const MAX_IMPORT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportConflictMode {
    #[default]
    Error,
    Skip,
    Update,
}

/// Opt-in CLI behavior; this does not change `import_kg` or its JSON/policy wrappers.
#[derive(Clone, Copy, Debug, Default)]
pub struct ImportOptions {
    pub on_conflict: ImportConflictMode,
    pub force: bool,
    pub dry_run: bool,
}

/// Reasons in the policy-aware report, separate from the legacy closed skip enum.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportPolicySkipCause {
    ExistingEdge,
    SkippedRecord,
    MissingSource,
    MissingTarget,
    EndpointContract,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImportPolicySkippedEdge {
    pub edge_id: Uuid,
    pub source: Uuid,
    pub target: Uuid,
    pub relation: khive_storage::EdgeRelation,
    pub cause: ImportPolicySkipCause,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ImportConflictSummary {
    pub entities_imported: usize,
    pub entities_inserted: usize,
    pub entities_updated: usize,
    pub entities_skipped: usize,
    pub edges_imported: usize,
    pub edges_inserted: usize,
    pub edges_updated: usize,
    pub edges_skipped: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_edges: Vec<ImportPolicySkippedEdge>,
    #[serde(default)]
    pub embedding_truncation: crate::retrieval::EmbeddingTruncationReport,
}

struct Probe {
    statement: SqlStatement,
    expected: serde_json::Value,
}

enum ImportEndpoint {
    Record(Box<Resolved>),
    Edge,
}

impl ImportEndpoint {
    fn record(&self) -> Option<&Resolved> {
        match self {
            Self::Record(record) => Some(record.as_ref()),
            Self::Edge => None,
        }
    }
}

struct PreparedImport {
    probes: Vec<Probe>,
    entities: Vec<AtomicOpPlan>,
    edges: Vec<SqlStatement>,
    summary: ImportConflictSummary,
}

fn invalid(message: impl Into<String>) -> RuntimeError {
    RuntimeError::InvalidInput(message.into())
}

fn archive_entity(record: &ExportedEntity, namespace: &str) -> Entity {
    Entity {
        entity_type: record.entity_type.clone(),
        description: record.description.clone(),
        properties: record.properties.clone().filter(|value| !value.is_null()),
        tags: record.tags.clone(),
        ..Entity::minimal(
            record.id,
            namespace,
            &record.kind,
            &record.name,
            record.created_at.timestamp_micros(),
            record.updated_at.timestamp_micros(),
        )
    }
}

fn archive_patch(record: &ExportedEntity) -> EntityPatch {
    EntityPatch {
        name: Some(record.name.clone()),
        description: record.description.clone().map(Some),
        entity_type: record.entity_type.clone().map(Some),
        properties: record.properties.clone().filter(|value| !value.is_null()),
        tags: (!record.tags.is_empty()).then(|| record.tags.clone()),
    }
}

fn entity_statement(id: Uuid) -> SqlStatement {
    SqlStatement::new(
        include_str!("../../sql/import-entity-state.sql"),
        vec![SqlValue::Text(id.to_string())],
    )
    .labelled("import-entity-state")
}

fn edge_statement(id: Uuid) -> SqlStatement {
    SqlStatement::new(
        include_str!("../../sql/import-edge-state.sql"),
        vec![SqlValue::Text(id.to_string())],
    )
    .labelled("import-edge-state")
}

fn key_statement(namespace: &str, record: &ExportedEdge) -> SqlStatement {
    let (source, target) = canonical_edge_endpoints(record.relation, record.source, record.target);
    SqlStatement::new(
        include_str!("../../sql/import-edge-key.sql"),
        vec![
            SqlValue::Text(namespace.to_owned()),
            SqlValue::Text(source.to_string()),
            SqlValue::Text(target.to_string()),
            SqlValue::Text(record.relation.to_string()),
        ],
    )
    .labelled("import-edge-key")
}

async fn read_probe(
    runtime: &KhiveRuntime,
    statement: SqlStatement,
) -> RuntimeResult<(Probe, Vec<SqlRow>)> {
    // Release the checkout before any hook/plan preparation or subsequent await.
    let rows = {
        let mut reader = runtime.sql().reader().await?;
        reader.query_all(statement.clone()).await?
    };
    let expected = serde_json::to_value(&rows)
        .map_err(|error| RuntimeError::Internal(format!("encode import precondition: {error}")))?;
    Ok((
        Probe {
            statement,
            expected,
        },
        rows,
    ))
}

async fn read_endpoint(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    probes: &mut Vec<Probe>,
) -> RuntimeResult<Option<ImportEndpoint>> {
    // Capture all substrate candidates, including absence and higher-precedence
    // identities, before canonical by-ID resolution (entity, note, event, edge).
    let (probe, _) = read_probe(
        runtime,
        SqlStatement::new(
            include_str!("../../sql/import-endpoint-state.sql"),
            vec![SqlValue::Text(id.to_string())],
        )
        .labelled("import-endpoint-state"),
    )
    .await?;
    probes.push(probe);
    if let Some(record) = runtime.resolve_edge_endpoint(token, id).await? {
        return Ok(Some(ImportEndpoint::Record(Box::new(record))));
    }
    Ok(runtime
        .get_edge(token, id)
        .await?
        .map(|_| ImportEndpoint::Edge))
}

fn validate_endpoints(
    runtime: &KhiveRuntime,
    edge: &ExportedEdge,
    source: &ImportEndpoint,
    target: &ImportEndpoint,
    rules: &[khive_types::EdgeEndpointRule],
) -> RuntimeResult<()> {
    // Resolved deliberately has no Edge variant. The canonical annotates rule
    // also admits an existing live edge as a target; never impersonate a record.
    if edge.relation == khive_storage::EdgeRelation::Annotates
        && matches!(source.record(), Some(Resolved::Note(_)))
        && matches!(target, ImportEndpoint::Edge)
    {
        if edge.source == edge.target {
            return Err(invalid(
                "self-loop edges are not allowed: source_id and target_id must be different",
            ));
        }
        return Ok(());
    }
    if matches!(source, ImportEndpoint::Edge) || matches!(target, ImportEndpoint::Edge) {
        return Err(invalid(format!(
            "archive edge {}: a stored edge is only a valid annotates target from a note",
            edge.edge_id
        )));
    }
    runtime.validate_link_endpoints_by_resolved_with_rules(
        edge.source,
        edge.target,
        edge.relation,
        source.record(),
        target.record(),
        rules,
    )
}

fn skipped(
    record: &ExportedEdge,
    cause: ImportPolicySkipCause,
    error: Option<String>,
) -> ImportPolicySkippedEdge {
    ImportPolicySkippedEdge {
        edge_id: record.edge_id,
        source: record.source,
        target: record.target,
        relation: record.relation,
        cause,
        error,
    }
}

impl KhiveRuntime {
    /// Validate policy-aware input before opening the destination. Imports admit at most
    /// 10,000 records and 64 MiB of serialized archive data per atomic unit.
    pub fn validate_kg_import_options(
        &self,
        archive: &KgArchive,
        token: &NamespaceToken,
        policy: ImportKindPolicy,
    ) -> RuntimeResult<ImportValidation> {
        let count = archive
            .entities
            .len()
            .checked_add(archive.edges.len())
            .ok_or_else(|| invalid("import record count overflow"))?;
        if count > MAX_IMPORT_RECORDS {
            return Err(invalid(format!(
                "import has {count} records; maximum is {MAX_IMPORT_RECORDS}"
            )));
        }
        struct Budget(usize);
        impl std::io::Write for Budget {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self
                    .0
                    .checked_add(bytes.len())
                    .filter(|size| *size <= MAX_IMPORT_BYTES)
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "archive exceeds 64 MiB import limit",
                        )
                    })?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        serde_json::to_writer(Budget(0), archive)
            .map_err(|error| invalid(format!("import archive budget: {error}")))?;
        let validation = self.validate_kg_import(archive, policy)?;
        let mut ids = HashSet::new();
        for record in &archive.entities {
            if !ids.insert(record.id) {
                return Err(invalid(format!(
                    "duplicate archive entity UUID {}",
                    record.id
                )));
            }
            validate_import_entity_record(
                self,
                token,
                &archive_entity(record, token.namespace().as_str()),
                policy,
            )?;
        }
        Ok(validation)
    }

    /// Apply explicit conflict modes after complete preflight. All row changes share one
    /// existing storage owner; ordinary entity reindexing runs only after commit. Preview
    /// uses the same prepared outcomes but never acquires a writer or executes effects.
    pub async fn import_kg_with_options(
        &self,
        archive: &KgArchive,
        token: &NamespaceToken,
        policy: ImportKindPolicy,
        options: ImportOptions,
    ) -> RuntimeResult<ImportConflictSummary> {
        let validation = self.validate_kg_import_options(archive, token, policy)?;
        let prepared = self.prepare_import(archive, token, policy, options).await?;
        if options.dry_run {
            return Ok(prepared.summary);
        }
        for kind in validation.unknown_entity_kinds {
            tracing::warn!(target: "khive.import.schema",
                kind = %crate::secret_gate::bounded_masked_log_text(&kind),
                "preserving unknown entity kind during explicit import");
        }
        self.apply_prepared_import(token, prepared).await
    }

    async fn apply_prepared_import(
        &self,
        token: &NamespaceToken,
        prepared: PreparedImport,
    ) -> RuntimeResult<ImportConflictSummary> {
        // Store constructors/table preparation are outside the suspend-free owner.
        self.entities(token)?;
        self.graph(token)?;
        self.text(token)?;
        self.events(token)?;
        let op: PreparedAtomicOp<ImportConflictSummary, RuntimeError> = Box::new(move |writer| {
            Box::pin(async move {
                // Revalidate every identity, endpoint and natural-key observation BEFORE DML.
                // A changed destination is a retryable refusal, never a stale overwrite/skip count.
                for probe in prepared.probes {
                    let rows = writer.query_all(probe.statement).await?;
                    let actual = serde_json::to_value(rows).map_err(|error| {
                        PreparedAtomicError::Refused {
                            failure: RuntimeError::Internal(format!(
                                "encode import precondition: {error}"
                            )),
                            message: "import precondition encoding failed".into(),
                        }
                    })?;
                    if actual != probe.expected {
                        return Err(PreparedAtomicError::Refused {
                            failure: invalid(
                                "import destination changed during preflight; retry the import",
                            ),
                            message: "import destination changed during preflight".into(),
                        });
                    }
                }
                let mut effects = Vec::<PostCommitEffect>::new();
                for plan in prepared.entities {
                    match apply_plan_preserving_storage(writer, &plan, false).await {
                        Ok(applied) => effects.extend(applied.effect),
                        Err(PlanExecutionFailure::Storage { source, .. }) => {
                            return Err(PreparedAtomicError::Storage(source))
                        }
                        Err(PlanExecutionFailure::Operation(failure)) => {
                            return Err(PreparedAtomicError::Refused {
                                failure: invalid(format!(
                                    "import entity precondition failed: {failure:?}"
                                )),
                                message: "import entity precondition failed".into(),
                            });
                        }
                    }
                }
                for statement in prepared.edges {
                    if writer.execute(statement).await? != 1 {
                        return Err(PreparedAtomicError::Refused {
                            failure: invalid("import edge precondition failed"),
                            message: "import edge precondition failed".into(),
                        });
                    }
                }
                Ok((prepared.summary, effects))
            })
        });
        let access = self.sql();
        match run_prepared_atomic_unit(access.as_ref(), op).await? {
            PreparedAtomicOutcome::RolledBack(error) => Err(error),
            PreparedAtomicOutcome::Committed {
                mut value,
                post_commit,
            } => {
                for outcome in
                    apply_post_commit_effects_with_report(self, token, post_commit).await?
                {
                    value.embedding_truncation.merge(outcome.truncation);
                }
                Ok(value)
            }
        }
    }

    async fn prepare_import(
        &self,
        archive: &KgArchive,
        token: &NamespaceToken,
        policy: ImportKindPolicy,
        options: ImportOptions,
    ) -> RuntimeResult<PreparedImport> {
        let namespace = token.namespace().as_str();
        let mut result = PreparedImport {
            probes: Vec::new(),
            entities: Vec::new(),
            edges: Vec::new(),
            summary: ImportConflictSummary::default(),
        };
        let mut endpoints = HashMap::<Uuid, Option<ImportEndpoint>>::new();
        let mut skipped_entities = HashSet::new();
        for record in &archive.entities {
            let (probe, rows) = read_probe(self, entity_statement(record.id)).await?;
            if rows.len() > 1 {
                return Err(invalid(format!("entity UUID {} is ambiguous", record.id)));
            }
            result.probes.push(probe);
            if !rows.is_empty() && options.on_conflict == ImportConflictMode::Error {
                return Err(invalid(format!(
                    "archive entity UUID {} already exists",
                    record.id
                )));
            }
            if !rows.is_empty() && options.on_conflict == ImportConflictMode::Skip {
                result.summary.entities_skipped += 1;
                skipped_entities.insert(record.id);
                continue;
            }
            let entity = if rows.is_empty() {
                let entity = archive_entity(record, namespace);
                result.entities.push(prepare_import_entity_record(
                    self,
                    token,
                    entity.clone(),
                    policy,
                )?);
                result.summary.entities_inserted += 1;
                entity
            } else {
                let current = self
                    .entities(token)?
                    .get_entity(record.id)
                    .await?
                    .filter(|entity| entity.namespace == namespace && entity.deleted_at.is_none())
                    .ok_or_else(|| {
                        invalid(format!(
                            "entity {} cannot be updated in the destination namespace",
                            record.id
                        ))
                    })?;
                // The archive's tags cannot attest ownership of the stored row.
                // Refuse before a patch can replace its properties or tags.
                canonical_refuse_registry_tags(&current.tags, "update")?;
                if current.kind != record.kind {
                    return Err(invalid(format!(
                        "archive entity {} kind differs from the stored kind",
                        record.id
                    )));
                }
                let (entity, plan) = prepare_update_entity_record(
                    self,
                    token,
                    record.id,
                    archive_patch(record),
                    Some(current.version),
                )
                .await?;
                result.entities.push(plan);
                result.summary.entities_updated += 1;
                entity
            };
            endpoints.insert(
                record.id,
                Some(ImportEndpoint::Record(Box::new(Resolved::Entity(entity)))),
            );
        }
        let rules = self.pack_edge_rules();
        let mut edge_ids = HashSet::new();
        let mut edge_keys = HashSet::new();
        for record in &archive.edges {
            let (probe, stored_ids) = read_probe(self, edge_statement(record.edge_id)).await?;
            if stored_ids.len() > 1 {
                return Err(invalid(format!(
                    "edge UUID {} is ambiguous",
                    record.edge_id
                )));
            }
            result.probes.push(probe);
            let (key_probe, key_rows) = read_probe(self, key_statement(namespace, record)).await?;
            result.probes.push(key_probe);
            if key_rows.iter().any(|row| {
                row.text("id")
                    .map_or(true, |id| id != record.edge_id.to_string())
            }) {
                return Err(invalid(format!(
                    "archive edge {} triple is held by another stored UUID",
                    record.edge_id
                )));
            }
            if !stored_ids.is_empty() && options.on_conflict == ImportConflictMode::Error {
                return Err(invalid(format!(
                    "archive edge UUID {} already exists",
                    record.edge_id
                )));
            }
            // Force cannot hide a stored-identity/endpoint mismatch by turning it
            // into an endpoint skip. Validate the immutable identity first.
            let current_edge =
                if !stored_ids.is_empty() && options.on_conflict == ImportConflictMode::Update {
                    let current = self
                        .get_edge_including_deleted(token, record.edge_id)
                        .await?
                        .filter(|edge| edge.namespace == namespace && edge.deleted_at.is_none())
                        .ok_or_else(|| {
                            invalid(format!(
                                "edge {} cannot be updated in the destination namespace",
                                record.edge_id
                            ))
                        })?;
                    let (source, target) =
                        canonical_edge_endpoints(record.relation, record.source, record.target);
                    if current.source_id != source || current.target_id != target {
                        return Err(invalid(format!(
                            "archive edge {} endpoints differ from stored endpoints",
                            record.edge_id
                        )));
                    }
                    Some(current)
                } else {
                    None
                };
            if skipped_entities.contains(&record.source)
                || skipped_entities.contains(&record.target)
            {
                result.summary.skipped_edges.push(skipped(
                    record,
                    ImportPolicySkipCause::SkippedRecord,
                    None,
                ));
                continue;
            }
            if !stored_ids.is_empty() && options.on_conflict == ImportConflictMode::Skip {
                result.summary.skipped_edges.push(skipped(
                    record,
                    ImportPolicySkipCause::ExistingEdge,
                    None,
                ));
                continue;
            }
            for id in [record.source, record.target] {
                if let std::collections::hash_map::Entry::Vacant(entry) = endpoints.entry(id) {
                    entry.insert(read_endpoint(self, token, id, &mut result.probes).await?);
                }
            }
            let source = endpoints.get(&record.source).and_then(Option::as_ref);
            let target = endpoints.get(&record.target).and_then(Option::as_ref);
            let refusal = match (source, target) {
                (None, _) => Some((
                    ImportPolicySkipCause::MissingSource,
                    format!("link source {} not found", record.source),
                )),
                (Some(_), None) => Some((
                    ImportPolicySkipCause::MissingTarget,
                    format!("link target {} not found", record.target),
                )),
                (Some(source), Some(target)) => {
                    match validate_endpoints(self, record, source, target, &rules) {
                        Ok(()) => None,
                        Err(
                            error @ (RuntimeError::InvalidInput(_) | RuntimeError::NotFound(_)),
                        ) => Some((ImportPolicySkipCause::EndpointContract, error.to_string())),
                        Err(error) => return Err(error),
                    }
                }
            };
            if let Some((cause, error)) = refusal {
                if !options.force {
                    return Err(invalid(format!("archive edge {}: {error}", record.edge_id)));
                }
                result.summary.skipped_edges.push(skipped(
                    record,
                    cause,
                    (cause == ImportPolicySkipCause::EndpointContract).then_some(error),
                ));
                continue;
            }
            let (source_id, target_id) =
                canonical_edge_endpoints(record.relation, record.source, record.target);
            // Repeated refused/skipped edges retain one report per input item. For
            // edges that would be written, reject duplicate identities and tuples
            // before the owner starts so no plan can overwrite another input row.
            if !edge_ids.insert(record.edge_id) {
                return Err(invalid(format!(
                    "duplicate archive edge UUID {}",
                    record.edge_id
                )));
            }
            if !edge_keys.insert((source_id, target_id, record.relation)) {
                return Err(invalid(format!(
                    "duplicate archive edge triple at {}",
                    record.edge_id
                )));
            }
            let mut edge = khive_storage::Edge {
                id: record.edge_id.into(),
                namespace: namespace.to_owned(),
                source_id,
                target_id,
                relation: record.relation,
                weight: record.weight,
                created_at: record.created_at,
                updated_at: record.updated_at,
                deleted_at: None,
                metadata: record.properties.clone().filter(|value| !value.is_null()),
                target_backend: None,
            };
            if stored_ids.is_empty() {
                result.edges.push(
                    khive_db::stores::graph::edge_insert_only_guarded_by_endpoints_statement(&edge),
                );
                result.summary.edges_inserted += 1;
            } else {
                let current = current_edge.ok_or_else(|| {
                    RuntimeError::Internal("missing prepared edge preimage".into())
                })?;
                edge.created_at = current.created_at;
                edge.target_backend = current.target_backend.clone();
                edge.metadata = edge.metadata.or_else(|| current.metadata.clone());
                let minimum = current
                    .updated_at
                    .timestamp_micros()
                    .checked_add(1)
                    .ok_or_else(|| invalid("stored edge revision cannot advance"))?;
                edge.updated_at = chrono::DateTime::from_timestamp_micros(
                    chrono::Utc::now().timestamp_micros().max(minimum),
                )
                .ok_or_else(|| invalid("edge update timestamp is out of range"))?;
                result.edges.push(
                    khive_db::stores::graph::edge_replace_if_unchanged_statement(
                        &edge,
                        current.updated_at,
                        current.deleted_at,
                    ),
                );
                result.summary.edges_updated += 1;
            }
        }
        result.summary.entities_imported =
            result.summary.entities_inserted + result.summary.entities_updated;
        result.summary.edges_imported =
            result.summary.edges_inserted + result.summary.edges_updated;
        result.summary.edges_skipped = result.summary.skipped_edges.len();
        Ok(result)
    }
}

#[cfg(test)]
#[path = "conflicts_tests.rs"]
mod tests;

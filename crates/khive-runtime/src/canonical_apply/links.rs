use khive_storage::{Edge, EdgeUpsertDisposition, SqlStatement, SqlValue, SqlWriter};
use khive_types::{EdgeEndpointRule, EdgeRelation, EventKind, SubstrateKind};
use uuid::Uuid;

use crate::atomic_plan::{AffectedRowGuard, LinkPlan, PlanStatement};
use crate::atomic_runner::{AtomicOpFailure, AtomicOpPlan};
use crate::operations::{canonical_edge_endpoints, merge_dependency_kind, pack_rule_allows};
use crate::{EdgeEndpointKind, NamespaceToken, RuntimeError, RuntimeResult};

use super::{failure, Record};

pub(super) fn endpoints(
    source: &Record,
    target: &Record,
    relation: EdgeRelation,
    rules: &[EdgeEndpointRule],
) -> RuntimeResult<(EdgeEndpointKind, EdgeEndpointKind)> {
    if !source.live() || !target.live() {
        return Err(RuntimeError::InvalidInput(
            "canonical link endpoint is not live".into(),
        ));
    }
    if relation == EdgeRelation::Annotates {
        if matches!(source, Record::Note(_)) {
            return Ok((source.kind(), target.kind()));
        }
    } else if crate::pack::is_special_relation(relation) {
        match (source, target) {
            (Record::Note(_), Record::Note(_)) => return Ok((source.kind(), target.kind())),
            (Record::Entity(a), Record::Entity(b))
                if khive_types::base_entity_rule_allows(&a.kind, relation, &b.kind) =>
            {
                return Ok((source.kind(), target.kind()))
            }
            _ => {}
        }
    } else {
        let a = source.resolved();
        let b = target.resolved();
        if pack_rule_allows(rules, relation, a.as_ref(), b.as_ref())
            || matches!((source,target),(Record::Entity(a),Record::Entity(b))
                if khive_types::base_entity_rule_allows(&a.kind,relation,&b.kind))
        {
            return Ok((source.kind(), target.kind()));
        }
    }
    Err(RuntimeError::InvalidInput(
        "canonical link endpoint relation is not admitted".into(),
    ))
}

pub(super) fn draft(
    op: &khive_changeset::LinkOp,
    source: &Record,
    target: &Record,
    now: i64,
) -> RuntimeResult<Edge> {
    if op.source == op.target {
        return Err(RuntimeError::InvalidInput(
            "self-loop edges are not allowed".into(),
        ));
    }
    crate::operations::validate_edge_weight(op.weight)?;
    let (source_id, target_id) =
        canonical_edge_endpoints(op.relation, op.source.into(), op.target.into());
    let mut metadata = Some(
        serde_json::to_value(&op.properties)
            .map_err(|_| RuntimeError::InvalidInput("invalid canonical edge properties".into()))?,
    );
    if op.relation == EdgeRelation::DependsOn {
        let (a, b) = if source_id == Uuid::from(op.source) {
            (source, target)
        } else {
            (target, source)
        };
        if let (Record::Entity(a), Record::Entity(b)) = (a, b) {
            metadata = merge_dependency_kind(&a.kind, &b.kind, metadata);
        }
    }
    crate::operations::validate_edge_metadata(op.relation, metadata.as_ref())?;
    crate::secret_gate::check_json_at(
        metadata.as_ref().expect("properties present"),
        "edge",
        "properties",
    )?;
    crate::secret_gate::reject_reserved_secret_gate_property(metadata.as_ref())?;
    let now = chrono::DateTime::from_timestamp_micros(now)
        .ok_or_else(|| RuntimeError::InvalidInput("invalid canonical link timestamp".into()))?;
    Ok(Edge {
        id: Uuid::from(op.id).into(),
        namespace: op.namespace.as_str().into(),
        source_id,
        target_id,
        relation: op.relation,
        weight: op.weight,
        created_at: now,
        updated_at: now,
        deleted_at: None,
        metadata,
        target_backend: None,
    })
}

pub(super) async fn materialize(
    writer: &mut dyn SqlWriter,
    token: &NamespaceToken,
    edge: &Edge,
    rules: &[EdgeEndpointRule],
) -> Result<AtomicOpPlan, AtomicOpFailure> {
    let source = super::read_writer(writer, edge.source_id)
        .await?
        .ok_or_else(|| failure("canonical link source is missing"))?;
    let target = super::read_writer(writer, edge.target_id)
        .await?
        .ok_or_else(|| failure("canonical link target is missing"))?;
    let (source_kind, target_kind) =
        endpoints(&source, &target, edge.relation, rules).map_err(|e| failure(e.to_string()))?;
    let rows = writer
        .query_all(
            SqlStatement::new(
                include_str!("../../sql/canonical_edge_key_exists.sql"),
                vec![
                    SqlValue::Text(edge.namespace.clone()),
                    SqlValue::Text(edge.source_id.to_string()),
                    SqlValue::Text(edge.target_id.to_string()),
                    SqlValue::Text(edge.relation.to_string()),
                ],
            )
            .labelled("canonical-edge-key-collision"),
        )
        .await
        .map_err(|e| failure(e.to_string()))?;
    if !rows.is_empty() {
        return Err(failure("canonical edge natural key already exists"));
    }
    let metadata = edge
        .metadata
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| failure(e.to_string()))?;
    let mut statements = vec![PlanStatement {
        statement: khive_db::stores::graph::edge_insert_new_guarded_by_endpoints_statement(
            &edge.namespace,
            Uuid::from(edge.id),
            edge.source_id,
            edge.target_id,
            edge.relation,
            edge.weight,
            edge.created_at.timestamp_micros(),
            metadata.as_deref(),
        ),
        guard: Some(AffectedRowGuard::exactly(1)),
    }];
    statements.extend(crate::atomic_prepare::event_append_statements(token,&edge.namespace,"link",
        EventKind::LinkCreated,SubstrateKind::Entity,Uuid::from(edge.id),serde_json::json!({
            "id":edge.id,"namespace":edge.namespace,"mutation":"created","source_id":edge.source_id,
            "target_id":edge.target_id,"relation":edge.relation,"weight":edge.weight,"metadata":edge.metadata,
            "previous":null,"source_kind":source_kind.name(),"target_kind":target_kind.name()}))
        .map_err(|e|failure(e.to_string()))?);
    Ok(AtomicOpPlan::Link(LinkPlan {
        source_id: edge.source_id,
        target_id: edge.target_id,
        statements,
        disposition: EdgeUpsertDisposition::Created,
    }))
}

use khive_storage::{EdgeInsertIfAbsentOutcome, GraphStatementGuard};

use super::{
    accepted_entity_relations_description, base_entity_rule_allows, canonical_edge_endpoint_kinds,
    canonical_edge_endpoints, compose_graph_mutation_events, merge_dependency_kind,
    pack_rule_allows, validate_edge_metadata, validate_edge_weight, Edge, EdgeEndpointKind,
    EdgeRelation, EdgeUpsertDisposition, EdgeUpsertRefusal, EdgeUpsertRequest, EdgeUpsertResult,
    Event, EventKind, GraphMutationOutcome, GraphMutationPreconditions, GraphMutationRequest,
    GuardedEdgeUpsertOutcome, GuardedWriteFailure, KhiveRuntime, LinkId, LinkSpec, NamespaceToken,
    Resolved, RuntimeError, RuntimeResult, SubstrateKind, Uuid,
};

pub(super) fn guarded_link_batch_failure(
    spec: &LinkSpec,
    entry_index: usize,
    missing: khive_storage::MissingEndpoints,
) -> GuardedWriteFailure {
    // Storage flags describe the canonical edge built from this spec, not the
    // caller's potentially reversed spelling of a symmetric relation.
    let (source_id, target_id) =
        canonical_edge_endpoints(spec.relation, spec.source_id, spec.target_id);
    GuardedWriteFailure {
        entry_index: Some(entry_index),
        missing_source: missing.source.then_some(source_id),
        missing_target: missing.target.then_some(target_id),
    }
}

impl KhiveRuntime {
    // ---- Edge operations ----

    /// Validate that `source_id` and `target_id` are legal endpoints for `relation`.
    ///
    /// Centralises the three-case relation contract so that both
    /// `link()` and `update_edge()` share identical enforcement:
    ///
    /// - `annotates`: source MUST be a note; target may be any substrate.
    /// - `supersedes` / `supports` / `refutes`: same-substrate only (note→note or entity→entity).
    /// - All other relations: both endpoints MUST be entities.
    ///
    /// Returns the validated endpoint substrates when valid; otherwise
    /// `InvalidInput` or `NotFound` for an invalid endpoint pair.
    ///
    /// `pub(crate)`: the atomic prepare pass (`crate::atomic_prepare`) reuses
    /// this exact endpoint-type validation during its async prepare step,
    /// before building a `LinkPlan`, rather than re-deriving the checks.
    pub(crate) async fn validate_edge_relation_endpoints(
        &self,
        token: &NamespaceToken,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
    ) -> RuntimeResult<(EdgeEndpointKind, EdgeEndpointKind)> {
        if source_id == target_id {
            return Err(RuntimeError::InvalidInput(
                "self-loop edges are not allowed: source_id and target_id must be different".into(),
            ));
        }
        if relation == EdgeRelation::Annotates {
            // Source must be a note. By-ID endpoint resolution is namespace-agnostic:
            // link consumes two by-ID endpoints, so it must resolve exactly what
            // get() resolves, regardless of caller namespace.
            match self.resolve_edge_endpoint(token, source_id).await? {
                Some(Resolved::Note(_)) => {}
                Some(_) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "annotates source {source_id} must be a note"
                    )));
                }
                None => {
                    // Existing edge used as annotates source: wrong kind, not absent.
                    if self.get_edge(token, source_id).await?.is_some() {
                        return Err(RuntimeError::InvalidInput(format!(
                            "annotates source {source_id} must be a note"
                        )));
                    }
                    return Err(RuntimeError::NotFound(format!(
                        "link source {source_id} not found"
                    )));
                }
            }
            // Target may be any substrate (entity, note, event, or edge) — by-ID, unfiltered.
            let target_kind = match self.resolve_edge_endpoint(token, target_id).await? {
                Some(Resolved::Entity(_)) => EdgeEndpointKind::Entity,
                Some(Resolved::Note(_)) => EdgeEndpointKind::Note,
                Some(Resolved::Event(_)) => EdgeEndpointKind::Event,
                Some(Resolved::PackRecord { .. }) => {
                    return Err(RuntimeError::InvalidInput(
                        "pack-private record is not a valid edge endpoint for annotates".into(),
                    ));
                }
                None => match self.get_edge(token, target_id).await {
                    Ok(Some(_)) => EdgeEndpointKind::Edge,
                    Ok(None) | Err(RuntimeError::NotFound(_)) => {
                        return Err(RuntimeError::NotFound(format!(
                            "link target {target_id} not found"
                        )));
                    }
                    Err(error) => return Err(error),
                },
            };
            return Ok((EdgeEndpointKind::Note, target_kind));
        } else if crate::pack::is_special_relation(relation) {
            // supersedes / supports / refutes: same-substrate only (note→note or entity→entity).
            // Event and edge endpoints are invalid regardless of the other endpoint.
            // Endpoint resolution is by-ID and namespace-agnostic.
            let rel_name = relation.as_str();
            let src = match self.resolve_edge_endpoint(token, source_id).await? {
                Some(r) => r,
                None => {
                    if self.get_edge(token, source_id).await?.is_some() {
                        return Err(RuntimeError::InvalidInput(format!(
                            "{rel_name} source {source_id} must be a note or entity (got edge)"
                        )));
                    }
                    return Err(RuntimeError::NotFound(format!(
                        "link source {source_id} not found"
                    )));
                }
            };
            let tgt = match self.resolve_edge_endpoint(token, target_id).await? {
                Some(r) => r,
                None => {
                    if self.get_edge(token, target_id).await?.is_some() {
                        return Err(RuntimeError::InvalidInput(format!(
                            "{rel_name} target {target_id} must be a note or entity (got edge)"
                        )));
                    }
                    return Err(RuntimeError::NotFound(format!(
                        "link target {target_id} not found"
                    )));
                }
            };
            return match (&src, &tgt) {
                (Resolved::Entity(src_e), Resolved::Entity(tgt_e)) => {
                    if !base_entity_rule_allows(&src_e.kind, relation, &tgt_e.kind) {
                        let legal_relations = accepted_entity_relations_description(
                            &self.pack_edge_rules(),
                            &src_e.kind,
                            src_e.entity_type.as_deref(),
                            &tgt_e.kind,
                            tgt_e.entity_type.as_deref(),
                        );
                        let rule_hint = match relation {
                            EdgeRelation::Supports | EdgeRelation::Refutes => {
                                "requires concept|document|dataset|artifact -> concept \
                                 (or same-substrate note -> note)"
                            }
                            _ => "requires same-kind entity endpoints",
                        };
                        return Err(RuntimeError::InvalidInput(format!(
                            "({}) -[{rel_name}]-> ({}) is not in the base endpoint \
                             allowlist; {rel_name} {rule_hint}; currently legal relations for \
                             {} -> {} under the loaded endpoint rules: {legal_relations}",
                            src_e.kind, tgt_e.kind, src_e.kind, tgt_e.kind
                        )));
                    }
                    Ok((EdgeEndpointKind::Entity, EdgeEndpointKind::Entity))
                }
                (Resolved::Note(_), Resolved::Note(_)) => {
                    Ok((EdgeEndpointKind::Note, EdgeEndpointKind::Note))
                }
                (Resolved::Event(_), _) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "{rel_name} does not apply to events; source {source_id} is an event"
                    )));
                }
                (_, Resolved::Event(_)) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "{rel_name} does not apply to events; target {target_id} is an event"
                    )));
                }
                (Resolved::Entity(_), Resolved::Note(_)) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "{rel_name} endpoints must be the same substrate (note→note or entity→entity); \
                         got source={source_id} (entity) target={target_id} (note)"
                    )));
                }
                (Resolved::Note(_), Resolved::Entity(_)) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "{rel_name} endpoints must be the same substrate (note→note or entity→entity); \
                         got source={source_id} (note) target={target_id} (entity)"
                    )));
                }
                (Resolved::PackRecord { .. }, _) | (_, Resolved::PackRecord { .. }) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "pack-private record is not a valid edge endpoint for {rel_name}"
                    )));
                }
            };
        } else {
            // All remaining base relations require entity→entity with kind-level
            // restrictions (see base allowlist). Packs may extend the allowlist
            // additively via EDGE_RULES.
            //
            // Strategy: resolve both endpoints once (by-ID, unfiltered), consult pack
            // rules; on miss, fall through to the original base-rule error messages.
            let src_res = self.resolve_edge_endpoint(token, source_id).await?;
            let tgt_res = self.resolve_edge_endpoint(token, target_id).await?;
            let pack_rules = self.pack_edge_rules();

            if pack_rule_allows(&pack_rules, relation, src_res.as_ref(), tgt_res.as_ref()) {
                let kind = |resolved: Option<&Resolved>| match resolved {
                    Some(Resolved::Entity(_)) => Some(EdgeEndpointKind::Entity),
                    Some(Resolved::Note(_)) => Some(EdgeEndpointKind::Note),
                    _ => None,
                };
                return match (kind(src_res.as_ref()), kind(tgt_res.as_ref())) {
                    (Some(source_kind), Some(target_kind)) => Ok((source_kind, target_kind)),
                    _ => Err(RuntimeError::Internal(
                        "pack endpoint rule admitted an unsupported substrate".into(),
                    )),
                };
            }

            // Substrate check: both endpoints must be entities.
            let (src_kind, src_entity_type) = match src_res.as_ref() {
                Some(Resolved::Entity(e)) => (e.kind.as_str(), e.entity_type.as_deref()),
                Some(_) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "link source {source_id} must be an entity for relation {relation:?} \
                         (only `annotates` crosses substrates)"
                    )));
                }
                None => {
                    if self.get_edge(token, source_id).await?.is_some() {
                        return Err(RuntimeError::InvalidInput(format!(
                            "link source {source_id} must be an entity for relation {relation:?} \
                             (only `annotates` crosses substrates)"
                        )));
                    }
                    return Err(RuntimeError::NotFound(format!(
                        "link source {source_id} not found"
                    )));
                }
            };
            let (tgt_kind, tgt_entity_type) = match tgt_res.as_ref() {
                Some(Resolved::Entity(e)) => (e.kind.as_str(), e.entity_type.as_deref()),
                Some(_) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "link target {target_id} must be an entity for relation {relation:?} \
                         (only `annotates` crosses substrates)"
                    )));
                }
                None => {
                    if self.get_edge(token, target_id).await?.is_some() {
                        return Err(RuntimeError::InvalidInput(format!(
                            "link target {target_id} must be an entity for relation {relation:?} \
                             (only `annotates` crosses substrates)"
                        )));
                    }
                    return Err(RuntimeError::NotFound(format!(
                        "link target {target_id} not found"
                    )));
                }
            };
            if !base_entity_rule_allows(src_kind, relation, tgt_kind) {
                let legal_relations = accepted_entity_relations_description(
                    &pack_rules,
                    src_kind,
                    src_entity_type,
                    tgt_kind,
                    tgt_entity_type,
                );
                return Err(RuntimeError::InvalidInput(format!(
                    "({src_kind}) -[{}]-> ({tgt_kind}) is not in the base endpoint \
                     allowlist; use pack EDGE_RULES to extend the allowlist; currently legal \
                     relations for {src_kind} -> {tgt_kind} under the loaded endpoint rules: \
                     {legal_relations}",
                    relation.as_str()
                )));
            }
        }
        Ok((EdgeEndpointKind::Entity, EdgeEndpointKind::Entity))
    }

    /// Public delegator for cross-backend link validation.
    ///
    /// Exposes `validate_edge_relation_endpoints` for the `SubstrateCoordinator`
    /// so it can validate the relation before writing the edge on the source backend.
    pub async fn validate_link_endpoints(
        &self,
        token: &NamespaceToken,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
    ) -> RuntimeResult<()> {
        self.validate_edge_relation_endpoints(token, source_id, target_id, relation)
            .await
            .map(|_| ())
    }

    /// Validate an edge relation using pre-fetched endpoint records.
    ///
    /// For cross-backend links the source and target live on different backends —
    /// the source runtime cannot resolve the target. The coordinator fetches each
    /// endpoint from its own backend, then calls this method to enforce the
    /// kind-pairing rules without a second DB round-trip.
    ///
    /// `src` and `tgt` are the `resolve_edge_endpoint` results from each backend. The
    /// `token` supplies the pack edge rules installed on this (source) runtime;
    /// no DB access is performed.
    pub fn validate_link_endpoints_by_resolved(
        &self,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
        src: Option<&Resolved>,
        tgt: Option<&Resolved>,
    ) -> RuntimeResult<()> {
        self.validate_link_endpoints_by_resolved_with_rules(
            source_id,
            target_id,
            relation,
            src,
            tgt,
            &self.pack_edge_rules(),
        )
    }

    pub(crate) fn validate_link_endpoints_by_resolved_with_rules(
        &self,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
        src: Option<&Resolved>,
        tgt: Option<&Resolved>,
        pack_rules: &[khive_types::EdgeEndpointRule],
    ) -> RuntimeResult<()> {
        if source_id == target_id {
            return Err(RuntimeError::InvalidInput(
                "self-loop edges are not allowed: source_id and target_id must be different".into(),
            ));
        }

        if relation == EdgeRelation::Annotates {
            match src {
                Some(Resolved::Note(_)) => {}
                Some(_) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "annotates source {source_id} must be a note"
                    )));
                }
                None => {
                    return Err(RuntimeError::NotFound(format!(
                        "link source {source_id} not found"
                    )));
                }
            }
            if tgt.is_none() {
                return Err(RuntimeError::NotFound(format!(
                    "link target {target_id} not found"
                )));
            }
            return Ok(());
        }

        if crate::pack::is_special_relation(relation) {
            let rel_name = relation.as_str();
            let src = src.ok_or_else(|| {
                RuntimeError::NotFound(format!("link source {source_id} not found"))
            })?;
            let tgt = tgt.ok_or_else(|| {
                RuntimeError::NotFound(format!("link target {target_id} not found"))
            })?;
            match (src, tgt) {
                (Resolved::Entity(src_e), Resolved::Entity(tgt_e)) => {
                    if !base_entity_rule_allows(&src_e.kind, relation, &tgt_e.kind) {
                        let legal_relations = accepted_entity_relations_description(
                            pack_rules,
                            &src_e.kind,
                            src_e.entity_type.as_deref(),
                            &tgt_e.kind,
                            tgt_e.entity_type.as_deref(),
                        );
                        let rule_hint = match relation {
                            EdgeRelation::Supports | EdgeRelation::Refutes => {
                                "requires concept|document|dataset|artifact -> concept \
                                 (or same-substrate note -> note)"
                            }
                            _ => "requires same-kind entity endpoints",
                        };
                        return Err(RuntimeError::InvalidInput(format!(
                            "({}) -[{rel_name}]-> ({}) is not in the base endpoint \
                             allowlist; {rel_name} {rule_hint}; currently legal relations for \
                             {} -> {} under the loaded endpoint rules: {legal_relations}",
                            src_e.kind, tgt_e.kind, src_e.kind, tgt_e.kind
                        )));
                    }
                }
                (Resolved::Note(_), Resolved::Note(_)) => {}
                (Resolved::Entity(_), Resolved::Note(_)) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "{rel_name} endpoints must be the same substrate \
                         (note→note or entity→entity); got source={source_id} (entity) \
                         target={target_id} (note)"
                    )));
                }
                (Resolved::Note(_), Resolved::Entity(_)) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "{rel_name} endpoints must be the same substrate \
                         (note→note or entity→entity); got source={source_id} (note) \
                         target={target_id} (entity)"
                    )));
                }
                (Resolved::PackRecord { .. }, _) | (_, Resolved::PackRecord { .. }) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "pack-private record is not a valid edge endpoint for {rel_name}"
                    )));
                }
                _ => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "{rel_name} endpoints must be notes or entities (not events)"
                    )));
                }
            }
            return Ok(());
        }

        // All remaining base relations: entity→entity with kind-level restrictions.
        // Consult pack rules installed on this (source) runtime first.
        if pack_rule_allows(pack_rules, relation, src, tgt) {
            return Ok(());
        }

        let (src_kind, src_entity_type) = match src {
            Some(Resolved::Entity(e)) => (e.kind.as_str(), e.entity_type.as_deref()),
            Some(_) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "link source {source_id} must be an entity for relation {relation:?} \
                     (only `annotates` crosses substrates)"
                )));
            }
            None => {
                return Err(RuntimeError::NotFound(format!(
                    "link source {source_id} not found"
                )));
            }
        };
        let (tgt_kind, tgt_entity_type) = match tgt {
            Some(Resolved::Entity(e)) => (e.kind.as_str(), e.entity_type.as_deref()),
            Some(_) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "link target {target_id} must be an entity for relation {relation:?} \
                     (only `annotates` crosses substrates)"
                )));
            }
            None => {
                return Err(RuntimeError::NotFound(format!(
                    "link target {target_id} not found"
                )));
            }
        };

        if !base_entity_rule_allows(src_kind, relation, tgt_kind) {
            let legal_relations = accepted_entity_relations_description(
                pack_rules,
                src_kind,
                src_entity_type,
                tgt_kind,
                tgt_entity_type,
            );
            return Err(RuntimeError::InvalidInput(format!(
                "({src_kind}) -[{}]-> ({tgt_kind}) is not in the base endpoint \
                 allowlist; use pack EDGE_RULES to extend the allowlist; currently legal relations \
                 for {src_kind} -> {tgt_kind} under the loaded endpoint rules: {legal_relations}",
                relation.as_str()
            )));
        }

        Ok(())
    }

    /// Validate an `annotates` edge relation using pre-located endpoint kinds.
    ///
    /// Sibling of [`Self::validate_link_endpoints_by_resolved`] for callers that
    /// only have an [`EdgeEndpointKind`] (entity/note/event/edge) rather than a
    /// full [`Resolved`] record — the `SubstrateCoordinator`'s cross-backend
    /// `locate_endpoint` resolves edge-substrate UUIDs too (matching `get`'s
    /// by-ID resolution order), but edges have no `Resolved` variant, so
    /// `validate_link_endpoints_by_resolved` cannot express them.
    ///
    /// `annotates` is the only relation this covers: source must be a note,
    /// target may be any substrate (entity, note, event, or edge).
    pub fn validate_annotates_endpoint_kinds(
        &self,
        source_id: Uuid,
        target_id: Uuid,
        source: Option<EdgeEndpointKind>,
        target: Option<EdgeEndpointKind>,
    ) -> RuntimeResult<()> {
        if source_id == target_id {
            return Err(RuntimeError::InvalidInput(
                "self-loop edges are not allowed: source_id and target_id must be different".into(),
            ));
        }
        match source {
            Some(EdgeEndpointKind::Note) => {}
            Some(_) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "annotates source {source_id} must be a note"
                )));
            }
            None => {
                return Err(RuntimeError::NotFound(format!(
                    "link source {source_id} not found"
                )));
            }
        }
        if target.is_none() {
            return Err(RuntimeError::NotFound(format!(
                "link target {target_id} not found"
            )));
        }
        Ok(())
    }

    /// Create a directed edge between two substrates.
    ///
    /// Enforces the three-case relation contract via
    /// `validate_edge_relation_endpoints`. See that method for the full contract.
    ///
    /// For symmetric relations (`competes_with`, `composed_with`) the endpoint
    /// pair is canonicalised to `source_uuid < target_uuid` so that A→B and B→A
    /// deduplicate to one row.
    ///
    /// `metadata` is validated against governed keys; `dependency_kind` is
    /// inferred for `depends_on` edges when absent.
    ///
    /// `target_backend` is always `None` for locally-routed edges written through
    /// this path. Both endpoints must exist in the local namespace, so setting
    /// `target_backend = None` is the only valid choice.
    ///
    /// Endpoint existence is a by-ID check and namespace-agnostic: a record
    /// that exists in a different namespace than the caller still resolves,
    /// exactly as `get()` would.
    pub async fn link(
        &self,
        token: &NamespaceToken,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
        weight: f64,
        metadata: Option<serde_json::Value>,
    ) -> RuntimeResult<Edge> {
        self.link_observed(
            token, source_id, target_id, relation, weight, metadata, false,
        )
        .await
        .map(|result| result.edge)
    }

    /// Observable form of [`Self::link`]. Live natural-key conflicts retain
    /// the accepted replace semantics, while tombstones require the explicit
    /// `resurrect` opt-in. The returned preimage and disposition are derived
    /// inside the graph writer transaction and drive the lifecycle event.
    #[allow(clippy::too_many_arguments)]
    pub async fn link_observed(
        &self,
        token: &NamespaceToken,
        source_id: Uuid,
        target_id: Uuid,
        relation: EdgeRelation,
        weight: f64,
        metadata: Option<serde_json::Value>,
        resurrect: bool,
    ) -> RuntimeResult<EdgeUpsertResult> {
        validate_edge_weight(weight)?;
        validate_edge_metadata(relation, metadata.as_ref())?;
        let (source_kind, target_kind) = self
            .validate_edge_relation_endpoints(token, source_id, target_id, relation)
            .await?;
        let (canonical_source, canonical_target) =
            canonical_edge_endpoints(relation, source_id, target_id);
        let (source_kind, target_kind) =
            canonical_edge_endpoint_kinds(source_id, canonical_source, source_kind, target_kind);
        let (source_id, target_id) = (canonical_source, canonical_target);
        let metadata = if relation == EdgeRelation::DependsOn {
            // By-ID, unfiltered — matches the namespace-agnostic endpoint validation
            // above. The visible-set-scoped `resolve` would silently drop the
            // dependency_kind inference for endpoints validation now allows outside
            // the caller's visible set.
            match (
                self.resolve_edge_endpoint(token, source_id).await?,
                self.resolve_edge_endpoint(token, target_id).await?,
            ) {
                (Some(Resolved::Entity(src_e)), Some(Resolved::Entity(tgt_e))) => {
                    merge_dependency_kind(&src_e.kind, &tgt_e.kind, metadata)
                }
                _ => metadata,
            }
        } else {
            metadata
        };
        validate_edge_metadata(relation, metadata.as_ref())?;
        let now = chrono::Utc::now();
        let ns = token.namespace().as_str();
        let edge = Edge {
            id: LinkId::from(Uuid::new_v4()),
            namespace: ns.to_string(),
            source_id,
            target_id,
            relation,
            weight,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            metadata,
            target_backend: None,
        };
        // `upsert_edge_guarded` re-checks both endpoints exist as part of the same
        // write, not the separate `validate_edge_relation_endpoints` read above: a
        // concurrent hard-delete landing between that read and this write can no
        // longer create a durably dangling edge. Which endpoint(s) were missing is
        // reported by the guard's own in-transaction probe (`GuardedWriteOutcome::
        // Refused`), not reconstructed here by re-reading the endpoints after the
        // fact: a second concurrent write landing between the refusal and a
        // post-hoc read could otherwise misreport which endpoint was actually
        // missing at write time.
        let attribution = crate::EventAttribution::from_token(token);
        let outcome = compose_graph_mutation_events(
            self.backend(),
            GraphMutationRequest::Single {
                request: EdgeUpsertRequest { edge, resurrect },
                guard_endpoints: true,
            },
            GraphMutationPreconditions::default(),
            Vec::new(),
            move |outcome| match &outcome.mutation {
                GraphMutationOutcome::Single(GuardedEdgeUpsertOutcome::Written(result)) => {
                    Ok(vec![Self::link_mutation_event(
                        &attribution,
                        result,
                        source_kind,
                        target_kind,
                    )])
                }
                _ => Err(Self::link_composition_shape_error(
                    "expected a written singleton",
                )),
            },
        )
        .await?;
        let GraphMutationOutcome::Single(outcome) = outcome.mutation else {
            return Err(RuntimeError::Internal(
                "link: unexpected composition outcome".into(),
            ));
        };
        let result = match outcome {
            GuardedEdgeUpsertOutcome::Written(result) => result,
            GuardedEdgeUpsertOutcome::Refused(EdgeUpsertRefusal::MissingEndpoints(missing)) => {
                return Err(RuntimeError::GuardedWriteFailed(GuardedWriteFailure {
                    entry_index: None,
                    missing_source: missing.source.then_some(source_id),
                    missing_target: missing.target.then_some(target_id),
                }));
            }
            GuardedEdgeUpsertOutcome::Refused(EdgeUpsertRefusal::ResurrectionRequired { edge }) => {
                return Err(RuntimeError::InvalidInput(format!(
                    "edge {} is soft-deleted; pass resurrect=true to link explicitly",
                    edge.id
                )))
            }
        };
        Ok(result)
    }

    /// Write an edge with an explicit `target_backend` stamp (ADR-029 D3).
    ///
    /// Called by the `SubstrateCoordinator` when source and target are on
    /// different backends. The coordinator validates endpoints before calling
    /// this method via [`Self::validate_link_endpoints`], and supplies the
    /// resolved endpoint kinds for the lifecycle event. Endpoint validation is
    /// skipped here. The edge is written on the source backend only.
    #[allow(clippy::too_many_arguments)]
    pub async fn link_with_target_backend(
        &self,
        token: &NamespaceToken,
        source_id: Uuid,
        target_id: Uuid,
        source_kind: EdgeEndpointKind,
        target_kind: EdgeEndpointKind,
        relation: EdgeRelation,
        weight: f64,
        metadata: Option<serde_json::Value>,
        target_backend: Option<String>,
    ) -> RuntimeResult<Edge> {
        self.link_with_target_backend_observed(
            token,
            source_id,
            target_id,
            source_kind,
            target_kind,
            relation,
            weight,
            metadata,
            target_backend,
            false,
        )
        .await
        .map(|result| result.edge)
    }

    /// Policy-aware cross-backend form of [`Self::link_observed`]. Endpoint
    /// validation remains the coordinator's responsibility; mutation
    /// classification and tombstone handling stay inside the source store.
    #[allow(clippy::too_many_arguments)]
    pub async fn link_with_target_backend_observed(
        &self,
        token: &NamespaceToken,
        source_id: Uuid,
        target_id: Uuid,
        source_kind: EdgeEndpointKind,
        target_kind: EdgeEndpointKind,
        relation: EdgeRelation,
        weight: f64,
        metadata: Option<serde_json::Value>,
        target_backend: Option<String>,
        resurrect: bool,
    ) -> RuntimeResult<EdgeUpsertResult> {
        validate_edge_weight(weight)?;
        let (canonical_source, canonical_target) =
            canonical_edge_endpoints(relation, source_id, target_id);
        let (source_kind, target_kind) =
            canonical_edge_endpoint_kinds(source_id, canonical_source, source_kind, target_kind);
        let (source_id, target_id) = (canonical_source, canonical_target);
        validate_edge_metadata(relation, metadata.as_ref())?;
        let now = chrono::Utc::now();
        let ns = token.namespace().as_str();
        let edge = Edge {
            id: LinkId::from(Uuid::new_v4()),
            namespace: ns.to_string(),
            source_id,
            target_id,
            relation,
            weight,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            metadata,
            target_backend,
        };
        let attribution = crate::EventAttribution::from_token(token);
        let outcome = compose_graph_mutation_events(
            self.backend(),
            GraphMutationRequest::Single {
                request: EdgeUpsertRequest { edge, resurrect },
                guard_endpoints: false,
            },
            GraphMutationPreconditions::default(),
            Vec::new(),
            move |outcome| match &outcome.mutation {
                GraphMutationOutcome::Single(GuardedEdgeUpsertOutcome::Written(result)) => {
                    Ok(vec![Self::link_mutation_event(
                        &attribution,
                        result,
                        source_kind,
                        target_kind,
                    )])
                }
                _ => Err(Self::link_composition_shape_error(
                    "expected a written singleton",
                )),
            },
        )
        .await?;
        match outcome.mutation {
            GraphMutationOutcome::Single(GuardedEdgeUpsertOutcome::Written(result)) => Ok(result),
            GraphMutationOutcome::Single(GuardedEdgeUpsertOutcome::Refused(
                EdgeUpsertRefusal::ResurrectionRequired { edge },
            )) => {
                let error = khive_storage::StorageError::Conflict {
                    capability: khive_storage::StorageCapability::Graph,
                    operation: "upsert_edge_observed".into(),
                    message: format!(
                        "edge {} is soft-deleted; explicit resurrection is required",
                        edge.id,
                    ),
                };
                Err(RuntimeError::InvalidInput(format!(
                    "edge natural key is soft-deleted; pass resurrect=true to link explicitly: {error}"
                )))
            }
            _ => Err(RuntimeError::Internal(
                "link: unexpected composition outcome".into(),
            )),
        }
    }

    fn link_mutation_event(
        attribution: &crate::EventAttribution,
        result: &EdgeUpsertResult,
        source_kind: EdgeEndpointKind,
        target_kind: EdgeEndpointKind,
    ) -> Event {
        let kind = match result.disposition {
            EdgeUpsertDisposition::Created => EventKind::LinkCreated,
            EdgeUpsertDisposition::Updated | EdgeUpsertDisposition::Resurrected => {
                EventKind::EdgeUpdated
            }
        };
        let edge_id = Uuid::from(result.edge.id);
        let mut payload = serde_json::json!({
            "id": edge_id,
            "namespace": result.edge.namespace,
            "mutation": result.disposition.name(),
            "source_id": result.edge.source_id,
            "target_id": result.edge.target_id,
            "relation": result.edge.relation,
            "weight": result.edge.weight,
            "metadata": result.edge.metadata,
            "previous": result.previous,
        });
        if kind == EventKind::LinkCreated {
            payload["source_kind"] = serde_json::json!(source_kind.name());
            payload["target_kind"] = serde_json::json!(target_kind.name());
        }
        attribution.stamp(
            Event::new(
                result.edge.namespace.clone(),
                "link",
                kind,
                SubstrateKind::Entity,
                "",
            )
            .with_target(edge_id)
            .with_payload(payload),
        )
    }

    fn link_composition_shape_error(message: &'static str) -> khive_storage::StorageError {
        khive_storage::StorageError::InvalidInput {
            capability: khive_storage::StorageCapability::Graph,
            operation: "link_mutation_event".into(),
            message: message.into(),
        }
    }

    /// Validate and construct an edge from a [`LinkSpec`] without writing to storage.
    ///
    /// Applies the full edge contract (endpoint validation, symmetric
    /// canonicalization, `dependency_kind` inference and metadata validation).
    /// Returns the constructed `Edge` on success; the caller is responsible for
    /// persisting it (e.g. via `upsert_edge` or `link_many`).
    ///
    /// The `token` must be a pre-authorized namespace token from the dispatch
    /// layer. If `spec.namespace` is set it must match `token.namespace()`;
    /// a mismatch returns `RuntimeError::InvalidInput`.
    pub async fn build_edge(&self, token: &NamespaceToken, spec: &LinkSpec) -> RuntimeResult<Edge> {
        self.build_edge_with_endpoint_kinds(token, spec)
            .await
            .map(|(edge, _)| edge)
    }

    async fn build_edge_with_endpoint_kinds(
        &self,
        token: &NamespaceToken,
        spec: &LinkSpec,
    ) -> RuntimeResult<(Edge, (EdgeEndpointKind, EdgeEndpointKind))> {
        validate_edge_metadata(spec.relation, spec.metadata.as_ref())?;
        let ns_str = match &spec.namespace {
            Some(s) => {
                let spec_ns = crate::Namespace::parse(s)
                    .map_err(|e| RuntimeError::InvalidInput(format!("invalid namespace: {e}")))?;
                if &spec_ns != token.namespace() {
                    return Err(RuntimeError::InvalidInput(
                        "LinkSpec namespace does not match token namespace".into(),
                    ));
                }
                s.as_str()
            }
            None => token.namespace().as_str(),
        };
        let endpoint_kinds = self
            .validate_edge_relation_endpoints(token, spec.source_id, spec.target_id, spec.relation)
            .await?;
        let (source_id, target_id) =
            canonical_edge_endpoints(spec.relation, spec.source_id, spec.target_id);
        let endpoint_kinds = canonical_edge_endpoint_kinds(
            spec.source_id,
            source_id,
            endpoint_kinds.0,
            endpoint_kinds.1,
        );
        let metadata = if spec.relation == EdgeRelation::DependsOn {
            // By-ID, unfiltered — matches the namespace-agnostic endpoint validation
            // above. The visible-set-scoped `resolve` would silently drop the
            // dependency_kind inference for endpoints validation now allows outside
            // the caller's visible set.
            match (
                self.resolve_edge_endpoint(token, source_id).await?,
                self.resolve_edge_endpoint(token, target_id).await?,
            ) {
                (Some(Resolved::Entity(src_e)), Some(Resolved::Entity(tgt_e))) => {
                    merge_dependency_kind(&src_e.kind, &tgt_e.kind, spec.metadata.clone())
                }
                _ => spec.metadata.clone(),
            }
        } else {
            spec.metadata.clone()
        };
        validate_edge_metadata(spec.relation, metadata.as_ref())?;
        let now = chrono::Utc::now();
        Ok((
            Edge {
                id: LinkId::from(Uuid::new_v4()),
                namespace: ns_str.to_string(),
                source_id,
                target_id,
                relation: spec.relation,
                weight: spec.weight,
                created_at: now,
                updated_at: now,
                deleted_at: None,
                metadata,
                target_backend: None,
            },
            endpoint_kinds,
        ))
    }

    /// Validate and atomically upsert a batch of edges.
    ///
    /// All edges are validated and constructed with `build_edge` before any
    /// write. If validation fails for any entry the entire batch is rejected
    /// (no writes occur). On success, all edges are persisted in a single
    /// source transaction with every lifecycle event and observation projection.
    ///
    /// After the bulk upsert, each edge is read back by its natural key
    /// (namespace, source_id, target_id, relation) so that the returned IDs
    /// are always the persisted row IDs, not the locally-generated UUIDs that
    /// may have been displaced by an ON CONFLICT DO UPDATE. This mirrors the
    /// same read-back applied to singleton `link()` and prevents phantom-ID
    /// exposure when callers upsert overlapping triples with `verbose=true`.
    ///
    /// All specs must share the same namespace; the namespace is taken from
    /// `token` (or validated against it if `spec.namespace` is set).
    pub async fn link_many(
        &self,
        token: &NamespaceToken,
        specs: Vec<LinkSpec>,
    ) -> RuntimeResult<Vec<Edge>> {
        self.link_many_observed(token, specs)
            .await
            .map(|rows| rows.into_iter().map(|row| row.edge).collect())
    }

    /// Observed all-or-nothing bulk link upsert. Every row carries its own
    /// create/update/resurrection disposition, and every tombstone policy is
    /// preflighted inside the same writer transaction before any mutation.
    pub async fn link_many_observed(
        &self,
        token: &NamespaceToken,
        specs: Vec<LinkSpec>,
    ) -> RuntimeResult<Vec<EdgeUpsertResult>> {
        self.link_many_guarded_observed(
            token,
            specs,
            GraphMutationPreconditions::default(),
            Vec::new(),
        )
        .await
        .map(|(rows, _)| rows)
    }

    /// Internal seam for khive-runtime; no compatibility promise.
    ///
    /// Guarded composition seam for packs.
    #[doc(hidden)]
    pub async fn link_many_guarded_observed(
        &self,
        token: &NamespaceToken,
        specs: Vec<LinkSpec>,
        preconditions: GraphMutationPreconditions,
        retirements: Vec<Edge>,
    ) -> RuntimeResult<(Vec<EdgeUpsertResult>, Vec<LinkId>)> {
        let namespace = token.namespace().as_str();
        let foreign_document = preconditions
            .document
            .as_ref()
            .is_some_and(|guard| guard.namespace != namespace);
        let foreign_edge = preconditions.edges.iter().any(|guard| {
            guard.namespace != namespace
                || guard
                    .expected
                    .as_ref()
                    .is_some_and(|edge| edge.namespace != namespace)
        });
        if foreign_document
            || foreign_edge
            || retirements.iter().any(|edge| edge.namespace != namespace)
        {
            return Err(RuntimeError::InvalidInput(
                "guarded link namespace does not match token namespace".into(),
            ));
        }
        if specs.is_empty()
            && preconditions.document.is_none()
            && preconditions.edges.is_empty()
            && preconditions.statement_guards.is_empty()
            && retirements.is_empty()
        {
            return Ok((Vec::new(), Vec::new()));
        }
        let mut edges = Vec::with_capacity(specs.len());
        let mut endpoint_kinds = Vec::with_capacity(specs.len());
        for spec in &specs {
            let (edge, kinds) = self.build_edge_with_endpoint_kinds(token, spec).await?;
            edges.push(edge);
            endpoint_kinds.push(kinds);
        }
        // `upsert_edges_guarded` re-checks every edge's endpoints as part of the
        // same write, not the separate per-spec `build_edge` validation reads
        // above. A concurrent hard-delete of any endpoint landing between those
        // reads and this write aborts the whole batch (all-or-nothing, no
        // partial write) instead of persisting a dangling edge. The failing
        // entry's index and its missing endpoint(s) come from the guard's own
        // in-transaction pre-check (`GuardedBatchOutcome::refused`), not a
        // post-hoc re-read of the batch after the write already failed.
        let requests = edges
            .into_iter()
            .zip(specs.iter())
            .map(|(edge, spec)| EdgeUpsertRequest {
                edge,
                resurrect: spec.resurrect,
            })
            .collect();
        let attribution = crate::EventAttribution::from_token(token);
        let outcome = compose_graph_mutation_events(
            self.backend(),
            GraphMutationRequest::Batch {
                requests,
                guard_endpoints: true,
            },
            preconditions,
            retirements,
            move |outcome| {
                let GraphMutationOutcome::Batch(batch) = &outcome.mutation else {
                    return Err(Self::link_composition_shape_error(
                        "expected a written batch",
                    ));
                };
                if batch.rows.len() != endpoint_kinds.len() {
                    return Err(Self::link_composition_shape_error(
                        "edge result count differs from validated endpoint count",
                    ));
                }
                let mut events = Vec::with_capacity(batch.rows.len() + outcome.retired.len());
                for (row, (source_kind, target_kind)) in batch.rows.iter().zip(endpoint_kinds) {
                    events.push(Self::link_mutation_event(
                        &attribution,
                        row,
                        source_kind,
                        target_kind,
                    ));
                }
                for edge in &outcome.retired {
                    let edge_id = Uuid::from(edge.id);
                    events.push(
                        attribution.stamp(
                            Event::new(
                                edge.namespace.clone(),
                                "delete",
                                EventKind::EdgeDeleted,
                                SubstrateKind::Entity,
                                "",
                            )
                            .with_target(edge_id)
                            .with_payload(serde_json::json!({
                                "id": edge_id, "namespace": edge.namespace, "hard": false,
                            })),
                        ),
                    );
                }
                Ok(events)
            },
        )
        .await?;
        let retired = outcome.retired.into_iter().map(|edge| edge.id).collect();
        let outcome = match outcome.mutation {
            GraphMutationOutcome::Batch(outcome) => outcome,
            GraphMutationOutcome::GuardFailed(failure) => {
                return Err(khive_storage::StorageError::Conflict {
                    capability: khive_storage::StorageCapability::Graph,
                    operation: "link_many_guarded_observed".into(),
                    message: failure.to_string(),
                }
                .into());
            }
            _ => {
                return Err(RuntimeError::Internal(
                    "link_many: unexpected composition outcome".into(),
                ));
            }
        };
        if let Some(refusal) = outcome.refusal {
            return match refusal.reason {
                EdgeUpsertRefusal::MissingEndpoints(missing) => {
                    Err(RuntimeError::GuardedWriteFailed(guarded_link_batch_failure(
                        &specs[refusal.entry_index],
                        refusal.entry_index,
                        missing,
                    )))
                }
                EdgeUpsertRefusal::ResurrectionRequired { edge } => {
                    Err(RuntimeError::InvalidInput(format!(
                        "batch entry {} targets soft-deleted edge {}; pass resurrect=true for that link",
                        refusal.entry_index, edge.id
                    )))
                }
            };
        }
        Ok((outcome.rows, retired))
    }

    /// Create an edge only when its natural key is absent and all supplied guards pass.
    ///
    /// Endpoint and namespace validation precede the write. Guards run in order in the
    /// same writer transaction as insertion; only a created edge emits LinkCreated.
    /// Existing live edges and tombstones are never changed. Registry link hooks
    /// remain the dispatch layer's responsibility, as with [`Self::build_edge`].
    pub async fn link_if_absent_with_guards(
        &self,
        token: &NamespaceToken,
        spec: LinkSpec,
        guards: Vec<GraphStatementGuard>,
    ) -> RuntimeResult<EdgeInsertIfAbsentOutcome> {
        if spec.resurrect {
            return Err(RuntimeError::InvalidInput(
                "create-only link does not accept resurrect=true".into(),
            ));
        }
        validate_edge_weight(spec.weight)?;
        let (edge, (source_kind, target_kind)) =
            self.build_edge_with_endpoint_kinds(token, &spec).await?;
        let attribution = crate::EventAttribution::from_token(token);
        let outcome = compose_graph_mutation_events(
            self.backend(),
            GraphMutationRequest::InsertIfAbsent { edge },
            GraphMutationPreconditions {
                statement_guards: guards,
                ..Default::default()
            },
            Vec::new(),
            move |outcome| match &outcome.mutation {
                GraphMutationOutcome::InsertIfAbsent(EdgeInsertIfAbsentOutcome::Created(edge)) => {
                    Ok(vec![Self::link_mutation_event(
                        &attribution,
                        &EdgeUpsertResult {
                            edge: edge.clone(),
                            disposition: EdgeUpsertDisposition::Created,
                            previous: None,
                        },
                        source_kind,
                        target_kind,
                    )])
                }
                _ => Err(Self::link_composition_shape_error(
                    "expected a created edge",
                )),
            },
        )
        .await?;
        match outcome.mutation {
            GraphMutationOutcome::InsertIfAbsent(outcome) => Ok(outcome),
            GraphMutationOutcome::GuardFailed(failure) => {
                Ok(EdgeInsertIfAbsentOutcome::GuardFailed(failure))
            }
            _ => Err(RuntimeError::Internal(
                "create-only link: unexpected composition outcome".into(),
            )),
        }
    }
}

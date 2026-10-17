use super::{
    base_entity_rule_allows, endpoint_matches, resolved_pair, EdgeEndpointKind, EdgeEndpointRule,
    EdgeRelation, EndpointKind, Resolved, RuntimeError, RuntimeResult, Uuid,
    BASE_ENTITY_ENDPOINT_RULES,
};

/// `true` if `spec` matches the given substrate + kind + entity_type triple,
/// treating an *absent* `entity_type` on the query side as unconstrained
/// rather than an exact match against "no subtype".
///
/// Used only by the static GQL impossibility hint (`static_impossible_edge_pattern_warnings`,
/// `accepted_entity_kind_pairs_for_relation`), which reasons over a *pattern*
/// endpoint, not a resolved entity. A pattern endpoint that names a kind but
/// no `entity_type` (`(a:concept)-[:depends_on]->(b:concept)`) has not ruled
/// out any subtype, so an `EntityOfType` rule for that kind still makes the
/// triple possible — unlike `endpoint_matches`, which the live link
/// validator applies to *resolved* entities, where a `None` `entity_type`
/// means the entity genuinely has no subtype and must be an exact miss
/// against a typed rule. Do not use this for validation.
fn pattern_endpoint_matches(
    spec: &EndpointKind,
    substrate: &str,
    kind: &str,
    entity_type: Option<&str>,
) -> bool {
    match spec {
        EndpointKind::EntityOfType {
            kind: k,
            entity_type: t,
        } => substrate == "entity" && *k == kind && entity_type.is_none_or(|et| et == *t),
        _ => endpoint_matches(spec, substrate, kind, entity_type),
    }
}

fn collect_pack_relations(
    rules: &[EdgeEndpointRule],
    accepts: impl Fn(&EdgeEndpointRule) -> bool,
) -> Vec<EdgeRelation> {
    let mut relations: Vec<EdgeRelation> = rules
        .iter()
        .filter(|rule| accepts(rule))
        .map(|rule| rule.relation)
        .collect();
    relations.sort_by_key(|relation| relation.as_str());
    relations.dedup();
    relations
}

/// Relations that a composed pack `EDGE_RULES` set accepts for a given
/// `(entity_kind, entity_type)` endpoint pair, using the EXACT SAME
/// `endpoint_matches` semantics `pack_rule_allows` applies internally
/// (`EntityOfKind`, `EntityOfType`, `NoteOfKind`) — never a re-filtered copy.
///
/// Both endpoints are treated as entities (substrate `"entity"`), matching
/// the only case pack-layer error-hint code needs (issue #543): a rejected
/// `link` between two already-resolved entities. `entity_type` is the
/// pack-owned granular subtype (e.g. `"theorem"`); pass `None` for
/// untyped entities. Exposed so `khive-pack-kg`'s hint derivation cannot
/// silently diverge from the validator by only matching `EntityOfKind` and
/// missing pack rules declared via `EntityOfType` (e.g. `khive-pack-formal`'s
/// typed `theorem -> definition` `depends_on` rules).
pub fn accepted_pack_relations_for_entities(
    rules: &[EdgeEndpointRule],
    src_kind: &str,
    src_entity_type: Option<&str>,
    tgt_kind: &str,
    tgt_entity_type: Option<&str>,
) -> Vec<EdgeRelation> {
    collect_pack_relations(rules, |r| {
        endpoint_matches(&r.source, "entity", src_kind, src_entity_type)
            && endpoint_matches(&r.target, "entity", tgt_kind, tgt_entity_type)
    })
}

/// Relations accepted for one resolved entity endpoint pair under the full
/// live contract: the base allowlist plus the loaded packs' additive rules.
///
/// This is the pair-oriented counterpart to the private
/// `accepted_entity_kind_pairs_for_relation` helper. It is shared by validation
/// errors and pack-layer hints so every write path can tell a caller which
/// relations would be legal without maintaining a second endpoint table.
/// Pack declarations for relations with dedicated substrate branches are
/// excluded because the live validator resolves `annotates` and the three
/// same-substrate special relations before pack rules are consulted.
pub fn accepted_entity_relations_for_entities(
    rules: &[EdgeEndpointRule],
    src_kind: &str,
    src_entity_type: Option<&str>,
    tgt_kind: &str,
    tgt_entity_type: Option<&str>,
) -> Vec<EdgeRelation> {
    let mut relations: Vec<EdgeRelation> = BASE_ENTITY_ENDPOINT_RULES
        .iter()
        .filter(|(src, _relation, tgt)| (*src == "*" || *src == src_kind) && *tgt == tgt_kind)
        .map(|(_src, relation, _tgt)| *relation)
        .collect();
    relations.extend(
        accepted_pack_relations_for_entities(
            rules,
            src_kind,
            src_entity_type,
            tgt_kind,
            tgt_entity_type,
        )
        .into_iter()
        .filter(|relation| {
            *relation != EdgeRelation::Annotates && !crate::pack::is_special_relation(*relation)
        }),
    );
    relations.sort_by_key(|relation| relation.as_str());
    relations.dedup();
    relations
}

pub(super) fn accepted_entity_relations_description(
    rules: &[EdgeEndpointRule],
    src_kind: &str,
    src_entity_type: Option<&str>,
    tgt_kind: &str,
    tgt_entity_type: Option<&str>,
) -> String {
    let relations = accepted_entity_relations_for_entities(
        rules,
        src_kind,
        src_entity_type,
        tgt_kind,
        tgt_entity_type,
    );
    if relations.is_empty() {
        "none".to_string()
    } else {
        relations
            .iter()
            .map(EdgeRelation::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Hint-only counterpart to [`accepted_pack_relations_for_entities`] that
/// matches via [`pattern_endpoint_matches`] instead of [`endpoint_matches`],
/// so an absent `entity_type` is treated as unconstrained rather than an
/// exact-match miss against `EntityOfType` rules. Used exclusively by the
/// static GQL impossibility hint — never by validation.
fn accepted_pack_relations_for_pattern_entities(
    rules: &[EdgeEndpointRule],
    src_kind: &str,
    src_entity_type: Option<&str>,
    tgt_kind: &str,
    tgt_entity_type: Option<&str>,
) -> Vec<EdgeRelation> {
    collect_pack_relations(rules, |r| {
        pattern_endpoint_matches(&r.source, "entity", src_kind, src_entity_type)
            && pattern_endpoint_matches(&r.target, "entity", tgt_kind, tgt_entity_type)
    })
}

/// All `(source_kind, target_kind)` entity-kind pairs — restricted to the closed
/// 8-kind base [`khive_types::EntityKind`] taxonomy — that accept `relation`
/// under the composed base allowlist plus pack `EDGE_RULES`. Reuses
/// [`base_entity_rule_allows`] and [`accepted_pack_relations_for_pattern_entities`]
/// over the closed kind set rather than re-deriving a parallel table (for GQL
/// query-pattern hint derivation). Pack rules are skipped when
/// `crate::pack::is_special_relation` is true (supersedes/supports/refutes):
/// those relations are resolved by the live validator's special-relation branch
/// before `pack_rule_allows` is ever reached — see `pack.rs`'s
/// `edge_endpoint_table` doc comment.
fn accepted_entity_kind_pairs_for_relation(
    pack_rules: &[EdgeEndpointRule],
    relation: EdgeRelation,
) -> Vec<(&'static str, &'static str)> {
    let mut pairs = Vec::new();
    for src in khive_types::EntityKind::ALL {
        for tgt in khive_types::EntityKind::ALL {
            let allowed = base_entity_rule_allows(src.name(), relation, tgt.name())
                || (!crate::pack::is_special_relation(relation)
                    && accepted_pack_relations_for_pattern_entities(
                        pack_rules,
                        src.name(),
                        None,
                        tgt.name(),
                        None,
                    )
                    .contains(&relation));
            if allowed {
                pairs.push((src.name(), tgt.name()));
            }
        }
    }
    pairs
}

/// Scans a GQL `MATCH` pattern for edges that name an explicit relation and
/// explicit entity kinds on both endpoints — a single mandatory hop, a fixed
/// direction — where the `(source_kind, relation, target_kind)` triple can
/// never match under the composed edge endpoint contract. Returns one warning
/// per statically-impossible edge.
///
/// Deliberately conservative: unlabeled nodes, unlabeled/multi-relation edges,
/// undirected edges, variable-length hops, and note-kind endpoints are left
/// unchecked, since none of those name a single static triple to test against
/// the validator (issue #593).
///
/// Mirrors the validator's special-relation precedence for `supersedes` /
/// `supports` / `refutes` (see [`accepted_entity_kind_pairs_for_relation`]):
/// pack rules never make those triples possible, only the base allowlist does.
pub(super) fn static_impossible_edge_pattern_warnings(
    language: khive_query::QueryLanguage,
    pattern: &khive_query::ast::MatchPattern,
    pack_rules: &[EdgeEndpointRule],
) -> Vec<String> {
    use khive_query::ast::{EdgeDirection, PatternElement};

    if language != khive_query::QueryLanguage::Gql {
        return Vec::new();
    }

    let elements = &pattern.elements;
    let mut warnings = Vec::new();

    for (i, el) in elements.iter().enumerate() {
        let PatternElement::Edge(edge) = el else {
            continue;
        };
        if edge.relations.len() != 1 || edge.min_hops != 1 || edge.max_hops != 1 {
            continue;
        }
        let (left, right) = match (elements.get(i.wrapping_sub(1)), elements.get(i + 1)) {
            (Some(PatternElement::Node(l)), Some(PatternElement::Node(r))) => (l, r),
            _ => continue,
        };
        let (src_node, tgt_node) = match edge.direction {
            EdgeDirection::Out => (left, right),
            EdgeDirection::In => (right, left),
            EdgeDirection::Both => continue,
        };
        let (Some(src_raw), Some(tgt_raw)) = (src_node.kind.as_deref(), tgt_node.kind.as_deref())
        else {
            continue;
        };
        let (Ok(src_kind), Ok(tgt_kind)) = (
            src_raw.parse::<khive_types::EntityKind>(),
            tgt_raw.parse::<khive_types::EntityKind>(),
        ) else {
            continue;
        };
        let Ok(relation) = edge.relations[0].parse::<EdgeRelation>() else {
            continue;
        };

        let possible = base_entity_rule_allows(src_kind.name(), relation, tgt_kind.name())
            || (!crate::pack::is_special_relation(relation)
                && accepted_pack_relations_for_pattern_entities(
                    pack_rules,
                    src_kind.name(),
                    src_node.entity_type.as_deref(),
                    tgt_kind.name(),
                    tgt_node.entity_type.as_deref(),
                )
                .contains(&relation));
        if possible {
            continue;
        }

        let accepted = accepted_entity_kind_pairs_for_relation(pack_rules, relation);
        let accepted_str = if accepted.is_empty() {
            "none".to_string()
        } else {
            accepted
                .iter()
                .map(|(s, t)| format!("{s}->{t}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        warnings.push(format!(
            "pattern ({src})-[:{relation}]->({tgt}) can never match: '{relation}' does not accept \
             {src}->{tgt} endpoints; accepted source->target kinds for '{relation}': {accepted_str}",
            src = src_kind.name(),
            tgt = tgt_kind.name(),
        ));
    }

    warnings
}

/// `true` if any pack-declared edge endpoint rule allows the
/// `(source, relation, target)` triple. Pack rules are additive only.
pub(crate) fn pack_rule_allows(
    rules: &[EdgeEndpointRule],
    relation: EdgeRelation,
    src: Option<&Resolved>,
    tgt: Option<&Resolved>,
) -> bool {
    let Some((src_sub, src_kind, src_type)) = resolved_pair(src) else {
        return false;
    };
    let Some((tgt_sub, tgt_kind, tgt_type)) = resolved_pair(tgt) else {
        return false;
    };
    rules.iter().any(|r| {
        r.relation == relation
            && endpoint_matches(&r.source, src_sub, src_kind, src_type)
            && endpoint_matches(&r.target, tgt_sub, tgt_kind, tgt_type)
    })
}

/// Canonical endpoint order for symmetric relations (F012).
///
/// For `competes_with` and `composed_with`, normalises direction so that
/// `source_uuid < target_uuid` (lexicographic on the UUID bytes). This
/// collapses A→B and B→A into a single canonical row, preventing duplicates.
pub(crate) fn canonical_edge_endpoints(
    relation: EdgeRelation,
    source_id: Uuid,
    target_id: Uuid,
) -> (Uuid, Uuid) {
    relation.canonical_endpoints(source_id, target_id)
}

/// Keep endpoint substrates paired with their IDs when a symmetric link swaps direction.
pub(crate) fn canonical_edge_endpoint_kinds(
    requested_source_id: Uuid,
    canonical_source_id: Uuid,
    source_kind: EdgeEndpointKind,
    target_kind: EdgeEndpointKind,
) -> (EdgeEndpointKind, EdgeEndpointKind) {
    if requested_source_id == canonical_source_id {
        (source_kind, target_kind)
    } else {
        (target_kind, source_kind)
    }
}

/// Infer the default `dependency_kind` from endpoint entity kinds.
///
/// `pub(crate)` so `crate::atomic_prepare::prepare_link` can reuse this exact
/// inference table, keeping `--atomic link` byte-for-byte consistent with the
/// non-atomic `link()` rather than re-deriving the table.
pub(crate) fn infer_dependency_kind(src_kind: &str, tgt_kind: &str) -> Option<&'static str> {
    match (src_kind, tgt_kind) {
        ("project", "project") => Some("build"),
        ("service", "service") => Some("runtime"),
        ("service", "dataset") => Some("data"),
        ("service", "artifact") => Some("artifact"),
        ("artifact", "project") | ("artifact", "service") => Some("tooling"),
        ("document", "document") => Some("normative"),
        _ => None,
    }
}

/// Merge an inferred `dependency_kind` into `depends_on` edge metadata.
///
/// If `metadata` already carries a `dependency_kind` key the existing value is
/// preserved. If the key is absent and the endpoint pair has a known default,
/// the inferred value is added. Returns `metadata` unchanged for all other
/// cases (no matching default, or metadata already has the key).
///
/// `pub(crate)` so `crate::atomic_prepare::prepare_link` can reuse it for
/// atomic/non-atomic parity.
pub(crate) fn merge_dependency_kind(
    src_kind: &str,
    tgt_kind: &str,
    metadata: Option<serde_json::Value>,
) -> Option<serde_json::Value> {
    // JSON null has the same meaning as an omitted metadata argument. All
    // callers validate the object shape before reaching inference.
    let metadata = metadata.filter(|value| !value.is_null());
    if let Some(ref m) = metadata {
        if m.get("dependency_kind").is_some() {
            return metadata;
        }
    }
    let Some(inferred) = infer_dependency_kind(src_kind, tgt_kind) else {
        return metadata;
    };
    let mut obj = metadata.unwrap_or_else(|| serde_json::json!({}));
    if let Some(o) = obj.as_object_mut() {
        o.insert("dependency_kind".to_string(), serde_json::json!(inferred));
    }
    Some(obj)
}

/// Explain a dependency qualifier added by inference without changing metadata.
///
/// Call after metadata validation, with the caller's metadata after merging an
/// explicit top-level qualifier and the effective metadata after inference.
/// A present caller key always suppresses the hint. The effective value comes
/// from the existing inference path rather than a second endpoint-kind table.
pub fn dependency_kind_inference_hint(
    relation: EdgeRelation,
    supplied_metadata: Option<&serde_json::Value>,
    effective_metadata: Option<&serde_json::Value>,
) -> Option<String> {
    if relation != EdgeRelation::DependsOn
        || supplied_metadata.is_some_and(|metadata| metadata.get("dependency_kind").is_some())
    {
        return None;
    }
    let kind = effective_metadata?
        .get("dependency_kind")?
        .as_str()
        .filter(|kind| VALID_DEPENDENCY_KINDS.contains(kind))?;
    let mut hint = format!("dependency_kind was inferred as {kind:?} from endpoint kinds.");
    if kind == "normative" {
        hint.push_str(" For references that are not normative, use links_to.");
    }
    Some(hint)
}

/// Merge a caller-supplied top-level `dependency_kind` param into an edge's
/// `metadata` object, filling the key only if `metadata` doesn't already
/// carry one. This is distinct from `merge_dependency_kind` above (which
/// infers a default from endpoint entity kinds when no explicit value was
/// given at all) — this one folds in an EXPLICIT `dependency_kind` argument
/// the caller passed alongside `metadata`.
///
/// `pub`: the single source both `khive-pack-kg::handlers::link::handle_link`
/// (via `khive_runtime::merge_entry_metadata`) and
/// `crate::atomic_prepare::prepare_link` call. Lives in `khive-runtime` (not
/// pack-kg) because packs depend on `khive-runtime`, never the reverse: the
/// only direction that lets both call sites share one copy instead of a
/// hand-duplicated block.
pub fn merge_entry_metadata(
    metadata: Option<serde_json::Value>,
    dependency_kind: Option<String>,
) -> RuntimeResult<Option<serde_json::Value>> {
    validate_metadata_shape(metadata.as_ref())?;
    let metadata = metadata.filter(|value| !value.is_null());
    let Some(dk) = dependency_kind else {
        return Ok(metadata);
    };
    let mut obj = metadata.unwrap_or_else(|| serde_json::json!({}));
    let map = obj
        .as_object_mut()
        .ok_or_else(|| RuntimeError::InvalidInput("metadata must be a JSON object".into()))?;
    map.entry("dependency_kind".to_string())
        .or_insert_with(|| serde_json::json!(dk));
    Ok(Some(obj))
}

/// Valid `dependency_kind` values for `depends_on` edges.
const VALID_DEPENDENCY_KINDS: &[&str] = &[
    "build",
    "runtime",
    "data",
    "artifact",
    "tooling",
    "normative",
];

/// Validate that an edge weight is finite and within `[0.0, 1.0]`.
///
/// Rejects NaN, infinities, negative values, and values exceeding 1.0.
/// Used by `link` and `import_kg` to enforce the weight invariant consistently
/// across all edge creation paths.
pub(crate) fn validate_edge_weight(weight: f64) -> RuntimeResult<()> {
    if !khive_types::validate_edge_weight(weight) {
        return Err(RuntimeError::InvalidInput(format!(
            "edge weight must be finite and in [0.0, 1.0], got {weight}"
        )));
    }
    Ok(())
}

fn validate_metadata_shape(metadata: Option<&serde_json::Value>) -> RuntimeResult<()> {
    if metadata.is_some_and(|value| !value.is_null() && !value.is_object()) {
        return Err(RuntimeError::InvalidInput(
            "metadata must be a JSON object".into(),
        ));
    }
    Ok(())
}

/// Validate governed edge metadata keys.
///
/// Enforces object shape, the `depends_on` scope and vocabulary of
/// `dependency_kind`, and the boolean type of `optional`.
pub(crate) fn validate_edge_metadata(
    relation: EdgeRelation,
    metadata: Option<&serde_json::Value>,
) -> RuntimeResult<()> {
    validate_metadata_shape(metadata)?;
    let Some(meta) = metadata.filter(|value| !value.is_null()) else {
        return Ok(());
    };
    let object = meta.as_object().expect("validated metadata object");
    if object
        .get("optional")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err(RuntimeError::InvalidInput(
            "metadata.optional must be a boolean".into(),
        ));
    }
    if let Some(dk) = meta.get("dependency_kind") {
        if relation != EdgeRelation::DependsOn {
            return Err(RuntimeError::InvalidInput(format!(
                "dependency_kind is only valid on depends_on edges (got {})",
                relation.as_str()
            )));
        }
        let dk_str = dk
            .as_str()
            .ok_or_else(|| RuntimeError::InvalidInput("dependency_kind must be a string".into()))?;
        if !VALID_DEPENDENCY_KINDS.contains(&dk_str) {
            return Err(RuntimeError::InvalidInput(format!(
                "unknown dependency_kind {dk_str:?}; valid: {}",
                VALID_DEPENDENCY_KINDS.join(" | ")
            )));
        }
    }
    Ok(())
}

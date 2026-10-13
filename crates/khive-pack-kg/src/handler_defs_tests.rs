use super::*;

fn find_handler(name: &str) -> &'static HandlerDef {
    KG_HANDLERS
        .iter()
        .find(|h| h.name == name)
        .unwrap_or_else(|| panic!("handler {name:?} not found in KG_HANDLERS"))
}

#[test]
fn count_verb_help_explains_edge_count_scopes_and_ledger_delta() {
    let stats = find_handler("stats");
    assert!(
        stats.description.contains("caller-visible namespaces")
            && stats.description.contains("live rows"),
        "stats help must disclose its namespace and deletion scope"
    );

    let diagnostics = find_handler("db_diagnostics");
    for required in [
        "reader admission capacity",
        "maximum completed hold time",
        "all namespaces",
        "soft-deleted rows",
        "hard deletion",
        "graph_edges_seq_minus_graph_edges",
    ] {
        assert!(
            diagnostics.description.contains(required),
            "db_diagnostics help must explain {required:?}"
        );
    }
}

/// `link`'s id params resolve through `resolve_uuid_unfiltered`, which passes no
/// namespace predicate at all (`KhiveRuntime::resolve_prefix_unfiltered` ->
/// `resolve_prefix_inner(None, ..)`). Their help text said the lookup searched "every
/// namespace you can see", which is a visibility-bounded reach and a narrower claim than
/// the truth, while the shared id contract appended to the same description said "no
/// namespace filter" — so one description asserted both readings at once and a caller
/// could take either.
///
/// The must-not-match arm is the load-bearing one: a description can state the correct
/// rule and still carry the contradicting sentence beside it, which is exactly the state
/// this test was written to end.
#[test]
fn link_id_params_state_unfiltered_resolution_and_not_a_visible_set() {
    let link = find_handler("link");
    for name in ["source_id", "target_id"] {
        let param = link
            .params
            .iter()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("link must declare {name:?}"));
        assert!(
            param.description.contains("no namespace filter"),
            "link.{name} help must state that UUID and prefix lookup are unfiltered; got: {}",
            param.description
        );
        assert!(
            !param.description.contains("every namespace you can see"),
            "link.{name} help must not describe resolution as bounded by the caller's \
                 visible set — it is not bounded at all; got: {}",
            param.description
        );
        assert!(
            param.description.contains("primary namespace"),
            "link.{name} help must keep the entity-name fallback's namespace scope, which \
                 IS the caller's primary namespace; got: {}",
            param.description
        );
    }
}

/// Regression for #899: `create.entity_kind`/`list.entity_kind` help text must list
/// every canonical `EntityKind::NAMES` entry, so a stale hand-written list fails loudly.
#[test]
fn entity_kind_param_descriptions_list_all_canonical_kinds() {
    for handler_name in ["create", "list"] {
        let h = find_handler(handler_name);
        let entity_kind_param = h
            .params
            .iter()
            .find(|p| p.name == "entity_kind")
            .unwrap_or_else(|| panic!("{handler_name}.entity_kind param not found"));
        for kind in crate::vocab::EntityKind::NAMES {
            assert!(
                entity_kind_param.description.contains(kind),
                "{handler_name}.entity_kind description missing canonical kind {kind:?}: {:?}",
                entity_kind_param.description
            );
        }
    }
}

#[test]
fn propose_params_has_required_title_description_changeset() {
    let h = find_handler("propose");
    assert!(!h.params.is_empty(), "propose must have params");
    assert!(
        h.params.iter().any(|p| p.name == "title" && p.required),
        "propose must have required title param"
    );
    assert!(
        h.params
            .iter()
            .any(|p| p.name == "description" && p.required),
        "propose must have required description param"
    );
    assert!(
        h.params.iter().any(|p| p.name == "changeset" && p.required),
        "propose must have required changeset param"
    );
}

#[test]
fn link_help_schema_matches_strict_singleton_and_bulk_params() {
    let handler = find_handler("link");
    let advertised = handler
        .params
        .iter()
        .map(|param| param.name)
        .collect::<Vec<_>>();
    assert_eq!(
        advertised,
        [
            "source_id",
            "target_id",
            "relation",
            "weight",
            "metadata",
            "dependency_kind",
            "resurrect",
            "verbose",
            "links",
            "atomic",
        ],
        "link help must enumerate the complete LinkParams surface in wire order"
    );
    assert!(
        handler.params.iter().all(|param| !param.required),
        "link has a singleton-or-bulk contract, so no one field is unconditionally required"
    );

    let accepted = serde_json::from_value::<crate::handlers::LinkParams>(serde_json::json!({
        "source_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
        "target_id": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
        "relation": "depends_on",
        "weight": 0.7,
        "metadata": {"optional": true},
        "dependency_kind": "build",
        "resurrect": true,
        "verbose": true,
        "links": [],
        "atomic": false,
    }));
    if let Err(error) = accepted {
        panic!("every link help parameter must be accepted by LinkParams: {error}");
    }

    for payload in [
        serde_json::json!({"properties": {"x": 1}}),
        serde_json::json!({
            "links": [{
                "source_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                "target_id": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
                "relation": "extends",
                "properties": {"x": 1},
            }],
        }),
    ] {
        let error = match serde_json::from_value::<crate::handlers::LinkParams>(payload) {
            Ok(_) => panic!("unknown link fields must be rejected"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains("unknown field") && error.contains("properties"),
            "link must reject unknown fields explicitly, got: {error}"
        );
    }
}

#[test]
fn propose_params_has_optional_reviewers_expiry_parent_id() {
    let h = find_handler("propose");
    assert!(
        h.params
            .iter()
            .any(|p| p.name == "reviewers" && !p.required),
        "propose must document optional reviewers"
    );
    assert!(
        h.params.iter().any(|p| p.name == "expiry" && !p.required),
        "propose must document optional expiry"
    );
    assert!(
        h.params
            .iter()
            .any(|p| p.name == "parent_id" && !p.required),
        "propose must document optional parent_id"
    );
}

#[test]
fn review_params_has_required_id_and_decision() {
    let h = find_handler("review");
    assert!(!h.params.is_empty(), "review must have params");
    assert!(
        h.params.iter().any(|p| p.name == "id" && p.required),
        "review must have required id param"
    );
    assert!(
        h.params.iter().any(|p| p.name == "decision" && p.required),
        "review must have required decision param"
    );
    assert!(
        h.params.iter().any(|p| p.name == "comment" && !p.required),
        "review must document optional comment param"
    );
}

/// Locate the substring of `desc` documenting `relation` — either its own
/// `"{relation}:"` clause, its `"{relation} (symmetric):"` clause, or a
/// grouped `"a / b:"` clause (used for `supports / refutes`).
fn relation_clause<'a>(desc: &'a str, relation: &str) -> &'a str {
    desc.split(". ")
        .find(|c| {
            c.contains(&format!("{relation}:"))
                || c.contains(&format!("{relation} ("))
                || c.contains(&format!("{relation} /"))
                || c.contains(&format!("/ {relation}"))
        })
        .unwrap_or_else(|| {
            panic!("link.relation help must document a clause for relation `{relation}`")
        })
}

fn endpoint_kind_label(kind: &khive_types::EndpointKind) -> String {
    match kind {
        khive_types::EndpointKind::EntityOfKind(k) => (*k).to_string(),
        khive_types::EndpointKind::NoteOfKind(k) => format!("{k}-note"),
        khive_types::EndpointKind::EntityOfType { kind, .. } => (*kind).to_string(),
    }
}

/// Regression for #964: `link(help=true)` must surface the per-relation
/// edge-endpoint allowlist so batch appliers can defer to the kernel's own
/// table instead of reimplementing (and drifting from) it.
///
/// Full-coverage drift tripwire (#1060): derives the
/// expected rows from the live rule sources (`base_entity_endpoint_rules()`
/// and `KG_EDGE_RULES`) instead of asserting a handful of substrings, so a
/// typo'd or dropped row in an untested relation (e.g. `derived_from`,
/// `depends_on`, the epistemic pairs) fails the test rather than shipping
/// a stale contract.
#[test]
fn link_relation_param_documents_edge_endpoint_allowlist() {
    let h = find_handler("link");
    let relation_param = h
        .params
        .iter()
        .find(|p| p.name == "relation")
        .expect("link must document a relation param");
    let desc = relation_param.description;

    for (src, relation, tgt) in khive_runtime::base_entity_endpoint_rules() {
        let rel = relation.as_str();
        let clause = relation_clause(desc, rel);
        if relation.is_symmetric() {
            let a = format!("{src}<->{tgt}");
            let b = format!("{tgt}<->{src}");
            assert!(
                clause.contains(&a) || clause.contains(&b),
                "link.relation help missing symmetric base row {rel}: {src}<->{tgt}"
            );
        } else {
            let row = format!("{src}->{tgt}");
            assert!(
                clause.contains(&row),
                "link.relation help missing base row {rel}: {row}\nclause: {clause}"
            );
        }
    }

    // The three same-substrate note->note families (ADR-055/ADR-002) must
    // be documented alongside their entity->entity cases.
    for rel in ["supersedes", "supports", "refutes"] {
        let clause = relation_clause(desc, rel);
        assert!(
            clause.contains("note->note"),
            "link.relation help must document {rel}: note->note (same-substrate only)"
        );
    }

    // annotates: the only relation permitting a note source with any target.
    assert!(
        desc.contains("annotates: note ->"),
        "link.relation help must document the annotates note->* endpoint"
    );

    // kg pack's additive EDGE_RULES — every (relation, source, target)
    // TRIPLE must appear in the pack clause, not merely the endpoint pair.
    // The clause groups relations that share an endpoint pair
    // ("part_of/instance_of person->org"), so a row is verified by finding a
    // comma-delimited segment that carries BOTH the relation token AND the
    // "src->tgt" endpoint — deleting a relation from a grouped run (e.g.
    // dropping instance_of but keeping part_of) then fails the test.
    let kg_clause = desc
        .split(". ")
        .find(|c| c.contains("kg` pack additionally allows"))
        .expect("link.relation help must document the kg pack's additive EDGE_RULES clause");
    for rule in crate::pack::KG_EDGE_RULES.iter() {
        let src = endpoint_kind_label(&rule.source);
        let tgt = endpoint_kind_label(&rule.target);
        let row = format!("{src}->{tgt}");
        let rel = rule.relation.as_str();
        let matched = kg_clause
            .split(", ")
            .any(|seg| seg.contains(&row) && seg.contains(rel));
        assert!(
            matched,
            "kg pack EDGE_RULES triple missing from help: {rel} {row}\nclause: {kg_clause}"
        );
    }
}

#[test]
fn withdraw_params_has_required_id_and_optional_rationale() {
    let h = find_handler("withdraw");
    assert!(!h.params.is_empty(), "withdraw must have params");
    assert!(
        h.params.iter().any(|p| p.name == "id" && p.required),
        "withdraw must have required id param"
    );
    assert!(
        h.params
            .iter()
            .any(|p| p.name == "rationale" && !p.required),
        "withdraw must document optional rationale param"
    );
}

// ── update/help param-documentation regressions ──────────────────────────

#[test]
fn update_params_documents_note_tag_replacement_and_precedence() {
    let h = find_handler("update");
    assert!(h
        .description
        .contains("name/content/salience/decay_factor/properties/tags"));
    let tags = h.params.iter().find(|p| p.name == "tags").unwrap();
    for detail in [
        "entities or notes",
        "omission preserves",
        "[] clears",
        "properties.tags",
        "takes precedence",
        "including when empty",
    ] {
        assert!(
            tags.description.contains(detail),
            "missing tag contract: {detail}"
        );
    }
}

/// ADR-172 Amendment 5, acceptance arm 6: both routes state which rule
/// an identical write gets.
#[test]
fn help_states_that_a_fenced_identical_write_still_mints_a_version() {
    let update = find_handler("update");
    let expected = update
        .params
        .iter()
        .find(|p| p.name == "expected_version")
        .expect("update documents expected_version");
    assert!(
        expected
            .description
            .contains("changes nothing is still a write")
            && expected.description.contains("unchanged=true"),
        "update.expected_version must state the fenced-write and no-op rules"
    );
    let batch = find_handler("stream.batch");
    let ops = batch.params.iter().find(|p| p.name == "ops").unwrap();
    assert!(
        ops.description
            .contains("always advances the version by exactly one")
            && ops.description.contains("never a no-op"),
        "stream.batch.ops must state that an accepted write member always mints"
    );
}

/// update.help must document `content` for notes.
#[test]
fn update_params_documents_content_for_notes() {
    let h = find_handler("update");
    assert!(
        h.params.iter().any(|p| p.name == "content"),
        "update must document 'content' param (notes only)"
    );
    let content = h.params.iter().find(|p| p.name == "content").unwrap();
    assert!(
        content.description.contains("note"),
        "update.content description must mention 'note'"
    );
}

/// update.name must NOT say "entities only".
#[test]
fn update_params_name_not_entities_only() {
    let h = find_handler("update");
    let name_param = h.params.iter().find(|p| p.name == "name").unwrap();
    assert!(
        !name_param.description.contains("entities only"),
        "update.name must not claim 'entities only' — notes also have names"
    );
}

/// update.help must document `salience` for notes.
#[test]
fn update_params_documents_salience_for_notes() {
    let h = find_handler("update");
    assert!(
        h.params.iter().any(|p| p.name == "salience"),
        "update must document 'salience' param (notes only)"
    );
}

/// update.help must document `decay_factor` for notes.
#[test]
fn update_params_documents_decay_factor_for_notes() {
    let h = find_handler("update");
    assert!(
        h.params.iter().any(|p| p.name == "decay_factor"),
        "update must document 'decay_factor' param (notes only)"
    );
}

/// update.help must document `relation` for edges.
#[test]
fn update_params_documents_relation_for_edges() {
    let h = find_handler("update");
    assert!(
        h.params.iter().any(|p| p.name == "relation"),
        "update must document 'relation' param (edges only)"
    );
    let rel = h.params.iter().find(|p| p.name == "relation").unwrap();
    assert!(
        rel.description.contains("edge"),
        "update.relation description must mention 'edge'"
    );
}

/// update.help must document `weight` for edges.
#[test]
fn update_params_documents_weight_for_edges() {
    let h = find_handler("update");
    assert!(
        h.params.iter().any(|p| p.name == "weight"),
        "update must document 'weight' param (edges only)"
    );
    let w = h.params.iter().find(|p| p.name == "weight").unwrap();
    assert!(
        w.description.contains("edge"),
        "update.weight description must mention 'edge'"
    );
}

// ── ue-help-introspection C3 regression ──────────────────────────────────

/// No handler named "thread" should exist in the KG pack (guards against accidental addition).
#[test]
fn no_thread_verb_in_kg_handlers() {
    assert!(
        KG_HANDLERS.iter().all(|h| h.name != "thread"),
        "KG_HANDLERS must not contain a 'thread' handler — see C3"
    );
}

// ── ue-help-introspection H5 regression ──────────────────────────────────

/// The `verbs` introspection handler must be present and have params.
#[test]
fn verbs_handler_is_present_and_has_params() {
    let h = find_handler("verbs");
    assert!(
        !h.params.is_empty(),
        "verbs must have documented params (category, pack)"
    );
    assert!(
        h.params.iter().any(|p| p.name == "category"),
        "verbs must document 'category' filter param"
    );
    assert!(
        h.params.iter().any(|p| p.name == "pack"),
        "verbs must document 'pack' filter param"
    );
}

#[test]
fn query_help_documents_mixed_variable_chain_limitation() {
    let h = find_handler("query");
    assert!(
        h.description
            .contains("mixes fixed-length and variable-length"),
        "query help must document mixed fixed/variable traversal limitation"
    );
    let query_param = h
        .params
        .iter()
        .find(|p| p.name == "query")
        .expect("query param documented");
    assert!(
        query_param
            .description
            .contains("split them into separate query() calls"),
        "query param help must document split-query workaround"
    );
    let page_size_param = h
        .params
        .iter()
        .find(|p| p.name == "page_size")
        .expect("page_size param must be documented in query handler metadata");
    assert!(!page_size_param.required, "page_size must be optional");
    assert!(
        page_size_param.description.contains("hard cap 10 000")
            && page_size_param.description.contains("Query-text LIMIT"),
        "page_size help must document hard-cap and query-LIMIT composition"
    );
    let limit_param = h
        .params
        .iter()
        .find(|p| p.name == "limit")
        .expect("limit param must be documented in query handler metadata");
    assert!(!limit_param.required, "limit must be optional");
    assert!(
        limit_param.description.contains("Deprecated alias"),
        "legacy limit must be explicitly documented as an alias"
    );
}

// ── issue #160 return-shape regressions ──────────────────────────────────

/// propose returns {id, ...}; the correct chain key is $prev.id, not $prev.proposal_id (#160).
/// The description may mention $prev.proposal_id in a "not this" warning, which is fine.
#[test]
fn propose_description_documents_id_field_not_proposal_id() {
    let h = find_handler("propose");
    assert!(
        h.description.contains("Returns {id"),
        "propose description must name the 'id' return field"
    );
    assert!(
        h.description.contains("$prev.id"),
        "propose description must document chaining via $prev.id"
    );
    // The description warns callers off $prev.proposal_id by name; the critical
    // check is that $prev.id appears first as the authoritative form.
    let id_pos = h
        .description
        .find("$prev.id")
        .expect("$prev.id must appear in propose description");
    let proposal_id_pos = h.description.find("$prev.proposal_id");
    if let Some(pid_pos) = proposal_id_pos {
        // $prev.proposal_id is only acceptable when it appears AFTER $prev.id
        // (i.e., as a negative example, not as the recommended form).
        assert!(
            id_pos < pid_pos,
            "propose description must present $prev.id before $prev.proposal_id"
        );
    }
}

/// merge returns {kept_id, removed_id, ...}; no top-level 'id' field.
/// Chain with $prev.kept_id, not $prev.id (#160).
#[test]
fn merge_description_documents_kept_id_and_removed_id_return_fields() {
    let h = find_handler("merge");
    assert!(
        h.description.contains("kept_id") && h.description.contains("removed_id"),
        "merge description must name both kept_id and removed_id return fields"
    );
    assert!(
        h.description.contains("$prev.kept_id"),
        "merge description must document chaining via $prev.kept_id"
    );
}

#[test]
fn merge_metadata_documents_the_complete_wire_contract() {
    let h = find_handler("merge");
    assert!(
        h.description.contains("entities or notes"),
        "merge description must cover both supported substrates"
    );
    for required in ["into_id", "from_id"] {
        assert!(
            h.params.iter().any(|p| p.name == required && p.required),
            "merge must document required {required}"
        );
    }
    for optional in [
        "kind",
        "strategy",
        "content_strategy",
        "dry_run",
        "force",
        "reason",
    ] {
        assert!(
            h.params.iter().any(|p| p.name == optional && !p.required),
            "merge must document optional {optional}"
        );
    }
    assert!(
        h.description.contains("details.guard") && h.description.contains("details.compared"),
        "merge must document the structured guard refusal and what the guard compared"
    );
    // The refusal a dry run predicts is part of the verb's contract, and the
    // field a caller reads it from has to be named where the caller looks.
    assert!(
        h.description.contains("would_merge"),
        "merge must document that a dry run reports the floor it would hit"
    );
}

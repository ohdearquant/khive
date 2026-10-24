//! ADR-084 Amendment 1, A1: every handler linked into this MCP build.
//! Naming roles are reviewed exact pairs, not spelling/optionality heuristics.
//! The historical ceiling is owner-approved in #5996 and must never grow here.

use std::collections::{BTreeMap, BTreeSet};

use khive_mcp::pack::{KhiveRuntime, PackRegistry, VerbRegistryBuilder};
use khive_runtime::RuntimeConfig;
use khive_types::{IdResolutionMode, ParamDef};

type Pair = (&'static str, &'static str);

#[derive(Clone, Copy, Debug)]
enum Role {
    PrimaryAlias,
    DistinctRole(&'static str),
}

#[derive(Clone, Copy, Debug)]
struct Classification {
    verb: &'static str,
    param: &'static str,
    role: Role,
}

// Exhaustive for the current noncanonical UUID/recurring-name candidates.
// Optional-pack rows, if ever needed, must use that exact MCP pack's cfg;
// missing verbs in a compiled pack must never be silently skipped.
const CLASSIFICATIONS: &[Classification] = &[
    Classification { verb: "list", param: "source_id", role: Role::DistinctRole("Filters an edge collection by the source endpoint, paired with an independently usable target endpoint filter.") },
    Classification { verb: "list", param: "target_id", role: Role::DistinctRole("Filters edges by target endpoint or events by subject; it does not select a single primary list record.") },
    Classification { verb: "list", param: "session_id", role: Role::DistinctRole("Filters the returned event collection by the producing session UUID.") },
    Classification { verb: "list", param: "observed", role: Role::DistinctRole("Requires each returned event to have observed every listed record; these are event-evidence filters.") },
    Classification { verb: "list", param: "selected", role: Role::DistinctRole("Requires each returned event to have selected every listed record; these are event-evidence filters.") },
    Classification { verb: "list", param: "thread_id", role: Role::DistinctRole("Filters message collection membership by a stored thread group, not by the primary message UUID.") },
    Classification { verb: "merge", param: "into_id", role: Role::DistinctRole("Identifies the survivor endpoint of a two-record merge, distinct from the consumed endpoint.") },
    Classification { verb: "merge", param: "from_id", role: Role::DistinctRole("Identifies the consumed endpoint of a two-record merge, distinct from the survivor endpoint.") },
    Classification { verb: "link", param: "source_id", role: Role::DistinctRole("Identifies the directed edge source endpoint, paired with a separately resolved target endpoint.") },
    Classification { verb: "link", param: "target_id", role: Role::DistinctRole("Identifies the directed edge target endpoint, paired with a separately resolved source endpoint.") },
    Classification { verb: "propose", param: "parent_id", role: Role::DistinctRole("References a preexisting proposal in the ancestry of the newly created proposal; it is not the new proposal identifier.") },
    Classification { verb: "gtd.assign", param: "due", role: Role::DistinctRole("Stores the new task deadline, including date-only timezone anchoring; it is not a deferred-dispatch schedule time.") },
    Classification { verb: "gtd.assign", param: "depends_on", role: Role::DistinctRole("References blocking task records used to create dependency edges for the newly assigned task.") },
    Classification { verb: "gtd.assign", param: "context_entity_id", role: Role::DistinctRole("References the KG context entity attached to the newly assigned task, distinct from the task identifier.") },
    Classification { verb: "gtd.complete", param: "duplicate_of", role: Role::DistinctRole("References the kept task recorded when cancelling the separately identified task as a duplicate.") },
    Classification { verb: "gtd.tasks", param: "duplicate_of", role: Role::DistinctRole("Filters a collection of duplicate judgements by its kept-task reference.") },
    Classification { verb: "gtd.tasks", param: "context_entity_id", role: Role::DistinctRole("Filters task collection membership by its stored KG context reference, including after the context is deleted.") },
    Classification { verb: "gtd.transition", param: "duplicate_of", role: Role::DistinctRole("References the kept task recorded when cancelling the separately identified transitioning task.") },
    Classification { verb: "brain.feedback", param: "target_id", role: Role::PrimaryAlias },
    Classification { verb: "brain.emit", param: "target_id", role: Role::PrimaryAlias },
    Classification { verb: "comm.suppression_remove", param: "evidence_inbound_message_id", role: Role::DistinctRole("Supplies the inbound evidence note for removing the separately identified suppression entry.") },
    Classification { verb: "comm.send", param: "thread_id", role: Role::DistinctRole("Groups a newly created message under an existing thread root; it does not identify the new message.") },
    Classification { verb: "comm.inbox", param: "thread_id", role: Role::DistinctRole("Filters inbox/outbox collection membership by stored thread group before pagination.") },
    Classification { verb: "comm.ingest", param: "thread_id", role: Role::DistinctRole("Assigns thread-group membership to a newly ingested message, whose external identity is separate.") },
    Classification { verb: "knowledge.cite", param: "concept_id", role: Role::DistinctRole("Identifies the concept endpoint of the newly created introduced_by relation.") },
    Classification { verb: "knowledge.cite", param: "source_id", role: Role::DistinctRole("Identifies the source endpoint of the newly created introduced_by relation.") },
    Classification { verb: "git.ingest_cursor", param: "project", role: Role::DistinctRole("Scopes the cursor/checkpoint pair by project anchor plus source_kind; the returned cursor records are not the project record.") },
];

#[derive(Clone, Copy, Debug)]
struct LegacyPair {
    verb: &'static str,
    param: &'static str,
    issue: u32,
}

// Historical approved population: retain a row here after fixing its metadata.
// Current violations still have to equal ACTIVE_BASELINE independently.
const APPROVED_CEILING: &[LegacyPair] = &[
    LegacyPair {
        verb: "neighbors",
        param: "node_id",
        issue: 5996,
    },
    LegacyPair {
        verb: "traverse",
        param: "roots",
        issue: 5996,
    },
    LegacyPair {
        verb: "memory.feedback",
        param: "target_id",
        issue: 5996,
    },
    LegacyPair {
        verb: "brain.feedback",
        param: "target_id",
        issue: 5996,
    },
    LegacyPair {
        verb: "brain.emit",
        param: "target_id",
        issue: 5996,
    },
];

// Shrink only. Each still-invalid pair cites the owner's actual tracking issue.
const ACTIVE_BASELINE: &[LegacyPair] = &[
    LegacyPair {
        verb: "brain.feedback",
        param: "target_id",
        issue: 5996,
    },
    LegacyPair {
        verb: "brain.emit",
        param: "target_id",
        issue: 5996,
    },
];

#[derive(Clone, Copy)]
struct ParamView {
    verb: &'static str,
    name: &'static str,
    param_type: &'static str,
}

impl ParamView {
    fn from_def(verb: &'static str, param: &ParamDef) -> Self {
        Self {
            verb,
            name: param.name,
            param_type: param.param_type,
        }
    }

    fn pair(self) -> Pair {
        (self.verb, self.name)
    }

    fn is_candidate(self) -> bool {
        let eligible = matches!(self.param_type, "uuid" | "array of uuid")
            || matches!(self.name, "id" | "ids");
        // Recurring-name checks are independent of the declared type hint.
        matches!(self.name, "q" | "slugs" | "thread_id" | "due")
            || (eligible && !matches!(self.name, "id" | "ids"))
    }
}

fn naming_diagnostics(
    params: &[ParamView],
    classifications: &[Classification],
    baseline: &[LegacyPair],
    ceiling: &[LegacyPair],
) -> Vec<String> {
    let mut errors = Vec::new();
    let present: BTreeSet<Pair> = params
        .iter()
        .copied()
        .filter(|param| param.is_candidate())
        .map(ParamView::pair)
        .collect();
    let mut roles = BTreeMap::new();
    for row in classifications {
        let pair = (row.verb, row.param);
        if roles.insert(pair, row.role).is_some() {
            errors.push(format!("DUPLICATE_CLASSIFICATION: {}.{}", pair.0, pair.1));
        }
        if !present.contains(&pair) {
            errors.push(format!("STALE_CLASSIFICATION: {}.{}", pair.0, pair.1));
        }
        if matches!(row.role, Role::DistinctRole(reason) if reason.trim().is_empty()) {
            errors.push(format!("EMPTY_ROLE_REASON: {}.{}", pair.0, pair.1));
        }
    }

    // Compute violations before consulting any baseline. Missing classifications
    // are violations as well as errors, never an automatic exception.
    let mut violations = BTreeSet::new();
    for pair in &present {
        match roles.get(pair) {
            Some(Role::DistinctRole(_)) => {}
            Some(Role::PrimaryAlias) => {
                violations.insert(*pair);
            }
            None => {
                errors.push(format!("MISSING_CLASSIFICATION: {}.{}", pair.0, pair.1));
                violations.insert(*pair);
            }
        }
    }

    let mut approved = BTreeMap::new();
    for row in ceiling {
        let pair = (row.verb, row.param);
        if approved.insert(pair, row.issue).is_some() {
            errors.push(format!("DUPLICATE_CEILING: {}.{}", pair.0, pair.1));
        }
        if row.issue == 0 {
            errors.push(format!("INVALID_CEILING_ISSUE: {}.{}", pair.0, pair.1));
        }
    }
    let mut active = BTreeSet::new();
    for row in baseline {
        let pair = (row.verb, row.param);
        if !active.insert(pair) {
            errors.push(format!("DUPLICATE_BASELINE: {}.{}", pair.0, pair.1));
        }
        match approved.get(&pair) {
            None => errors.push(format!(
                "BASELINE_GROWTH: {}.{} is outside the approved ceiling",
                pair.0, pair.1
            )),
            Some(issue) if *issue != row.issue || row.issue == 0 => {
                errors.push(format!("BASELINE_ISSUE_MISMATCH: {}.{}", pair.0, pair.1))
            }
            Some(_) => {}
        }
        if !violations.contains(&pair) {
            errors.push(format!(
                "STALE_BASELINE: {}.{} no longer violates",
                pair.0, pair.1
            ));
        }
    }
    for pair in violations.difference(&active) {
        errors.push(format!("UNBASELINED: {}.{}", pair.0, pair.1));
    }
    errors
}

#[test]
fn compiled_handler_metadata_matches_shrink_only_legacy_baseline() {
    // MCP is the composition boundary: runtime has no upper-pack dev dependencies.
    // Discover the linked population, including Charter and optional pack features.
    let mut names: Vec<String> = PackRegistry::discovered_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    names.sort();
    assert!(
        !names.is_empty(),
        "compiled factory population must not be empty"
    );
    assert_eq!(
        names.iter().collect::<BTreeSet<_>>().len(),
        names.len(),
        "duplicate pack factories"
    );
    for required in RuntimeConfig::built_in_packs() {
        assert!(
            names.contains(&required),
            "missing default-linked pack {required}"
        );
    }
    assert!(
        names.iter().any(|name| name == "charter"),
        "missing linked charter pack"
    );
    for (name, enabled) in [
        ("agent", cfg!(feature = "pack-agent")),
        ("telemetry", cfg!(feature = "pack-telemetry")),
        ("web", cfg!(feature = "pack-web")),
        ("moodboard", cfg!(feature = "pack-moodboard")),
    ] {
        assert_eq!(
            names.iter().any(|value| value == name),
            enabled,
            "compiled pack feature {name}"
        );
    }
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        brain_profile: None,
        ..RuntimeConfig::no_embeddings()
    })
    .expect("isolated metadata runtime");
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs(&names, runtime, &mut builder)
        .expect("all discovered dependencies are selected");
    let metadata = builder
        .build_metadata()
        .expect("compiled metadata registry without activation");
    let registered: BTreeSet<&str> = metadata.pack_names().into_iter().collect();
    let discovered: BTreeSet<&str> = names.iter().map(String::as_str).collect();
    assert_eq!(
        registered, discovered,
        "registered population equals discovered population"
    );
    let handlers = metadata.all_handlers_with_names();
    assert!(!handlers.is_empty(), "handler census must not be vacuous");
    let mut params = Vec::new();
    let mut errors =
        handler_name_diagnostics(handlers.iter().map(|(pack, handler)| (*pack, handler.name)));
    for (pack, handler) in &handlers {
        if handler.description.trim().is_empty() {
            errors.push(format!("EMPTY_DESCRIPTION: {pack}.{}", handler.name));
        }
        let mut names = BTreeSet::new();
        let help = metadata
            .describe_verb(handler.name)
            .expect("metadata help for every registered handler");
        let declared = help["params"].as_array().expect("help parameter array");
        if declared.len() != handler.params.len() {
            errors.push(format!("HELP_PARAM_COUNT: {}", handler.name));
        }
        for (index, param) in handler.params.iter().enumerate() {
            if !names.insert(param.name) {
                errors.push(format!("DUPLICATE_PARAM: {}.{}", handler.name, param.name));
            }
            // Required flags are checked against generated help. Do not infer
            // optionality or defaults by searching prose or overriding authored
            // JSON Schema alternatives such as get(id) versus get(key).
            let help_param = declared.get(index);
            if help_param.and_then(|value| value["name"].as_str()) != Some(param.name)
                || help_param.and_then(|value| value["required"].as_bool()) != Some(param.required)
            {
                errors.push(format!(
                    "HELP_PARAM_REQUIRED: {}.{}",
                    handler.name, param.name
                ));
            }
            params.push(ParamView::from_def(handler.name, param));
        }
    }
    // Real known-correct controls keep the snapshot tied to a nonempty surface.
    for pair in [
        ("get", "id"),
        ("knowledge.search", "query"),
        ("schedule.remind", "at"),
        ("comm.thread", "id"),
    ] {
        assert!(
            params.iter().any(|param| param.pair() == pair),
            "missing canonical positive {pair:?}"
        );
    }
    let get = handlers
        .iter()
        .find(|(_, handler)| handler.name == "get")
        .unwrap()
        .1;
    assert!(
        !get.params
            .iter()
            .find(|param| param.name == "id")
            .unwrap()
            .required,
        "get-style id is conditional on key"
    );
    errors.extend(naming_diagnostics(
        &params,
        CLASSIFICATIONS,
        ACTIVE_BASELINE,
        APPROVED_CEILING,
    ));
    assert!(
        errors.is_empty(),
        "ADR-084 metadata conformance:\n{}",
        errors.join("\n")
    );
}

fn fixture_param(name: &'static str, param_type: &'static str) -> ParamView {
    ParamView {
        verb: "fresh.lookup",
        name,
        param_type,
    }
}

fn has_error(errors: &[String], prefix: &str) -> bool {
    errors.iter().any(|error| error.starts_with(prefix))
}

#[test]
fn optional_primary_id_passes_without_a_role_exception() {
    let param = ParamDef {
        name: "id",
        param_type: "uuid",
        required: false,
        description: "Optional primary record",
        resolution_mode: IdResolutionMode::UnscopedById,
    };
    assert!(naming_diagnostics(
        &[ParamView::from_def("fresh.lookup", &param)],
        &[],
        &[],
        &[]
    )
    .is_empty());
}

#[test]
fn fresh_thread_id_fails_independently_of_type_hint() {
    for ty in ["uuid", "string", "integer"] {
        let errors = naming_diagnostics(&[fixture_param("thread_id", ty)], &[], &[], &[]);
        assert!(has_error(
            &errors,
            "MISSING_CLASSIFICATION: fresh.lookup.thread_id"
        ));
        assert!(has_error(&errors, "UNBASELINED: fresh.lookup.thread_id"));
    }
}

#[test]
fn plural_slugs_fails_independently_of_type_hint() {
    for ty in ["array of uuid", "array of string"] {
        let errors = naming_diagnostics(&[fixture_param("slugs", ty)], &[], &[], &[]);
        assert!(has_error(
            &errors,
            "MISSING_CLASSIFICATION: fresh.lookup.slugs"
        ));
        assert!(has_error(&errors, "UNBASELINED: fresh.lookup.slugs"));
    }
}

#[test]
fn cloned_primary_id_named_target_id_has_no_automatic_exception() {
    let errors = naming_diagnostics(&[fixture_param("target_id", "uuid")], &[], &[], &[]);
    assert!(has_error(
        &errors,
        "MISSING_CLASSIFICATION: fresh.lookup.target_id"
    ));
    assert!(has_error(&errors, "UNBASELINED: fresh.lookup.target_id"));
}

#[test]
fn link_endpoints_pass_only_their_exact_distinct_role_rows() {
    let params = [
        ParamView {
            verb: "link",
            name: "source_id",
            param_type: "uuid",
        },
        ParamView {
            verb: "link",
            name: "target_id",
            param_type: "uuid",
        },
    ];
    let roles: Vec<_> = CLASSIFICATIONS
        .iter()
        .copied()
        .filter(|row| row.verb == "link")
        .collect();
    assert_eq!(roles.len(), 2);
    assert!(naming_diagnostics(&params, &roles, &[], &[]).is_empty());
    let errors = naming_diagnostics(&[fixture_param("target_id", "uuid")], &roles, &[], &[]);
    assert!(has_error(
        &errors,
        "MISSING_CLASSIFICATION: fresh.lookup.target_id"
    ));
}

#[test]
fn classification_and_baseline_diagnostics_are_exhaustive() {
    let row = Classification {
        verb: "neighbors",
        param: "node_id",
        role: Role::PrimaryAlias,
    };
    let alias = ParamView {
        verb: "neighbors",
        name: "node_id",
        param_type: "uuid",
    };
    let baseline = [LegacyPair {
        verb: "neighbors",
        param: "node_id",
        issue: 5996,
    }];
    assert!(naming_diagnostics(&[alias], &[row], &baseline, APPROVED_CEILING).is_empty());
    let missing = naming_diagnostics(&[alias], &[], &baseline, APPROVED_CEILING);
    assert!(has_error(
        &missing,
        "MISSING_CLASSIFICATION: neighbors.node_id"
    ));
    let duplicate = naming_diagnostics(&[alias], &[row, row], &baseline, APPROVED_CEILING);
    assert!(has_error(
        &duplicate,
        "DUPLICATE_CLASSIFICATION: neighbors.node_id"
    ));
    let fixed = ParamView {
        verb: "neighbors",
        name: "id",
        param_type: "uuid",
    };
    let stale = naming_diagnostics(&[fixed], &[row], &baseline, APPROVED_CEILING);
    assert!(has_error(&stale, "STALE_CLASSIFICATION: neighbors.node_id"));
    assert!(has_error(&stale, "STALE_BASELINE: neighbors.node_id"));
    // A fixed pair leaves the active table and classification, not history.
    assert!(naming_diagnostics(&[fixed], &[], &[], APPROVED_CEILING).is_empty());
    let removed = naming_diagnostics(&[alias], &[row], &[], APPROVED_CEILING);
    assert!(has_error(&removed, "UNBASELINED: neighbors.node_id"));
}

#[test]
fn classification_plus_matching_baseline_cannot_grow_the_approved_ceiling() {
    let mut roles: Vec<_> = CLASSIFICATIONS
        .iter()
        .copied()
        .filter(|row| matches!(row.role, Role::PrimaryAlias))
        .collect();
    let mut params: Vec<_> = roles
        .iter()
        .map(|row| ParamView {
            verb: row.verb,
            name: row.param,
            param_type: "uuid",
        })
        .collect();
    roles.push(Classification {
        verb: "fresh.lookup",
        param: "target_id",
        role: Role::PrimaryAlias,
    });
    params.push(fixture_param("target_id", "uuid"));
    let mut active = ACTIVE_BASELINE.to_vec();
    active.push(LegacyPair {
        verb: "fresh.lookup",
        param: "target_id",
        issue: 5996,
    });
    let errors = naming_diagnostics(&params, &roles, &active, APPROVED_CEILING);
    assert!(has_error(
        &errors,
        "BASELINE_GROWTH: fresh.lookup.target_id"
    ));
    assert!(
        !has_error(&errors, "UNBASELINED:"),
        "current violations really match the expanded active table: {errors:?}"
    );
    assert!(
        !has_error(&errors, "STALE_BASELINE:"),
        "the injected alias is actually present: {errors:?}"
    );
}

#[test]
fn active_baseline_is_duplicate_free_and_preserves_owner_issue() {
    let param = ParamView {
        verb: "neighbors",
        name: "node_id",
        param_type: "uuid",
    };
    let class = Classification {
        verb: "neighbors",
        param: "node_id",
        role: Role::PrimaryAlias,
    };
    let row = LegacyPair {
        verb: "neighbors",
        param: "node_id",
        issue: 5996,
    };
    let duplicates = naming_diagnostics(&[param], &[class], &[row, row], APPROVED_CEILING);
    assert!(has_error(
        &duplicates,
        "DUPLICATE_BASELINE: neighbors.node_id"
    ));
    let wrong_issue = LegacyPair { issue: 5557, ..row };
    let mismatch = naming_diagnostics(&[param], &[class], &[wrong_issue], APPROVED_CEILING);
    assert!(has_error(
        &mismatch,
        "BASELINE_ISSUE_MISMATCH: neighbors.node_id"
    ));
}

fn handler_name_diagnostics<'a>(
    handlers: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<String> {
    let mut first_packs = BTreeMap::new();
    let mut errors = Vec::new();
    for (pack, name) in handlers {
        if let Some(first_pack) = first_packs.get(name) {
            errors.push(format!(
                "DUPLICATE_HANDLER_NAME: {name} ({first_pack}, {pack})"
            ));
        } else {
            first_packs.insert(name, pack);
        }
    }
    errors
}

#[test]
fn handler_name_diagnostics_reject_duplicates_and_accept_distinct_names() {
    assert!(handler_name_diagnostics([
        ("alpha", "alpha.one"),
        ("alpha", "alpha.two"),
        ("beta", "beta.one"),
    ])
    .is_empty());
    assert_eq!(
        handler_name_diagnostics([("alpha", "internal.shared"), ("beta", "internal.shared"),]),
        vec!["DUPLICATE_HANDLER_NAME: internal.shared (alpha, beta)".to_owned()],
        "duplicate handler names must be reported"
    );
    assert_eq!(
        handler_name_diagnostics([("alpha", "internal.shared"), ("alpha", "internal.shared"),]),
        vec!["DUPLICATE_HANDLER_NAME: internal.shared (alpha, alpha)".to_owned()]
    );
}

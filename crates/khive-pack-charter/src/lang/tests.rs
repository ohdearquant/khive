use super::ast::EntryKind;
use super::definition::{Route, WhenSource};
use super::registry::{Registry, CORE_VOCABULARY};
use super::*;

const PR_REVIEW: &str = include_str!("../../tests/fixtures/pr_review.charter");
const PR_FIX: &str = include_str!("../../tests/fixtures/pr_fix.charter");
const PR_SPLIT: &str = include_str!("../../tests/fixtures/pr_split.charter");
const REVIEW_ROUND: &str = include_str!("../../tests/fixtures/review_round.charter");
const INBOX_TRIAGE: &str = include_str!("../../tests/fixtures/inbox_triage.charter");

const HEADER: &str = "charter probe v1\nsubject pull_request\nroles:\n    author\n";

fn refused(source: &str) -> Vec<Diagnostic> {
    match compile(source, &Registry::core()) {
        Ok(compiled) => panic!("expected a refusal, compiled {:?}", compiled.summary()),
        Err(errors) => errors,
    }
}

fn assert_refused(body: &str, code: &str) {
    let source = format!("{HEADER}{body}");
    let errors = refused(&source);
    assert!(
        errors.iter().any(|d| d.code == code),
        "expected {code:?}, got {errors:?}\n--- source ---\n{source}"
    );
}

#[test]
fn core_vocabulary_parses_and_declares_every_kind() {
    let registry = Registry::core();
    for kind in [
        EntryKind::Subject,
        EntryKind::Trigger,
        EntryKind::Phrase,
        EntryKind::Action,
    ] {
        assert!(!registry.names_of(&kind).is_empty(), "{kind:?}");
    }
    let decision = registry.get("review.decision").expect("review.decision");
    assert_eq!(decision.outcomes, ["approve", "request_changes", "blocked"]);
    assert_eq!(decision.passes, ["approve"]);
    assert!(registry
        .get("message.kind")
        .expect("message.kind")
        .passes
        .is_empty());
    assert_eq!(
        registry.get("task.status").expect("task.status").outcomes,
        [
            "inbox",
            "next",
            "waiting",
            "someday",
            "active",
            "done",
            "cancelled"
        ]
    );
    assert_eq!(
        registry
            .get("graph.edge_present")
            .expect("graph.edge_present")
            .outcomes,
        ["present", "absent"]
    );
    assert_eq!(
        registry.get("work.unit").expect("work.unit").passes,
        ["done", "skipped"]
    );
}

#[test]
fn the_five_fixture_charters_compile() {
    let registry = Registry::core();
    for source in [PR_REVIEW, PR_FIX, PR_SPLIT, REVIEW_ROUND, INBOX_TRIAGE] {
        let compiled = compile(source, &registry).unwrap_or_else(|e| panic!("{e:?}\n{source}"));
        assert!(compiled.definition_digest.starts_with("blake3:"));
        assert_eq!(
            compiled.definition.gate_registry_digest,
            compiled.gate_registry_digest
        );
    }
}

#[test]
fn an_await_can_name_a_registered_phrase() {
    let source = format!(
        "{HEADER}workflow:\n    phase wait:\n        await message.kind\n        complete\n"
    );
    compile(&source, &Registry::core()).expect("phrases may release an awaited phase");
}

#[test]
fn pr_review_compiles_to_the_declared_topology() {
    let compiled = compile(PR_REVIEW, &Registry::core()).expect("pr_review");
    let d = &compiled.definition;
    let ids: Vec<&str> = d.phases.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(
        ids,
        ["intake", "review", "fix", "split", "approval", "merge"]
    );
    let review = d.phase("review").unwrap();
    assert_eq!(review.max_rounds.as_ref().unwrap().rounds, 3);
    assert_eq!(review.whens.len(), 3);
    assert_eq!(review.whens[1].label, "request_changes");
    assert_eq!(
        review.whens[1].route,
        Route::Goto {
            phase: "fix".into()
        }
    );
    let fix = d.phase("fix").unwrap();
    assert_eq!(fix.performs, ["pr_fix"]);
    assert_eq!(fix.whens[0].source, WhenSource::Charter);
    let intake = d.phase("intake").unwrap();
    assert_eq!(
        intake.next,
        Route::Goto {
            phase: "review".into()
        }
    );
    assert_eq!(intake.requires[1].args["branch"], "main");
    assert_eq!(d.phase("merge").unwrap().next, Route::Complete);
    assert!(d.registry.contains_key("review.decision"));
    assert!(!d.registry.contains_key("message.kind"));
}

#[test]
fn compilation_is_deterministic_and_comments_do_not_change_the_definition() {
    let registry = Registry::core();
    let a = compile(PR_REVIEW, &registry).unwrap();
    let b = compile(PR_REVIEW, &registry).unwrap();
    assert_eq!(a.definition_bytes, b.definition_bytes);
    let commented = PR_REVIEW.replace(
        "    phase fix by author:",
        "    # answer the findings\n    phase fix by author:  # one round",
    );
    let c = compile(&commented, &registry).unwrap();
    assert_eq!(a.definition_digest, c.definition_digest);
}

#[test]
fn changing_any_referenced_registry_entry_changes_the_gate_registry_digest() {
    let base = compile(PR_REVIEW, &Registry::core()).unwrap();
    let edits = [
        (
            "    outcomes approve request_changes blocked\n    passes approve\n",
            "    outcomes approve request_changes blocked\n    passes approve request_changes\n",
        ),
        (
            "phrase pull_request.base \"the pull request targets the named base branch\":\n    param branch string\n",
            "phrase pull_request.base \"the pull request targets the named base branch\":\n    param branch string\n    param strict bool optional\n",
        ),
        (
            "phrase review.reviewer_independent \"the reviewer is independent of the author\":\n    ceiling hard\n",
            "phrase review.reviewer_independent \"the reviewer is not the author\":\n    ceiling hard\n",
        ),
    ];
    for (from, to) in edits {
        assert!(
            CORE_VOCABULARY.contains(from),
            "fixture edit is stale: {from}"
        );
        let source = parse_vocabulary_source(&CORE_VOCABULARY.replace(from, to)).unwrap();
        let mut registry = Registry::default();
        registry.add(&source).unwrap();
        let changed = compile(PR_REVIEW, &registry).unwrap();
        assert_ne!(
            base.gate_registry_digest, changed.gate_registry_digest,
            "{to}"
        );
        assert_ne!(base.definition_digest, changed.definition_digest, "{to}");
    }
    let unrelated = CORE_VOCABULARY.replace("\"stop every running agent\"", "\"halt agents\"");
    let mut registry = Registry::default();
    registry
        .add(&parse_vocabulary_source(&unrelated).unwrap())
        .unwrap();
    assert_eq!(
        base.gate_registry_digest,
        compile(PR_REVIEW, &registry).unwrap().gate_registry_digest,
        "an entry the charter does not reference is outside its digest"
    );
}

#[test]
fn a_vocabulary_cannot_take_a_name_another_vocabulary_holds() {
    let mut registry = Registry::core();
    let other = parse_vocabulary_source(
        "vocabulary acme v1\nphrase review.decision \"shadow\":\n    ceiling hard\n",
    )
    .unwrap();
    let errors = registry.add(&other).unwrap_err();
    assert_eq!(errors[0].code, "entry_conflict");
    let fresh = parse_vocabulary_source(
        "vocabulary acme v1\nphrase acme.gate \"a deployment gate\":\n    ceiling hard\n",
    )
    .unwrap();
    assert_eq!(registry.add(&fresh).unwrap().len(), 1);
    let source = format!(
        "{HEADER}workflow:\n    phase only:\n        require acme.gate hard\n        complete\n"
    );
    compile(&source, &registry).expect("a published vocabulary extends the registry");
}

#[test]
fn a_subject_kind_names_the_triggers_that_start_a_new_generation() {
    let registry = Registry::core();
    assert_eq!(
        registry
            .get("pull_request")
            .expect("pull_request")
            .new_generation,
        ["pull_request.opened", "pull_request.head_moved"]
    );
    let mut registry = Registry::core();
    let accepted = parse_vocabulary_source(
        "vocabulary acme v1\nsubject widget \"a widget\":\n    new_generation acme.rebuilt \
         pull_request.head_moved\ntrigger acme.rebuilt \"the widget was rebuilt\"\n",
    )
    .unwrap();
    registry
        .add(&accepted)
        .expect("own and core triggers both count");
    for (source, code) in [
        (
            "vocabulary acme v2\nsubject widget \"a widget\":\n    new_generation acme.nope\n",
            "unknown_trigger",
        ),
        (
            "vocabulary acme v2\nsubject widget \"a widget\":\n    new_generation review.decision\n",
            "unknown_trigger",
        ),
        (
            "vocabulary acme v2\ntrigger acme.rebuilt \"rebuilt\":\n    new_generation \
             pull_request.opened\n",
            "bad_entry",
        ),
    ] {
        let errors = Registry::core()
            .add(&parse_vocabulary_source(source).unwrap())
            .unwrap_err();
        assert!(errors.iter().any(|d| d.code == code), "{code}: {errors:?}");
    }
}

#[test]
fn every_refusal_names_its_construct() {
    let cases: &[(&str, &str)] = &[
        (
            "workflow:\n    phase a:\n        require no.such_phrase hard\n        complete\n",
            "unknown_phrase",
        ),
        (
            "workflow:\n    phase a:\n        await no.such_trigger\n        complete\n",
            "unknown_trigger",
        ),
        (
            "workflow:\n    phase a:\n        action no.such_action\n        complete\n",
            "unknown_action",
        ),
        (
            "workflow:\n    phase a:\n        require pull_request.base(target=\"main\") hard\n        complete\n",
            "unknown_argument",
        ),
        (
            "workflow:\n    phase a:\n        require pull_request.base(branch=5) hard\n        complete\n",
            "argument_type",
        ),
        (
            "workflow:\n    phase a:\n        require pull_request.base hard\n        complete\n",
            "missing_argument",
        ),
        (
            "workflow:\n    phase a:\n        require approval.present(true, 1) hard\n        complete\n",
            "positional_argument",
        ),
        (
            "workflow:\n    phase a:\n        when review.decision is maybe: complete\n        complete\n",
            "unknown_label",
        ),
        (
            "workflow:\n    phase a:\n        require agent.no_outward_writes hard\n        complete\n",
            "level_above_ceiling",
        ),
        (
            "workflow:\n    phase a:\n        require message.kind advisory\n        complete\n",
            "no_passing_outcome",
        ),
        (
            "workflow:\n    phase a:\n        when review.decision is approve: goto nowhere\n        complete\n",
            "unknown_phase",
        ),
        (
            "workflow:\n    phase a:\n        complete\n    phase a:\n        complete\n",
            "duplicate_phase",
        ),
        (
            "workflow:\n    phase a:\n        complete\n    phase b:\n        complete\n",
            "unreachable_phase",
        ),
        (
            "workflow:\n    phase a:\n        when review.decision is approve: complete\n    phase b:\n        goto a\n",
            "unbounded_cycle",
        ),
        (
            "workflow:\n    phase a:\n        action work.unit\n    phase b:\n        max_rounds 2 then a\n        goto a\n",
            "no_exit",
        ),
        (
            "workflow:\n    phase a:\n        action work.unit\n",
            "open_end",
        ),
        (
            "workflow:\n    phase a by nobody:\n        complete\n",
            "unknown_role",
        ),
        (
            "triggers:\n    review.posted\nworkflow:\n    phase a:\n        await pull_request.opened\n        complete\n",
            "undeclared_trigger",
        ),
        (
            "workflow:\n    phase a:\n        require review.within_words(words=500) >= 1 advisory\n        complete\n",
            "expression",
        ),
        (
            "workflow:\n    phase a:\n        when message.kind == directive: complete\n        complete\n",
            "expression",
        ),
        (
            "workflow:\n    phase a:\n        require pull_request.base(branch=pr.base) hard\n        complete\n",
            "reference",
        ),
        (
            "workflow:\n    phase a:\n        perform probe\n        complete\n",
            "perform_self",
        ),
        (
            "workflow:\n    phase a:\n        when work.unit is done: complete\n        complete\n",
            "action_not_in_phase",
        ),
        (
            "workflow:\n    phase a:\n        require pull_request.open hard\n        complete\nbreak_glass:\n    by author\n    on a.review.decision\n",
            "bad_break_glass",
        ),
        (
            "workflow:\n    phase a:\n        grant author may no.such_action\n        complete\n",
            "unknown_action",
        ),
        (
            "workflow:\n    phase a:\n        complete\n        action work.unit\n",
            "after_end",
        ),
    ];
    for (body, code) in cases {
        assert_refused(body, code);
    }
}

#[test]
fn a_charter_without_a_known_subject_is_refused() {
    let errors = refused("charter probe v1\nworkflow:\n    phase a:\n        complete\n");
    assert!(
        errors.iter().any(|d| d.code == "missing_subject"),
        "{errors:?}"
    );
    let errors =
        refused("charter probe v1\nsubject spaceship\nworkflow:\n    phase a:\n        complete\n");
    assert!(
        errors.iter().any(|d| d.code == "unknown_subject"),
        "{errors:?}"
    );
}

#[test]
fn tabs_in_indentation_are_refused() {
    let errors =
        refused("charter probe v1\nsubject pull_request\nworkflow:\n\tphase a:\n\t\tcomplete\n");
    assert_eq!(errors[0].code, "indent_tab");
}

#[test]
fn a_bounded_cycle_compiles() {
    let source = format!(
        "{HEADER}workflow:\n    phase review:\n        await review.posted\n        when review.decision is approve: complete\n        max_rounds 3 then give_up\n    phase fix:\n        action fix.push\n        goto review\n    phase give_up:\n        terminate \"too many rounds\"\n"
    );
    compile(&source, &Registry::core()).expect("bounded loop");
}

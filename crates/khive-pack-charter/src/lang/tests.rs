use super::ast::EntryKind;
use super::definition::{Route, WhenSource};
use super::registry::Registry;
use super::*;

const PR_REVIEW: &str = include_str!("../../tests/fixtures/pr_review.charter");
const PR_FIX: &str = include_str!("../../tests/fixtures/pr_fix.charter");
const PR_SPLIT: &str = include_str!("../../tests/fixtures/pr_split.charter");
const REVIEW_ROUND: &str = include_str!("../../tests/fixtures/review_round.charter");
const INBOX_TRIAGE: &str = include_str!("../../tests/fixtures/inbox_triage.charter");
const STALE_TASK_TRIAGE: &str = include_str!("../../tests/fixtures/stale_task_triage.charter");
const ISSUE_READONLY_INVESTIGATION: &str =
    include_str!("../../tests/fixtures/issue_readonly_investigation.charter");
const CODE_FIX_WITH_PROBE_TEST: &str =
    include_str!("../../tests/fixtures/code_fix_with_probe_test.charter");
const INBOX_SWEEP: &str = include_str!("../../tests/fixtures/inbox_sweep.charter");
const PR_MERGE_DECISION_HANDOFF: &str =
    include_str!("../../tests/fixtures/pr_merge_decision_handoff.charter");
const PR_REVIEW_VERDICT_ESCALATION: &str =
    include_str!("../../tests/fixtures/pr_review_verdict_escalation.charter");
const DIRECTIVE_RELAY: &str = include_str!("../../tests/fixtures/directive_relay.charter");
const PR_GATE_JOB_LAUNCH: &str = include_str!("../../tests/fixtures/pr_gate_job_launch.charter");
const REPLY_AFTER_VERIFICATION: &str =
    include_str!("../../tests/fixtures/reply_after_verification.charter");
const PR_MERGE_AFTER_CI_GATE: &str =
    include_str!("../../tests/fixtures/pr_merge_after_ci_gate.charter");
const PR_MERGE_VOCAB: &str = include_str!("../../tests/fixtures/pr_merge.vocab");

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
        registry.get("work.unit").expect("work.unit").passes,
        ["done", "skipped"]
    );
}

#[test]
fn producer_class_is_parsed_serialized_and_digest_pinned() {
    let vocabulary = |producer: &str| {
        format!(
            "vocabulary acme v1\nphrase acme.gate \"a gate\":\n    producer {producer}\n    ceiling advisory\n"
        )
    };
    let charter = format!(
        "{HEADER}workflow:\n    phase a:\n        require acme.gate advisory\n        complete\n"
    );
    let mut instrument_registry = Registry::core();
    instrument_registry
        .add(&parse_vocabulary_source(&vocabulary("instrument")).unwrap())
        .unwrap();
    let entry = instrument_registry.get("acme.gate").unwrap();
    assert_eq!(entry.to_json()["producer_class"], "instrument");

    let mut actor_registry = Registry::core();
    actor_registry
        .add(&parse_vocabulary_source(&vocabulary("actor")).unwrap())
        .unwrap();
    let instrument = compile(&charter, &instrument_registry).unwrap();
    let actor = compile(&charter, &actor_registry).unwrap();
    assert_ne!(instrument.gate_registry_digest, actor.gate_registry_digest);
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
fn the_workflow_fixtures_compile() {
    let registry = Registry::core();
    for source in [
        STALE_TASK_TRIAGE,
        ISSUE_READONLY_INVESTIGATION,
        CODE_FIX_WITH_PROBE_TEST,
        INBOX_SWEEP,
        PR_MERGE_DECISION_HANDOFF,
        PR_REVIEW_VERDICT_ESCALATION,
        DIRECTIVE_RELAY,
        PR_GATE_JOB_LAUNCH,
        REPLY_AFTER_VERIFICATION,
    ] {
        let compiled = compile(source, &registry).unwrap_or_else(|e| panic!("{e:?}\n{source}"));
        assert!(compiled.definition_digest.starts_with("blake3:"));
        assert_eq!(
            compiled.definition.gate_registry_digest,
            compiled.gate_registry_digest
        );
    }

    let mut registry = Registry::core();
    let vocabulary = parse_vocabulary_source(PR_MERGE_VOCAB).expect("pr_merge vocabulary");
    registry
        .add(&vocabulary)
        .expect("register pr_merge vocabulary");
    let compiled = compile(PR_MERGE_AFTER_CI_GATE, &registry)
        .unwrap_or_else(|e| panic!("{e:?}\n{PR_MERGE_AFTER_CI_GATE}"));
    assert!(compiled.definition_digest.starts_with("blake3:"));
    assert_eq!(
        compiled.definition.gate_registry_digest,
        compiled.gate_registry_digest
    );
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
fn gate_registry_digest_pins_every_entry_but_definition_keeps_referenced_entries() {
    let base = compile(PR_REVIEW, &Registry::core()).unwrap();
    let mut registry = Registry::core();
    registry
        .add(
            &parse_vocabulary_source(
                "vocabulary acme v1\nphrase acme.unrelated \"an unrelated observation\":\n    producer instrument\n",
            )
            .unwrap(),
        )
        .unwrap();
    let with_unrelated_entry = compile(PR_REVIEW, &registry).unwrap();

    assert_ne!(
        base.gate_registry_digest, with_unrelated_entry.gate_registry_digest,
        "the digest pins every registered entry"
    );
    assert_eq!(
        base.definition.registry, with_unrelated_entry.definition.registry,
        "the definition carries the entries it evaluates by"
    );
}

#[test]
fn a_vocabulary_cannot_take_a_name_another_vocabulary_holds() {
    let mut registry = Registry::core();
    let other = parse_vocabulary_source(
        "vocabulary acme v1\nphrase review.decision \"shadow\":\n    producer instrument\n",
    )
    .unwrap();
    let errors = registry.add(&other).unwrap_err();
    assert_eq!(errors[0].code, "compiled_entry");
    let fresh = parse_vocabulary_source(
        "vocabulary acme v1\nphrase acme.gate \"a deployment gate\":\n    producer instrument\n",
    )
    .unwrap();
    assert_eq!(registry.add(&fresh).unwrap().len(), 1);
    let source = format!(
        "{HEADER}workflow:\n    phase only:\n        require acme.gate advisory\n        complete\n"
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
        "vocabulary acme v1\nsubject widget \"a widget\":\n    producer system\n    new_generation acme.rebuilt \
         pull_request.head_moved\ntrigger acme.rebuilt \"the widget was rebuilt\":\n    producer platform\n",
    )
    .unwrap();
    registry
        .add(&accepted)
        .expect("own and core triggers both count");
    for (source, code) in [
        (
            "vocabulary acme v2\nsubject widget \"a widget\":\n    producer system\n    new_generation acme.nope\n",
            "unknown_trigger",
        ),
        (
            "vocabulary acme v2\nsubject widget \"a widget\":\n    producer system\n    new_generation review.decision\n",
            "unknown_trigger",
        ),
        (
            "vocabulary acme v2\ntrigger acme.rebuilt \"rebuilt\":\n    producer platform\n    new_generation \
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
            "workflow:\n    phase a:\n        await review.decision\n        complete\n",
            "wrong_entry_kind",
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
fn nested_grant_block_is_refused() {
    assert_refused(
        "workflow:\n    phase a:\n        grants:\n            author may work.unit\n                require no.such_phrase hard\n        complete\n",
        "unexpected_block",
    );
}

#[test]
fn overflowing_durations_are_refused_at_their_source_line() {
    let registry = Registry::core();
    for unit in ['m', 'h', 'd'] {
        let source = format!(
            "charter probe v1\nsubject message\nworkflow:\n    phase a:\n        require inbox.unread_older_than(age=9223372036854775807{unit}) advisory\n        complete\n"
        );
        let errors = compile(&source, &registry).expect_err("overflowing duration is refused");
        assert!(
            errors
                .iter()
                .any(|d| d.code == "duration_range" && d.line == 5),
            "expected a located duration_range diagnostic, got {errors:?}"
        );
    }

    let source = |duration: &str| {
        format!(
            "charter probe v1\nsubject message\nworkflow:\n    phase a:\n        require inbox.unread_older_than(age={duration}) advisory\n        complete\n"
        )
    };
    let seconds = compile(&source("60s"), &registry).unwrap();
    let minutes = compile(&source("1m"), &registry).unwrap();
    assert_eq!(seconds.definition_bytes, minutes.definition_bytes);
}

#[test]
fn exhausted_bound_redirects_cannot_hide_an_unbounded_cycle() {
    let source = format!(
        "{HEADER}workflow:\n    phase a:\n        when review.decision is approve: complete\n        max_rounds 1 then b\n        goto b\n    phase b:\n        await fix.pushed\n        goto a\n"
    );
    let errors = refused(&source);
    assert!(
        errors.iter().any(|d| d.code == "unbounded_cycle"),
        "{errors:?}"
    );
}

#[test]
fn exhausted_bounds_cannot_redirect_only_in_a_cycle() {
    let source = format!(
        "{HEADER}workflow:\n    phase a:\n        max_rounds 1 then b\n        goto b\n    phase b:\n        max_rounds 1 then a\n        goto a\n"
    );
    let errors = refused(&source);
    assert!(
        errors.iter().any(|d| d.code == "unbounded_cycle"),
        "{errors:?}"
    );
}

#[test]
fn closed_trigger_outcome_sets_remove_impossible_fallthrough() {
    let source = format!(
        "{HEADER}workflow:\n    phase a:\n        when review.posted is observed: complete\n    phase b:\n        complete\n"
    );
    let errors = refused(&source);
    assert!(
        errors.iter().any(|d| d.code == "unreachable_phase"),
        "{errors:?}"
    );
}

#[test]
fn closed_single_outcome_phrases_remove_impossible_fallthrough() {
    let mut registry = Registry::core();
    let vocabulary = parse_vocabulary_source(
        "vocabulary acme v1\nphrase acme.probe.one \"one outcome\":\n    producer instrument\n    outcomes pass\n    passes pass\n",
    )
    .unwrap();
    registry.add(&vocabulary).unwrap();
    let source = format!(
        "{HEADER}workflow:\n    phase a:\n        when acme.probe.one is pass: complete\n    phase b:\n        complete\n"
    );
    let errors = match compile(&source, &registry) {
        Ok(compiled) => panic!(
            "expected unreachable phase, compiled {:?}",
            compiled.summary()
        ),
        Err(errors) => errors,
    };
    assert!(
        errors.iter().any(|d| d.code == "unreachable_phase"),
        "{errors:?}"
    );
}

#[test]
fn covering_every_outcome_removes_default_fallthrough() {
    let source = format!(
        "{HEADER}workflow:\n    phase a:\n        when review.decision is approve: complete\n        when review.decision is request_changes: complete\n        when review.decision is blocked: complete\n    phase b:\n        complete\n"
    );
    let errors = refused(&source);
    assert!(
        errors.iter().any(|d| d.code == "unreachable_phase"),
        "{errors:?}"
    );
}

#[test]
fn shadowed_when_branches_are_not_topology_edges() {
    let source = format!(
        "{HEADER}workflow:\n    phase a:\n        when review.decision is approve: goto b\n        when review.decision is approve: goto c\n        goto b\n    phase b:\n        complete\n    phase c:\n        complete\n"
    );
    let errors = refused(&source);
    assert!(
        errors.iter().any(|d| d.code == "unreachable_phase"),
        "{errors:?}"
    );
}

#[test]
fn impossible_default_self_loop_does_not_refuse_an_acyclic_workflow() {
    let source = format!(
        "{HEADER}workflow:\n    phase a:\n        when review.posted is observed: goto b\n        goto a\n    phase b:\n        complete\n"
    );
    compile(&source, &Registry::core()).expect("the only reachable route ends");
}

#[test]
fn an_impossible_default_exit_does_not_satisfy_exit_reachability() {
    let source = format!(
        "{HEADER}workflow:\n    phase a:\n        when review.posted is observed: goto b\n        complete\n    phase b:\n        max_rounds 1 then a\n        goto a\n"
    );
    let errors = refused(&source);
    assert!(errors.iter().any(|d| d.code == "no_exit"), "{errors:?}");
}

#[test]
fn source_size_limit_has_a_named_diagnostic() {
    let source = "#".repeat(1_048_577);
    let errors = super::lex::lex(&source).expect_err("oversized source is refused");
    assert!(
        errors
            .iter()
            .any(|d| d.code == "source_too_large" && d.line == 1),
        "{errors:?}"
    );
}

#[test]
fn nesting_depth_limit_has_a_named_diagnostic() {
    let source: String = (0..=64)
        .map(|depth| format!("{}x\n", " ".repeat(depth)))
        .collect();
    let lines = super::lex::lex(&source).expect("short source lexes");
    let errors = super::lex::tree(lines).expect_err("excessive nesting is refused");
    assert!(
        errors
            .iter()
            .any(|d| d.code == "nesting_too_deep" && d.line == 65),
        "{errors:?}"
    );
}

#[test]
fn phase_count_limit_has_a_named_diagnostic() {
    let mut source = format!("{HEADER}workflow:\n");
    for i in 0..257 {
        source.push_str(&format!("    phase p{i}:\n        complete\n"));
    }
    let errors = refused(&source);
    assert!(
        errors.iter().any(|d| d.code == "too_many_phases"),
        "{errors:?}"
    );
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

#[test]
fn a_ceiling_change_changes_the_gate_registry_digest() {
    let base = compile(PR_REVIEW, &Registry::core()).unwrap();
    let from = "phrase review.within_words \"the verdict stays within a word budget\":\n    producer instrument\n    param words integer\n";
    assert!(super::registry::CORE_VOCABULARY.contains(from));
    let changed_source =
        super::registry::CORE_VOCABULARY.replace(from, &format!("{from}    ceiling hard\n"));
    let build = |core: &str| {
        let mut registry = Registry::default();
        registry
            .add_compiled(&parse_vocabulary_source(core).unwrap())
            .unwrap();
        registry
            .add_compiled(&parse_vocabulary_source(super::registry::PR_MERGE_VOCABULARY).unwrap())
            .unwrap();
        registry
    };
    let rebuilt = compile(PR_REVIEW, &build(super::registry::CORE_VOCABULARY)).unwrap();
    assert_eq!(base.gate_registry_digest, rebuilt.gate_registry_digest);
    let registry = build(&changed_source);
    assert_eq!(
        registry.get("review.within_words").unwrap().ceiling,
        super::ast::Level::Hard
    );
    let changed = compile(PR_REVIEW, &registry).unwrap();
    assert_ne!(base.gate_registry_digest, changed.gate_registry_digest);
}

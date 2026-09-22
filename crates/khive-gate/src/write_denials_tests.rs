use super::*;
use crate::{ActorRef, CLASSIFIED_OPERATIONS};
use khive_types::Namespace;
use serde_json::json;

fn request(actor: &str, verb: &str) -> GateRequest {
    GateRequest::new(
        ActorRef::new("actor", actor),
        Namespace::local(),
        verb,
        json!({}),
    )
}

#[test]
fn write_denials_allow_only_explicit_reads_after_enrollment() {
    let gate = CallerEnrollmentGate::with_write_denials(
        vec!["seat:duty".into(), "seat:writer".into()],
        false,
        vec!["*:duty".into()],
    );
    for &(verb, access) in CLASSIFIED_OPERATIONS {
        let decision = gate.check(&request("seat:duty", verb)).unwrap();
        assert_eq!(
            decision.is_allow(),
            access == OperationAccess::Read,
            "{verb}"
        );
        if access == OperationAccess::Write {
            assert!(
                matches!(decision, GateDecision::Deny { reason } if reason.contains("deny_writes_for"))
            );
        }
        assert!(gate
            .check(&request("seat:writer", verb))
            .unwrap()
            .is_allow());
    }
    for verb in [
        "unloaded.verb",
        "mount.remote_read",
        "authorize_with_visibility",
        "COMM.INBOX",
    ] {
        assert!(!gate.check(&request("seat:duty", verb)).unwrap().is_allow());
        assert!(gate
            .check(&request("seat:writer", verb))
            .unwrap()
            .is_allow());
    }
    assert!(matches!(gate.check(&request("other", "get")).unwrap(),
        GateDecision::Deny { reason } if reason == "actor is not enrolled"));
}

#[test]
fn write_denial_patterns_are_anchored_literal_except_star() {
    for (pattern, actor, matches) in [
        ("*:duty", "seat:child:duty", true),
        ("*:duty", "seat:duty:child", false),
        ("seat:duty", "seat:duty:child", false),
        ("seat:duty", "Seat:duty", false),
        ("seat*", "seat", true),
        ("a*b*c", "axbybc", true),
        ("a*b*c", "axbybd", false),
        ("**", "anything", true),
        ("*", "", true),
        ("é*值", "é中间值", true),
        ("é*值", "e中间值", false),
        ("[duty]?@host/path\\x", "[duty]?@host/path\\x", true),
        ("[duty]?", "dutyX", false),
        (" actor ", "actor", false),
    ] {
        assert_eq!(
            actor_matches(pattern, actor),
            matches,
            "{pattern:?} vs {actor:?}"
        );
    }
    let actor = "用户@host/值";
    let gate =
        CallerEnrollmentGate::with_write_denials(vec![actor.into()], false, vec!["用户@*".into()]);
    assert!(gate.check(&request(actor, "get")).unwrap().is_allow());
    assert!(!gate.check(&request(actor, "create")).unwrap().is_allow());
}

#[test]
fn write_denials_match_effective_id_not_argument_labels_and_preserve_anonymous_enrollment() {
    let gate =
        CallerEnrollmentGate::with_write_denials(vec!["local".into()], false, vec!["local".into()]);
    let mut req = request("local", "create");
    req.args = json!({"actor":"unrestricted", "process_ref":"writer", "namespace":"writer"});
    assert!(!gate.check(&req).unwrap().is_allow());
    req.verb = "get".into();
    assert!(gate.check(&req).unwrap().is_allow());
    req.actor = ActorRef::anonymous();
    assert!(
        matches!(gate.check(&req).unwrap(), GateDecision::Deny { reason }
        if reason == "unattributed caller is not enrolled")
    );
    let anonymous = CallerEnrollmentGate::with_write_denials(vec![], true, vec!["local".into()]);
    assert!(anonymous.check(&req).unwrap().is_allow());
    req.verb = "create".into();
    assert!(!anonymous.check(&req).unwrap().is_allow());
}

#[test]
fn invalid_programmatic_write_policy_fails_every_caller_closed() {
    for patterns in [
        vec![String::new()],
        vec![" \t".into()],
        vec!["é".repeat(129)],
        vec!["*".into(); 257],
    ] {
        assert!(CallerEnrollmentGate::validate_write_denials(&patterns).is_err());
        let gate = CallerEnrollmentGate::with_write_denials(vec!["writer".into()], true, patterns);
        for actor in ["writer", "unlisted"] {
            for verb in ["get", "create", "unknown"] {
                assert!(matches!(
                    gate.check(&request(actor, verb)),
                    Err(GateError::Policy(_))
                ));
            }
        }
        let mut anonymous = request("writer", "get");
        anonymous.actor = ActorRef::anonymous();
        assert!(matches!(gate.check(&anonymous), Err(GateError::Policy(_))));
    }
    assert!(CallerEnrollmentGate::validate_write_denials(&vec!["é".repeat(128); 256]).is_ok());
}

#[test]
fn write_policy_fingerprint_preserves_legacy_and_tracks_patterns_validity_and_classifier() {
    let base = CallerEnrollmentGate::new(vec!["writer".into()], false);
    let empty = CallerEnrollmentGate::with_write_denials(vec!["writer".into()], false, vec![]);
    assert_eq!(
        base.configuration_fingerprint(),
        empty.configuration_fingerprint()
    );
    let first = CallerEnrollmentGate::with_write_denials(
        vec!["writer".into()],
        false,
        vec!["*:duty".into(), "writer".into(), "writer".into()],
    );
    let reordered = CallerEnrollmentGate::with_write_denials(
        vec!["writer".into()],
        false,
        vec!["writer".into(), "*:duty".into()],
    );
    assert_eq!(
        first.configuration_fingerprint(),
        reordered.configuration_fingerprint()
    );
    assert_ne!(
        first.configuration_fingerprint(),
        base.configuration_fingerprint()
    );
    let changed =
        CallerEnrollmentGate::with_write_denials(vec!["writer".into()], false, vec!["*".into()]);
    assert_ne!(
        first.configuration_fingerprint(),
        changed.configuration_fingerprint()
    );
    let invalid = CallerEnrollmentGate::with_write_denials(
        vec!["writer".into()],
        false,
        vec!["*".into(); 257],
    );
    assert_ne!(
        invalid.configuration_fingerprint(),
        changed.configuration_fingerprint()
    );
    let expected = write_policy_fingerprint(
        base.configuration_fingerprint().unwrap(),
        OPERATION_CLASSIFIER_VERSION,
        false,
        &first.deny_writes_for,
    );
    assert_eq!(first.configuration_fingerprint(), Some(expected.as_str()));
    assert_ne!(
        expected,
        write_policy_fingerprint(
            base.configuration_fingerprint().unwrap(),
            "different-reviewed-contract",
            false,
            &first.deny_writes_for
        )
    );
}

#[test]
fn operation_classification_is_sorted_explicit_and_not_inferred_from_names() {
    assert!(CLASSIFIED_OPERATIONS
        .windows(2)
        .all(|pair| pair[0].0 < pair[1].0));
    for (verb, expected) in [
        ("authorize", OperationAccess::Write),
        ("authorize.visible", OperationAccess::Read),
        ("comm.read", OperationAccess::Write),
        ("comm.mark_read", OperationAccess::Write),
        ("telemetry.emit", OperationAccess::Write),
        ("git.diff", OperationAccess::Write),
        ("brain.record_serve", OperationAccess::Write),
        ("memory.recall", OperationAccess::Read),
        ("session.resume", OperationAccess::Read),
        ("agent.resume", OperationAccess::Write),
    ] {
        assert_eq!(classify_operation(verb), Some(expected));
    }
    assert_eq!(
        classify_operation("brain.emit"),
        classify_operation("brain.feedback")
    );
    assert_eq!(classify_operation("new_pack.read"), None);
}

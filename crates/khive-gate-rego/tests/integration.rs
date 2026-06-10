use std::path::PathBuf;
use std::sync::Arc;

use khive_gate::{ActorRef, Gate, GateContext, GateDecision, GateRef, GateRequest, Obligation};
use khive_gate_rego::{RegoGate, DEFAULT_ENTRYPOINT};
use khive_types::Namespace;
use serde_json::json;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn request(verb: &str) -> GateRequest {
    GateRequest::new(ActorRef::anonymous(), Namespace::local(), verb, json!({}))
}

#[test]
fn inline_policy_allows_search() {
    let policy = r#"
        package khive.gate
        import rego.v1
        default decision := {"decision": "deny", "reason": "default"}
        decision := {"decision": "allow", "obligations": []} if {
            input.verb == "search"
        }
    "#;
    let gate = RegoGate::from_policy_str(policy).expect("policy compiles");
    let decision = gate.check(&request("search")).expect("eval");
    assert!(decision.is_allow(), "expected allow, got {decision:?}");
}

#[test]
fn inline_policy_denies_non_match_with_default_reason() {
    let policy = r#"
        package khive.gate
        import rego.v1
        default decision := {"decision": "deny", "reason": "default"}
        decision := {"decision": "allow", "obligations": []} if {
            input.verb == "search"
        }
    "#;
    let gate = RegoGate::from_policy_str(policy).expect("policy compiles");
    let decision = gate.check(&request("create")).expect("eval");
    match decision {
        GateDecision::Deny { reason } => assert_eq!(reason, "default"),
        other => panic!("expected Deny, got {other:?}"),
    }
}

#[test]
fn from_dir_loads_single_file() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    // from_dir loads ALL .rego files; the two fixtures use the same package
    // path. Using a one-policy temp dir to keep this test scoped.
    let tmp = tempdir_with_file(
        "allow_search.rego",
        &std::fs::read_to_string(fixture("allow_search.rego")).unwrap(),
    );
    let _ = dir; // keep clippy quiet about the parent constant
    let gate = RegoGate::from_dir(tmp.path()).expect("load dir");
    assert!(gate.check(&request("search")).unwrap().is_allow());
    assert!(matches!(
        gate.check(&request("delete")).unwrap(),
        GateDecision::Deny { .. }
    ));
}

#[test]
fn namespace_scoped_policy_emits_audit_obligation() {
    let source = std::fs::read_to_string(fixture("namespace_scoped.rego")).unwrap();
    let gate = RegoGate::from_policy_str(&source).expect("compiles");

    let mut req = GateRequest::new(
        ActorRef::new("user", "ocean"),
        Namespace::local(),
        "search",
        json!({}),
    );
    req.context = GateContext {
        session_id: Some("sess-1".into()),
        ..GateContext::default()
    };

    let decision = gate.check(&req).expect("eval");
    match decision {
        GateDecision::Allow { obligations } => {
            assert_eq!(obligations.len(), 1, "expected 1 obligation");
            match &obligations[0] {
                Obligation::Audit { tag } => assert_eq!(tag, "verb.search"),
                other => panic!("expected Audit, got {other:?}"),
            }
        }
        other => panic!("expected Allow, got {other:?}"),
    }
}

#[test]
fn namespace_scoped_policy_denies_anonymous_create_with_reason() {
    let source = std::fs::read_to_string(fixture("namespace_scoped.rego")).unwrap();
    let gate = RegoGate::from_policy_str(&source).expect("compiles");

    let decision = gate.check(&request("create")).expect("eval");
    match decision {
        GateDecision::Deny { reason } => {
            assert_eq!(reason, "anonymous callers cannot write")
        }
        other => panic!("expected Deny, got {other:?}"),
    }
}

#[test]
fn malformed_policy_returns_policy_error() {
    let err = RegoGate::from_policy_str("package khive.gate\ndefault decision := {")
        .expect_err("should fail to compile");
    let msg = format!("{err}");
    assert!(msg.contains("policy error"), "wrong error variant: {msg}");
}

#[test]
fn missing_entrypoint_returns_deny_not_error() {
    // Compiles fine but has no `decision` rule — the default entrypoint
    // data.khive.gate.decision will be absent.  check() must return
    // Ok(Deny) rather than Err so the runtime's fail-open Err branch is
    // never reached.
    let policy = r#"
        package khive.gate
        import rego.v1
        verdict := "allow"
    "#;
    let gate = RegoGate::from_policy_str(policy).expect("compiles");
    let result = gate.check(&request("search"));
    match result {
        Ok(GateDecision::Deny { .. }) => {}
        Ok(GateDecision::Allow { .. }) => panic!("expected Deny, got Allow"),
        Err(e) => panic!("expected Ok(Deny), got Err({e})"),
    }
}

#[test]
fn try_with_entrypoint_rejects_rule_absent_from_policy() {
    // Policy defines `verdict`, not `decision`.  Requesting `decision` as
    // the entrypoint must fail at construction, not at first check().
    let policy = r#"
        package khive.gate
        import rego.v1
        default verdict := {"decision": "deny", "reason": "default"}
    "#;
    let gate = RegoGate::from_policy_str(policy).unwrap();
    let err = gate
        .try_with_entrypoint("data.khive.gate.decision")
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("not a valid rule") || msg.contains("policy"),
        "expected policy-error for absent rule, got: {msg}"
    );
}

#[test]
fn try_with_entrypoint_accepts_present_rule() {
    let policy = r#"
        package khive.custom
        import rego.v1
        default decision := {"decision": "deny", "reason": "default"}
        decision := {"decision": "allow", "obligations": []} if {
            input.verb == "search"
        }
    "#;
    let gate = RegoGate::from_policy_str(policy)
        .unwrap()
        .try_with_entrypoint("data.khive.custom.decision")
        .expect("rule exists — must be accepted");
    assert!(gate.check(&request("search")).unwrap().is_allow());
}

#[test]
fn try_with_entrypoint_rejects_malformed_data_dot_only() {
    let policy = r#"
        package khive.gate
        import rego.v1
        default decision := {"decision": "deny", "reason": "default"}
    "#;
    let gate = RegoGate::from_policy_str(policy).unwrap();
    let err = gate.try_with_entrypoint("data.").unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("empty path segment"),
        "wrong error for 'data.': {msg}"
    );
}

#[test]
fn try_with_entrypoint_rejects_consecutive_dots() {
    let policy = r#"
        package khive.gate
        import rego.v1
        default decision := {"decision": "deny", "reason": "default"}
    "#;
    let gate = RegoGate::from_policy_str(policy).unwrap();
    let err = gate
        .try_with_entrypoint("data.khive..gate.decision")
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("empty path segment"),
        "wrong error for consecutive dots: {msg}"
    );
}

#[test]
fn try_with_entrypoint_rejects_trailing_dot() {
    let policy = r#"
        package khive.gate
        import rego.v1
        default decision := {"decision": "deny", "reason": "default"}
    "#;
    let gate = RegoGate::from_policy_str(policy).unwrap();
    let err = gate
        .try_with_entrypoint("data.khive.gate.decision.")
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("empty path segment"),
        "wrong error for trailing dot: {msg}"
    );
}

#[test]
fn undefined_rule_result_returns_deny() {
    // A rule that exists but has no default and no matching branch returns
    // Value::Undefined from eval_rule.  check() must return Ok(Deny).
    let policy = r#"
        package khive.gate
        import rego.v1
        decision := {"decision": "allow", "obligations": []} if {
            input.verb == "search"
        }
    "#;
    let gate = RegoGate::from_policy_str(policy)
        .unwrap()
        .try_with_entrypoint("data.khive.gate.decision")
        .expect("rule exists");
    // verb != "search" → no branch matches → undefined
    let result = gate.check(&request("delete")).unwrap();
    assert!(
        matches!(result, GateDecision::Deny { .. }),
        "expected Deny for undefined result, got {result:?}"
    );
}

#[test]
fn wrong_shape_result_returns_deny() {
    // A rule that returns a boolean instead of a GateDecision object.
    // check() must return Ok(Deny) rather than Err.
    let policy = r#"
        package khive.gate
        import rego.v1
        default decision := true
    "#;
    let gate = RegoGate::from_policy_str(policy)
        .unwrap()
        .try_with_entrypoint("data.khive.gate.decision")
        .expect("rule exists");
    let result = gate.check(&request("search")).unwrap();
    assert!(
        matches!(result, GateDecision::Deny { .. }),
        "expected Deny for wrong-shape result, got {result:?}"
    );
}

#[test]
fn impl_name_reports_rego() {
    let policy = r#"
        package khive.gate
        import rego.v1
        default decision := {"decision": "deny", "reason": "x"}
    "#;
    let gate: GateRef = Arc::new(RegoGate::from_policy_str(policy).unwrap());
    assert_eq!(gate.impl_name(), "RegoGate");
}

#[test]
fn custom_entrypoint_is_respected() {
    let policy = r#"
        package khive.custom
        import rego.v1
        default verdict := {"decision": "deny", "reason": "custom default"}
        verdict := {"decision": "allow", "obligations": []} if {
            input.verb == "search"
        }
    "#;
    let gate = RegoGate::from_policy_str(policy)
        .unwrap()
        .with_entrypoint("data.khive.custom.verdict");
    assert!(gate.check(&request("search")).unwrap().is_allow());
    assert!(matches!(
        gate.check(&request("delete")).unwrap(),
        GateDecision::Deny { .. }
    ));
}

#[test]
fn default_entrypoint_constant_matches_doc() {
    assert_eq!(DEFAULT_ENTRYPOINT, "data.khive.gate.decision");
}

// ---- GATE-REGO-001: entrypoint trim/validation ----

#[test]
fn try_with_entrypoint_rejects_empty() {
    let policy = r#"
        package khive.gate
        import rego.v1
        default decision := {"decision": "deny", "reason": "default"}
    "#;
    let gate = RegoGate::from_policy_str(policy).unwrap();
    let err = gate.try_with_entrypoint("").unwrap_err();
    assert!(
        format!("{err}").contains("empty or whitespace"),
        "wrong error: {err}"
    );
}

#[test]
fn try_with_entrypoint_rejects_whitespace_only() {
    let policy = r#"
        package khive.gate
        import rego.v1
        default decision := {"decision": "deny", "reason": "default"}
    "#;
    let gate = RegoGate::from_policy_str(policy).unwrap();
    let err = gate.try_with_entrypoint("   ").unwrap_err();
    assert!(
        format!("{err}").contains("empty or whitespace"),
        "wrong error: {err}"
    );
}

#[test]
fn try_with_entrypoint_trims_whitespace_and_works() {
    // The entrypoint has surrounding whitespace. After trim it must resolve
    // to a valid data. path and the gate must evaluate correctly.
    let policy = r#"
        package khive.custom
        import rego.v1
        default verdict := {"decision": "deny", "reason": "custom default"}
        verdict := {"decision": "allow", "obligations": []} if {
            input.verb == "search"
        }
    "#;
    let gate = RegoGate::from_policy_str(policy)
        .unwrap()
        .try_with_entrypoint("  data.khive.custom.verdict  ")
        .expect("padded but valid entrypoint must be accepted");
    // Debug output must show the trimmed form, not the padded original.
    assert!(
        format!("{gate:?}").contains("data.khive.custom.verdict"),
        "Debug output did not contain trimmed entrypoint: {:?}",
        gate
    );
    // And the gate must resolve correctly.
    assert!(gate.check(&request("search")).unwrap().is_allow());
    assert!(matches!(
        gate.check(&request("delete")).unwrap(),
        GateDecision::Deny { .. }
    ));
}

#[test]
fn try_with_entrypoint_rejects_missing_data_prefix() {
    let policy = r#"
        package khive.gate
        import rego.v1
        default decision := {"decision": "deny", "reason": "default"}
    "#;
    let gate = RegoGate::from_policy_str(policy).unwrap();
    let err = gate.try_with_entrypoint("khive.gate.decision").unwrap_err();
    assert!(format!("{err}").contains("data."), "wrong error: {err}");
}

// ---- GATE-REGO-002: with_entrypoint trims whitespace ----

#[test]
fn with_entrypoint_trims_whitespace_and_allows() {
    // with_entrypoint (infallible) must trim surrounding whitespace so that
    // "  data.khive.gate  " behaves identically to "data.khive.gate.decision"
    // when used with a matching policy.
    let policy = r#"
        package khive.gate
        import rego.v1
        default decision := {"decision": "deny", "reason": "default"}
        decision := {"decision": "allow", "obligations": []} if {
            input.verb == "search"
        }
    "#;
    let gate = RegoGate::from_policy_str(policy)
        .unwrap()
        .with_entrypoint("  data.khive.gate.decision  ");
    // The stored entrypoint must be the trimmed form.
    assert!(
        format!("{gate:?}").contains("data.khive.gate.decision"),
        "Debug output did not contain trimmed entrypoint: {:?}",
        gate
    );
    // Evaluation must work correctly with the trimmed entrypoint.
    assert!(gate.check(&request("search")).unwrap().is_allow());
    assert!(matches!(
        gate.check(&request("delete")).unwrap(),
        GateDecision::Deny { .. }
    ));
}

// ---------- helpers ----------

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn path(&self) -> &PathBuf {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn tempdir_with_file(name: &str, contents: &str) -> TempDir {
    let base = std::env::temp_dir().join(format!("khive-gate-rego-test-{}", std::process::id()));
    // Each call gets a unique subdir so parallel tests don't collide.
    let suffix: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as u64;
    let dir = base.join(format!("{suffix}-{name}"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), contents).unwrap();
    TempDir { path: dir }
}

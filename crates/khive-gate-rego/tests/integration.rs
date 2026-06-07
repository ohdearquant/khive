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
fn missing_entrypoint_surfaces_evaluation_error() {
    // Compiles fine but has no `decision` rule.
    let policy = r#"
        package khive.gate
        import rego.v1
        verdict := "allow"
    "#;
    let gate = RegoGate::from_policy_str(policy).expect("compiles");
    let result = gate.check(&request("search"));
    // regorus returns null for a missing rule; that becomes "not a GateDecision"
    // → evaluation error.
    match result {
        Err(e) => {
            let msg = format!("{e}");
            assert!(msg.contains("evaluation"), "wrong error variant: {msg}");
        }
        Ok(d) => panic!("expected evaluation error, got Ok({d:?})"),
    }
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

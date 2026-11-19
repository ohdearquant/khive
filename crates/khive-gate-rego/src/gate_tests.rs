use super::*;
use khive_gate::ActorRef;
use khive_types::Namespace;
use serde_json::json;

fn request(verb: &str) -> GateRequest {
    GateRequest::new(ActorRef::anonymous(), Namespace::local(), verb, json!({}))
}

// ---- GATE-REGO-003: poisoned mutex → Deny, not Err ----

#[test]
fn poisoned_engine_mutex_returns_deny_not_err() {
    use std::sync::Arc;

    let policy = r#"
            package khive.gate
            import rego.v1
            default decision := {"decision": "allow", "obligations": []}
        "#;
    let gate = Arc::new(RegoGate::from_policy_str(policy).expect("policy compiles"));

    // Poison the mutex by panicking inside a spawned thread while holding
    // the lock.  After join() the mutex is permanently poisoned.
    {
        let gate_clone = Arc::clone(&gate);
        let handle = std::thread::spawn(move || {
            let _guard = gate_clone.engine.lock().unwrap();
            panic!("intentional panic to poison the mutex");
        });
        // Join to ensure the panic has propagated and the mutex is poisoned.
        let _ = handle.join();
    }

    // check() must return Ok(Deny), never Err.
    let result = gate.check(&request("search"));
    match result {
        Ok(GateDecision::Deny { .. }) => {}
        Ok(GateDecision::Allow { .. }) => panic!("expected Deny for poisoned mutex, got Allow"),
        Err(e) => panic!("expected Ok(Deny) for poisoned mutex, got Err({e})"),
    }
}

// ---- GATEREGO-AUD-001: wrong-shaped policy result must never echo caller args ----

#[test]
fn malformed_policy_echoing_input_args_does_not_leak_secret() {
    // A misconfigured/malformed policy that echoes `input.args` back as the
    // decision object, instead of a proper `{"decision": "allow"|"deny", ...}`
    // shape. This is exactly the GATEREGO-AUD-001 failure mode: the policy
    // author's mistake must not become a secret-exfiltration channel.
    let policy = r#"
            package khive.gate
            import rego.v1
            default decision := input.args
        "#;
    let gate = RegoGate::from_policy_str(policy).expect("policy compiles");

    // FAKE key: real AKIA/AWS shape, invented suffix (see khive-runtime's
    // secret_gate tests for the convention).
    let fake_key = "AKIAFAKEKEY000000000";
    let req = GateRequest::new(
        ActorRef::anonymous(),
        Namespace::local(),
        "propose",
        json!({
            "changeset": {
                "entity": {
                    "properties": {
                        "api_key": fake_key,
                    }
                }
            }
        }),
    );

    // `default decision := input.args` is rejected by regorus at eval time
    // ("invalid ref in default value"), not policy-authored deny — this is
    // the evaluator-failure path and fails closed via `Ok(GateDecision::Deny)`
    // carrying a static classified reason, never the evaluator's raw error
    // text or the caller-supplied input it echoes.
    let decision = gate
        .check(&req)
        .expect("evaluator failure must be Ok(Deny) (fail-closed)");
    let reason = match decision {
        GateDecision::Deny { reason } => reason,
        GateDecision::Allow { .. } => panic!("evaluator failure must deny, not allow"),
    };

    assert!(
        !reason.contains(fake_key),
        "deny reason must never echo the caller-supplied secret; got: {reason}"
    );
    assert!(
        !reason.contains("api_key"),
        "deny reason must never echo caller-supplied field names either; got: {reason}"
    );
    assert_eq!(
        reason, "policy evaluation failed",
        "evaluator failure must classify with the static evaluator-failure reason"
    );
}

// ---- GATEREGO-AUD-002: the shape-mismatch warn log must never leak the
// caller-supplied value either, even though the Deny reason is sanitized ----

#[derive(Clone, Default)]
struct CapturedLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
    type Writer = CapturedLog;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[test]
fn malformed_policy_shape_mismatch_log_does_not_leak_secret() {
    // Unlike the `default decision := input.args` shape in the sibling
    // test above (which regorus rejects at eval time as an "invalid ref
    // in default value" before any value is produced), this policy
    // evaluates successfully and returns a well-formed object whose
    // `decision` tag value is the caller-supplied secret itself, which
    // is exactly what drives serde's internally-tagged "unknown variant"
    // error message through the deserialize path this test targets.
    let policy = r#"
            package khive.gate
            import rego.v1
            decision := {"decision": input.args.changeset.entity.properties.api_key}
        "#;
    let gate = RegoGate::from_policy_str(policy).expect("policy compiles");

    let fake_key = "AKIAFAKEKEY000000000";
    let req = GateRequest::new(
        ActorRef::anonymous(),
        Namespace::local(),
        "propose",
        json!({
            "changeset": {
                "entity": {
                    "properties": {
                        "api_key": fake_key,
                    }
                }
            }
        }),
    );

    let captured = CapturedLog::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_ansi(false)
        .finish();

    let decision = tracing::subscriber::with_default(subscriber, || {
        gate.check(&req).expect("check must not Err (fail-closed)")
    });
    assert!(
        matches!(decision, GateDecision::Deny { .. }),
        "wrong-shaped policy result must deny, not allow"
    );

    let log_output = String::from_utf8(captured.0.lock().unwrap().clone())
        .expect("log output must be valid UTF-8");

    assert!(
        !log_output.is_empty(),
        "expected the shape-mismatch warn log to be captured"
    );
    assert!(
        log_output.contains("policy_decision_shape_mismatch"),
        "expected the shape-mismatch branch's fixed error category in the log; got: {log_output}"
    );
    assert!(
        !log_output.contains(fake_key),
        "tracing output must never contain the caller-supplied secret; got: {log_output}"
    );
    assert!(
        !log_output.contains("api_key"),
        "tracing output must never contain caller-supplied field names either; got: {log_output}"
    );
}

// ---- distinguishability: evaluator failure vs decision-serialization
// failure must classify into different static deny reasons ----

#[test]
fn evaluator_failure_and_serialization_failure_classify_differently() {
    let evaluator_failure_policy = r#"
            package khive.gate
            import rego.v1
            default decision := input.args
        "#;
    let evaluator_failure_gate =
        RegoGate::from_policy_str(evaluator_failure_policy).expect("policy compiles");
    let evaluator_decision = evaluator_failure_gate
        .check(&request("search"))
        .expect("evaluator failure must be Ok(Deny) (fail-closed)");
    let evaluator_reason = match evaluator_decision {
        GateDecision::Deny { reason } => reason,
        GateDecision::Allow { .. } => panic!("evaluator failure must deny, not allow"),
    };

    // A float product large enough that regorus's arbitrary-precision
    // number can no longer be reformatted into a JSON-parseable literal —
    // forces the decision-serialization branch without needing the
    // evaluator itself to fail.
    let serialization_failure_policy = r#"
            package khive.gate
            import rego.v1
            decision := 1.0e300 * 1.0e300
        "#;
    let serialization_failure_gate =
        RegoGate::from_policy_str(serialization_failure_policy).expect("policy compiles");
    let serialization_decision = serialization_failure_gate
        .check(&request("search"))
        .expect("unserializable decision value must be Ok(Deny) (fail-closed)");
    let serialization_reason = match serialization_decision {
        GateDecision::Deny { reason } => reason,
        GateDecision::Allow { .. } => panic!("unserializable decision must deny, not allow"),
    };

    assert_eq!(evaluator_reason, "policy evaluation failed");
    assert_eq!(
        serialization_reason,
        "policy produced an unserializable decision"
    );
    assert_ne!(
        evaluator_reason, serialization_reason,
        "evaluator failure must stay distinguishable from a decision-serialization failure"
    );
}

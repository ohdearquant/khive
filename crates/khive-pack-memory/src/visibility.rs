//! Validation for the memory session-visibility fence at the recall boundary.

use std::collections::HashSet;
use std::fmt;

use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_types::{Details, KhiveError};
use serde_json::Value;

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ModelFence {
    pub(crate) model: String,
    pub(crate) ann_write_log_seq: u64,
}

impl fmt::Debug for ModelFence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ModelFence([REDACTED])")
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct VisibilityFence {
    pub(crate) namespace: String,
    pub(crate) fences: Vec<ModelFence>,
}

impl fmt::Debug for VisibilityFence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("VisibilityFence([REDACTED])")
    }
}

impl VisibilityFence {
    pub(crate) fn seq_for_model(&self, model: &str) -> Option<u64> {
        self.fences
            .iter()
            .find(|fence| fence.model == model)
            .map(|fence| fence.ann_write_log_seq)
    }

    pub(crate) fn proof_for_model(&self, model: &str) -> Option<(u64, String)> {
        self.seq_for_model(model)
            .map(|seq| (seq, self.namespace.clone()))
    }
}

fn invalid(message: &str) -> RuntimeError {
    RuntimeError::InvalidInput(format!("memory.recall {message}"))
}

/// Eventual reads ignore an optional token. Session fences are copied only
/// after runtime custody authenticates scope, requested models and token age.
pub(crate) fn parse_recall_visibility(
    runtime: &KhiveRuntime,
    consistency: Option<&Value>,
    visibility_token: Option<&Value>,
    visible_namespaces: &[&str],
    requested_models: &[String],
) -> Result<Option<VisibilityFence>, RuntimeError> {
    match consistency {
        None => return Ok(None),
        Some(Value::String(value)) if value == "eventual" => return Ok(None),
        Some(Value::String(value)) if value == "session" => {}
        _ => return Err(invalid("consistency must be 'eventual' or 'session'")),
    }

    if visibility_token
        .and_then(Value::as_object)
        .and_then(|object| object.get("version"))
        .and_then(Value::as_u64)
        == Some(1)
    {
        return Err(KhiveError::invalid_input(
            "memory.recall clear visibility receipts are not accepted",
        )
        .with_details(Details::new([("reason", "visibility_token_legacy")]))
        .into());
    }
    let Some(token) = visibility_token.and_then(Value::as_str) else {
        // A write made without receipt custody returns a null token. Handing
        // that null back must name the retryable custody refusal, not a
        // malformed token; only with custody in place is the shape at fault.
        runtime.ensure_visibility_receipt_key()?;
        return Err(invalid(
            "consistency=session requires an opaque visibility_token string",
        ));
    };
    let receipt = runtime.open_visibility_receipt(token, visible_namespaces, requested_models)?;
    let mut seen = HashSet::new();
    let mut fences = Vec::new();
    for model in requested_models {
        if seen.insert(model.as_str()) {
            if let Some(ann_write_log_seq) = receipt.sequence_for_model(model) {
                fences.push(ModelFence {
                    model: model.clone(),
                    ann_write_log_seq,
                });
            }
        }
    }
    Ok(Some(VisibilityFence {
        namespace: receipt.namespace().to_owned(),
        fences,
    }))
}

/// Validate the caller's wait budget; the caller clips it further to the
/// remaining request deadline minus its two-second margin.
pub(crate) fn parse_timeout_ms(raw: Option<&Value>) -> Result<u64, RuntimeError> {
    match raw {
        None => Ok(0),
        Some(value) => match value.as_u64() {
            Some(ms) if ms <= 10_000 => Ok(ms),
            _ => Err(invalid(
                "timeout_ms must be an integer from 0 through 10000",
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_types::ErrorKind;
    use serde_json::json;

    fn runtime() -> KhiveRuntime {
        crate::test_support::with_receipt_credentials(
            KhiveRuntime::memory().expect("in-memory runtime"),
        )
    }

    fn models() -> Vec<String> {
        vec!["model-a".into(), "model-b".into()]
    }

    fn seal(runtime: &KhiveRuntime, namespace: &str, fences: &[(String, u64)]) -> Value {
        json!(runtime
            .seal_visibility_receipt(namespace, fences)
            .expect("seal authenticated fixture"))
    }

    fn parse(
        runtime: &KhiveRuntime,
        consistency: Option<&Value>,
        token: Option<&Value>,
    ) -> Result<Option<VisibilityFence>, RuntimeError> {
        parse_recall_visibility(runtime, consistency, token, &["local"], &models())
    }

    fn assert_invalid(result: Result<Option<VisibilityFence>, RuntimeError>) {
        assert!(
            matches!(&result, Err(RuntimeError::InvalidInput(_)))
                || matches!(
                    &result,
                    Err(RuntimeError::Khive(error)) if error.kind() == ErrorKind::InvalidInput
                ),
            "expected InvalidInput, got {result:?}"
        );
    }

    #[test]
    fn eventual_is_the_default_and_does_not_consume_an_optional_token() {
        let runtime = KhiveRuntime::memory().expect("runtime without receipt custody");
        assert_eq!(parse(&runtime, None, None).unwrap(), None);
        assert_eq!(
            parse(
                &runtime,
                Some(&json!("eventual")),
                Some(&json!({"bad": true}))
            )
            .unwrap(),
            None
        );
        for invalid_mode in [json!("linearizable"), json!(null), json!(1), json!({})] {
            assert_invalid(parse(&runtime, Some(&invalid_mode), None));
        }
    }

    #[test]
    fn session_requires_a_strict_versioned_namespace_bound_receipt() {
        let runtime = runtime();
        let mode = json!("session");
        assert_invalid(parse(&runtime, Some(&mode), None));
        for invalid_token in [
            json!(null),
            json!([]),
            json!({"version": 1, "namespace": "local"}),
            json!({"version": 2, "namespace": "local", "fences": []}),
            json!({"version": "1", "namespace": "local", "fences": []}),
            json!({"version": 1, "namespace": "other", "fences": []}),
            json!({"version": 1, "namespace": "local", "fences": [], "extra": true}),
            json!({"version": 1, "namespace": "local", "fences": {}}),
        ] {
            assert_invalid(parse(&runtime, Some(&mode), Some(&invalid_token)));
        }
        let legacy = json!({"version": 1, "namespace": "local", "fences": []});
        let error = parse(&runtime, Some(&mode), Some(&legacy)).unwrap_err();
        assert!(matches!(error, RuntimeError::Khive(error)
            if error.kind() == ErrorKind::InvalidInput
                && error.details().and_then(|details| details.get("reason"))
                    == Some("visibility_token_legacy")));
        let mut token = seal(&runtime, "local", &[])
            .as_str()
            .unwrap()
            .as_bytes()
            .to_vec();
        let last = token.last_mut().expect("nonempty token");
        *last = if *last == b'A' { b'B' } else { b'A' };
        let token = json!(String::from_utf8(token).unwrap());
        assert_invalid(parse(&runtime, Some(&mode), Some(&token)));
    }

    #[test]
    fn session_without_custody_refuses_a_missing_token_as_key_unavailable() {
        let runtime = KhiveRuntime::memory().expect("runtime without receipt custody");
        let mode = json!("session");
        for token in [None, Some(json!(null)), Some(json!([]))] {
            let error = parse(&runtime, Some(&mode), token.as_ref()).unwrap_err();
            assert!(
                matches!(&error, RuntimeError::Khive(error)
                    if error.kind() == ErrorKind::Unavailable
                        && error.details().and_then(|details| details.get("reason"))
                            == Some("visibility_key_unavailable")),
                "expected the custody refusal for {token:?}, got {error:?}"
            );
        }
        let legacy = json!({"version": 1, "namespace": "local", "fences": []});
        let error = parse(&runtime, Some(&mode), Some(&legacy)).unwrap_err();
        assert!(matches!(error, RuntimeError::Khive(error)
            if error.details().and_then(|details| details.get("reason"))
                == Some("visibility_token_legacy")));
    }

    #[test]
    fn session_accepts_a_visible_actor_namespace_but_not_a_foreign_namespace() {
        let runtime = runtime();
        let mode = json!("session");
        let receipt = seal(&runtime, "actor-a", &[]);
        let accepted = parse_recall_visibility(
            &runtime,
            Some(&mode),
            Some(&receipt),
            &["local", "actor-a"],
            &models(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(accepted.namespace, "actor-a");
        assert_invalid(parse_recall_visibility(
            &runtime,
            Some(&mode),
            Some(&receipt),
            &["local"],
            &models(),
        ));
    }

    #[test]
    fn session_rejects_duplicate_foreign_and_out_of_set_fences() {
        let runtime = runtime();
        let mode = json!("session");
        assert!(runtime
            .seal_visibility_receipt("local", &[("model-a".into(), 7), ("model-a".into(), 8)])
            .is_err());
        assert!(runtime
            .seal_visibility_receipt("local", &[("model-a".into(), 0)])
            .is_err());
        let foreign = seal(&runtime, "other", &[("model-a".into(), 7)]);
        assert_invalid(parse(&runtime, Some(&mode), Some(&foreign)));
        let outside = seal(&runtime, "local", &[("model-c".into(), 7)]);
        assert_invalid(parse(&runtime, Some(&mode), Some(&outside)));
        let model_b = seal(&runtime, "local", &[("model-b".into(), 9)]);
        assert_invalid(parse_recall_visibility(
            &runtime,
            Some(&mode),
            Some(&model_b),
            &["local"],
            &["model-a".into()],
        ));
    }

    #[test]
    fn empty_fences_succeed_and_future_sequences_remain_for_runtime_proof() {
        let runtime = runtime();
        let mode = json!("session");
        let empty = seal(&runtime, "local", &[]);
        let fence = parse(&runtime, Some(&mode), Some(&empty)).unwrap().unwrap();
        assert!(fence.fences.is_empty());
        assert_eq!(fence.namespace, "local");
        assert_eq!(fence.seq_for_model("model-a"), None);
        let future = seal(
            &runtime,
            "local",
            &[("model-b".into(), u64::MAX), ("model-a".into(), 7)],
        );
        let fence = parse(&runtime, Some(&mode), Some(&future))
            .unwrap()
            .unwrap();
        assert_eq!(fence.seq_for_model("model-a"), Some(7));
        assert_eq!(fence.seq_for_model("model-b"), Some(u64::MAX));
        assert_eq!(fence.seq_for_model("model-c"), None);
        assert_eq!(fence.namespace, "local");
    }

    #[test]
    fn timeout_defaults_to_one_proof_and_enforces_the_server_cap() {
        assert_eq!(parse_timeout_ms(None).unwrap(), 0);
        assert_eq!(parse_timeout_ms(Some(&json!(0))).unwrap(), 0);
        assert_eq!(parse_timeout_ms(Some(&json!(10_000))).unwrap(), 10_000);
        for invalid_timeout in [
            json!(-1),
            json!(10_001),
            json!(1.5),
            json!("10"),
            json!(null),
        ] {
            assert!(matches!(
                parse_timeout_ms(Some(&invalid_timeout)),
                Err(RuntimeError::InvalidInput(_))
            ));
        }
    }

    #[test]
    fn visibility_fence_debug_never_discloses_authenticated_fields() {
        let fence = VisibilityFence {
            namespace: "private-namespace-sentinel".into(),
            fences: vec![ModelFence {
                model: "private-model-sentinel".into(),
                ann_write_log_seq: 987654321,
            }],
        };
        let rendered = format!("{fence:?} {:?}", fence.fences[0]);
        assert!(rendered.contains("REDACTED"));
        for hidden in [
            "private-namespace-sentinel",
            "private-model-sentinel",
            "987654321",
        ] {
            assert!(!rendered.contains(hidden));
        }
    }
}

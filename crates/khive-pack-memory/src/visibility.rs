//! Validation for the memory session-visibility fence at the recall boundary.

use std::collections::HashSet;

use khive_runtime::RuntimeError;
use serde_json::Value;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ModelFence {
    pub(crate) model: String,
    pub(crate) ann_write_log_seq: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct VisibilityFence {
    pub(crate) namespace: String,
    pub(crate) fences: Vec<ModelFence>,
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

/// Parse the strict v1 receipt only when the caller asks for session consistency.
/// Eventual reads have no fence obligation, even if a caller also sends a token.
pub(crate) fn parse_recall_visibility(
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

    let token = visibility_token
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("consistency=session requires a visibility_token object"))?;
    if token.len() != 3 || token.get("version").and_then(Value::as_u64) != Some(1) {
        return Err(invalid(
            "visibility_token has an invalid version, namespace, or shape",
        ));
    }
    let namespace = token
        .get("namespace")
        .and_then(Value::as_str)
        .filter(|namespace| visible_namespaces.contains(namespace))
        .ok_or_else(|| invalid("visibility_token namespace is not caller-visible"))?;
    let entries = token
        .get("fences")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("visibility_token.fences must be an array"))?;
    let requested: HashSet<&str> = requested_models.iter().map(String::as_str).collect();
    let mut seen = HashSet::with_capacity(entries.len());
    let mut fences = Vec::with_capacity(entries.len());
    for entry in entries {
        let object = entry
            .as_object()
            .ok_or_else(|| invalid("visibility_token fence must be an object"))?;
        if object.len() != 2 {
            return Err(invalid("visibility_token fence has an invalid shape"));
        }
        let model = object
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.is_empty())
            .ok_or_else(|| invalid("visibility_token fence model must be nonempty text"))?;
        if !requested.contains(model) {
            return Err(invalid(
                "visibility_token fence model is outside the requested models",
            ));
        }
        if !seen.insert(model) {
            return Err(invalid("visibility_token contains a duplicate model fence"));
        }
        let ann_write_log_seq = object
            .get("ann_write_log_seq")
            .and_then(Value::as_u64)
            .filter(|seq| *seq > 0)
            .ok_or_else(|| invalid("visibility_token fence sequence must be a positive integer"))?;
        fences.push(ModelFence {
            model: model.to_owned(),
            ann_write_log_seq,
        });
    }
    Ok(Some(VisibilityFence {
        namespace: namespace.to_owned(),
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
    use serde_json::json;

    fn models() -> Vec<String> {
        vec!["model-a".into(), "model-b".into()]
    }

    fn parse(
        consistency: Option<&Value>,
        token: Option<&Value>,
    ) -> Result<Option<VisibilityFence>, RuntimeError> {
        parse_recall_visibility(consistency, token, &["local"], &models())
    }

    fn assert_invalid(result: Result<Option<VisibilityFence>, RuntimeError>) {
        assert!(
            matches!(&result, Err(RuntimeError::InvalidInput(_))),
            "expected InvalidInput, got {result:?}"
        );
    }

    #[test]
    fn eventual_is_the_default_and_does_not_consume_an_optional_token() {
        assert_eq!(parse(None, None).unwrap(), None);
        assert_eq!(
            parse(Some(&json!("eventual")), Some(&json!({"bad": true}))).unwrap(),
            None
        );
        for invalid_mode in [json!("linearizable"), json!(null), json!(1), json!({})] {
            assert_invalid(parse(Some(&invalid_mode), None));
        }
    }

    #[test]
    fn session_requires_a_strict_versioned_namespace_bound_receipt() {
        let mode = json!("session");
        assert_invalid(parse(Some(&mode), None));
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
            assert_invalid(parse(Some(&mode), Some(&invalid_token)));
        }
    }

    #[test]
    fn session_accepts_a_visible_actor_namespace_but_not_a_foreign_namespace() {
        let mode = json!("session");
        let receipt = json!({"version": 1, "namespace": "actor-a", "fences": []});
        let accepted = parse_recall_visibility(
            Some(&mode),
            Some(&receipt),
            &["local", "actor-a"],
            &models(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(accepted.namespace, "actor-a");
        assert_invalid(parse_recall_visibility(
            Some(&mode),
            Some(&receipt),
            &["local"],
            &models(),
        ));
    }

    #[test]
    fn session_rejects_duplicate_foreign_and_out_of_set_fences() {
        let mode = json!("session");
        let valid = json!({"model": "model-a", "ann_write_log_seq": 7});
        for invalid_fence in [
            json!({"model": "model-c", "ann_write_log_seq": 7}),
            json!({"model": "", "ann_write_log_seq": 7}),
            json!({"model": "model-a", "ann_write_log_seq": 0}),
            json!({"model": "model-a", "ann_write_log_seq": -1}),
            json!({"model": "model-a", "ann_write_log_seq": "7"}),
            json!({"model": "model-a", "ann_write_log_seq": 7, "extra": true}),
            json!(["model-a", 7]),
        ] {
            let token = json!({"version": 1, "namespace": "local", "fences": [invalid_fence]});
            assert_invalid(parse(Some(&mode), Some(&token)));
        }
        let duplicate =
            json!({"version": 1, "namespace": "local", "fences": [valid.clone(), valid]});
        assert_invalid(parse(Some(&mode), Some(&duplicate)));
        let token = json!({"version": 1, "namespace": "local", "fences": [{"model": "model-b", "ann_write_log_seq": 9}]});
        assert_invalid(parse_recall_visibility(
            Some(&mode),
            Some(&token),
            &["local"],
            &["model-a".into()],
        ));
    }

    #[test]
    fn empty_fences_succeed_and_future_sequences_remain_for_runtime_proof() {
        let mode = json!("session");
        let empty = json!({"version": 1, "namespace": "local", "fences": []});
        let fence = parse(Some(&mode), Some(&empty)).unwrap().unwrap();
        assert!(fence.fences.is_empty());
        assert_eq!(fence.seq_for_model("model-a"), None);

        let future = json!({"version": 1, "namespace": "local", "fences": [
            {"model": "model-b", "ann_write_log_seq": u64::MAX},
            {"model": "model-a", "ann_write_log_seq": 7}
        ]});
        let fence = parse(Some(&mode), Some(&future)).unwrap().unwrap();
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
        for invalid_value in [
            json!(10_001),
            json!(-1),
            json!(1.5),
            json!("10"),
            json!(null),
        ] {
            assert!(matches!(
                parse_timeout_ms(Some(&invalid_value)),
                Err(RuntimeError::InvalidInput(_))
            ));
        }
    }
}

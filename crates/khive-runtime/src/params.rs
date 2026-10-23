//! Shared deserialization of a verb's JSON parameters.
//!
//! Every pack that decodes its parameters into a typed struct reports a malformed call the same
//! way, so the text a caller sees for a bad argument does not depend on which pack handled it.

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::secret_gate::{mask_for_redaction_surface, RedactionSurface};
use crate::RuntimeError;

/// Deserialize a verb's JSON `params` into `T`.
///
/// A failure becomes [`RuntimeError::InvalidInput`] whose text is `bad params: ` followed by the
/// message serde reports with recognized secrets masked, for example a missing required field
/// or a field rejected by `deny_unknown_fields`. Ordinary diagnostics remain unchanged.
pub fn deser_params<T: DeserializeOwned>(params: Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params).map_err(|e| {
        RuntimeError::InvalidInput(format!(
            "bad params: {}",
            mask_for_redaction_surface(RedactionSurface::GateProbe, &e.to_string())
        ))
    })
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use serde_json::json;

    use crate::{deser_params, RuntimeError};

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Sample {
        name: String,
        #[serde(default)]
        count: u32,
    }

    /// Ordinary serde diagnostics must still pass through byte for byte.
    fn serde_message(params: &serde_json::Value) -> String {
        serde_json::from_value::<Sample>(params.clone())
            .expect_err("these params must not deserialize")
            .to_string()
    }

    #[test]
    fn missing_required_field_gives_bad_params_invalid_input() {
        let params = json!({"count": 3});
        let want = serde_message(&params);
        assert!(want.contains("missing field `name`"), "{want}");

        let err = deser_params::<Sample>(params).unwrap_err();
        let RuntimeError::InvalidInput(message) = err else {
            panic!("expected InvalidInput, got {err:?}");
        };
        assert!(message.starts_with("bad params: "), "{message}");
        assert_eq!(message, format!("bad params: {want}"));
    }

    #[test]
    fn unknown_field_under_deny_unknown_fields_gives_bad_params_invalid_input() {
        let params = json!({"name": "alpha", "extra": true});
        let want = serde_message(&params);
        assert!(want.contains("unknown field `extra`"), "{want}");

        let err = deser_params::<Sample>(params).unwrap_err();
        let RuntimeError::InvalidInput(message) = err else {
            panic!("expected InvalidInput, got {err:?}");
        };
        assert!(message.starts_with("bad params: "), "{message}");
        assert_eq!(message, format!("bad params: {want}"));
    }

    #[test]
    fn valid_params_deserialize_into_the_requested_type() {
        let params = json!({"name": "alpha", "count": 3});
        let parsed: Sample = deser_params(params).unwrap();
        let expected = Sample {
            name: "alpha".to_string(),
            count: 3,
        };
        assert_eq!(parsed, expected);

        let params = json!({"name": "beta"});
        let defaulted: Sample = deser_params(params).unwrap();
        let expected_default = Sample {
            name: "beta".to_string(),
            count: 0,
        };
        assert_eq!(defaulted, expected_default);
    }

    fn assert_masked_decode_failure<T: serde::de::DeserializeOwned + std::fmt::Debug>(
        params: serde_json::Value,
        raw: &str,
    ) {
        let original = serde_json::from_value::<T>(params.clone())
            .unwrap_err()
            .to_string();
        assert!(
            original.contains(raw),
            "fixture must exercise an echoed operand"
        );
        let RuntimeError::InvalidInput(message) = deser_params::<T>(params).unwrap_err() else {
            panic!("parameter refusal changed its error variant");
        };
        if raw.starts_with("sk-proj-") {
            assert!(
                !message.contains(raw),
                "decoder exposed a recognized credential"
            );
            assert!(message.contains("***MASKED***"), "decoder omitted the mask");
        }
        assert_eq!(
            message,
            format!(
                "bad params: {}",
                crate::secret_gate::mask_for_redaction_surface(
                    crate::secret_gate::RedactionSurface::GateProbe,
                    &original
                )
            )
        );
    }

    #[test]
    fn wrong_type_diagnostics_mask_recognized_credentials() {
        for raw in ["bogus".to_string(), format!("sk-proj-{}", "A".repeat(80))] {
            assert_masked_decode_failure::<Sample>(json!({"name":"alpha", "count":raw}), &raw);
        }
    }

    #[test]
    fn unknown_field_diagnostics_mask_recognized_credentials() {
        for raw in ["extra".to_string(), format!("sk-proj-{}", "A".repeat(80))] {
            let mut params = json!({"name":"alpha"});
            params
                .as_object_mut()
                .unwrap()
                .insert(raw.clone(), json!(true));
            assert_masked_decode_failure::<Sample>(params, &raw);
        }
    }

    #[test]
    fn nested_enum_diagnostics_mask_recognized_credentials() {
        #[derive(Debug, Deserialize, PartialEq)]
        enum Mode {
            Read,
            Write,
        }
        #[derive(Debug, Deserialize, PartialEq)]
        struct Nested {
            modes: Vec<Mode>,
        }
        for raw in ["bogus".to_string(), format!("sk-proj-{}", "A".repeat(80))] {
            assert_masked_decode_failure::<Nested>(json!({"modes":[raw]}), &raw);
        }
        assert_eq!(
            deser_params::<Nested>(json!({"modes":["Read", "Write"]})).unwrap(),
            Nested {
                modes: vec![Mode::Read, Mode::Write]
            }
        );
    }
}

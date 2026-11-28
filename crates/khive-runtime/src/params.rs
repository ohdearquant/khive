//! Shared deserialization of a verb's JSON parameters.
//!
//! Every pack that decodes its parameters into a typed struct reports a malformed call the same
//! way, so the text a caller sees for a bad argument does not depend on which pack handled it.

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::secret_gate::{mask_for_redaction_surface, RedactionSurface};
use crate::{ParamDef, RuntimeError};

/// Largest magnitude at which every integer has its own `f64` (2^53).
const EXACT_FLOAT_INTEGER_LIMIT: f64 = 9_007_199_254_740_992.0;

/// Rewrite each top-level argument that `defs` declares `integer` and that arrives as a
/// whole-number float (`5.0`) to that integer, so a client that writes every JSON number with a
/// decimal point decodes the same as one that does not. A fractional, non-finite or inexact float
/// stays as sent and the decoder still refuses it; nested values and other types are untouched.
pub fn integer_params_from_whole_floats(params: &mut Value, defs: &[ParamDef]) {
    let Value::Object(map) = params else {
        return;
    };
    for def in defs.iter().filter(|def| def.param_type == "integer") {
        if let Some(value) = map.get_mut(def.name) {
            if let Some(integer) = whole_float_as_integer(value) {
                *value = integer;
            }
        }
    }
}

fn whole_float_as_integer(value: &Value) -> Option<Value> {
    let Value::Number(number) = value else {
        return None;
    };
    if number.is_i64() || number.is_u64() {
        return None;
    }
    let float = number.as_f64()?;
    if !float.is_finite() || float.fract() != 0.0 || float.abs() > EXACT_FLOAT_INTEGER_LIMIT {
        return None;
    }
    // The bound above makes both casts exact; `-0.0` takes the non-negative arm and becomes 0.
    Some(if float >= 0.0 {
        Value::from(float as u64)
    } else {
        Value::from(float as i64)
    })
}

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

    fn integer_defs() -> [crate::ParamDef; 3] {
        let def = |name, param_type| crate::ParamDef {
            name,
            param_type,
            required: false,
            description: "",
            resolution_mode: crate::IdResolutionMode::NotApplicable,
        };
        [
            def("count", "integer"),
            def("offset", "integer"),
            def("weight", "number"),
        ]
    }

    #[test]
    fn a_whole_float_for_an_integer_param_decodes_as_that_integer() {
        let mut params = json!({"name": "alpha", "count": 5.0});
        super::integer_params_from_whole_floats(&mut params, &integer_defs());
        assert_eq!(params, json!({"name": "alpha", "count": 5}));
        let parsed: Sample = deser_params(params).unwrap();
        assert_eq!(parsed.count, 5);
    }

    #[test]
    fn a_fractional_float_for_an_integer_param_is_still_refused() {
        let mut params = json!({"name": "alpha", "count": 5.5});
        super::integer_params_from_whole_floats(&mut params, &integer_defs());
        assert_eq!(params, json!({"name": "alpha", "count": 5.5}));
        let RuntimeError::InvalidInput(message) = deser_params::<Sample>(params).unwrap_err()
        else {
            panic!("expected InvalidInput");
        };
        assert!(
            message.contains("floating point") && message.contains("5.5"),
            "{message}"
        );
    }

    #[test]
    fn only_exact_whole_floats_of_declared_integer_params_change() {
        let mut params = json!({
            "count": -3.0,
            "offset": -0.0,
            "weight": 2.0,
            "undeclared": 4.0,
            "nested": {"count": 6.0},
            "list": [7.0],
        });
        super::integer_params_from_whole_floats(&mut params, &integer_defs());
        assert_eq!(
            params,
            json!({
                "count": -3,
                "offset": 0,
                "weight": 2.0,
                "undeclared": 4.0,
                "nested": {"count": 6.0},
                "list": [7.0],
            })
        );
        assert!(params["offset"].is_u64(), "{params}");

        for inexact in [9_007_199_254_740_994.0_f64, 1e300, -1e300] {
            let mut params = json!({ "count": inexact });
            super::integer_params_from_whole_floats(&mut params, &integer_defs());
            assert!(
                params["count"].is_f64(),
                "{inexact} must stay a float: {params}"
            );
        }
        let mut params = json!({"count": 9_007_199_254_740_992.0_f64});
        super::integer_params_from_whole_floats(&mut params, &integer_defs());
        assert_eq!(params["count"], json!(9_007_199_254_740_992_u64));

        let mut integers = json!({"count": 5, "offset": -2});
        super::integer_params_from_whole_floats(&mut integers, &integer_defs());
        assert_eq!(integers, json!({"count": 5, "offset": -2}));

        let mut not_an_object = json!([5.0]);
        super::integer_params_from_whole_floats(&mut not_an_object, &integer_defs());
        assert_eq!(not_an_object, json!([5.0]));
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

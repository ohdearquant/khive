//! Shared deserialization of a verb's JSON parameters.
//!
//! Every pack that decodes its parameters into a typed struct reports a malformed call the same
//! way, so the text a caller sees for a bad argument does not depend on which pack handled it.

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::RuntimeError;

/// Deserialize a verb's JSON `params` into `T`.
///
/// A failure becomes [`RuntimeError::InvalidInput`] whose text is `bad params: ` followed by the
/// message serde reports, for example a missing required field or a field rejected by
/// `deny_unknown_fields`.
pub fn deser_params<T: DeserializeOwned>(params: Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("bad params: {e}")))
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

    /// The message serde itself reports for `params`, which `deser_params` must carry through.
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
}

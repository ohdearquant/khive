//! Derive a JSON Schema for a verb's parameters from its own [`ParamDef`] list.
//!
//! `help` publishes `params[].type` as a documentation string. Every machine
//! bridge parses it as a JSON Schema type, and several of the spellings in use
//! are not schema types: `uuid` names a format, `array` names no element type,
//! and `object or array of object` is a union. A driver that cannot read a
//! verb's parameter types cannot call it, and only the git pack publishes a
//! hand-authored `input_schema` to read instead.
//!
//! The duplicate spellings this module used to absorb are gone: one type is
//! written one way, and the registry-wide test in `kkernel` asserts the exact
//! set. The map below therefore has one arm per spelling, and adding an
//! alternation to an arm is the shape to refuse, because it lets a second
//! spelling of an existing type back in without anything failing.
//!
//! This module closes that by deriving a schema from the declarations the
//! runtime already holds. A pack-supplied `input_schema` still wins, so the
//! hand-authored ones are untouched; every other verb gains a derived one.
//! `params[].type` is left exactly as it is: it is documentation, and `uuid`
//! carries information a bare schema type does not.

use serde_json::{json, Value};

use khive_types::ParamDef;

/// Parameters every verb accepts without declaring them, and which therefore
/// must not be rejected by a derived schema.
///
/// `help` short-circuits any dispatch to the verb's own description, and
/// `namespace` is resolved for every verb by the dispatch path whether or not
/// the verb lists it. A schema stricter than the dispatcher turns a working
/// call into a driver-side refusal, which is the same defect this module exists
/// to fix pointed the other way, so `additionalProperties` stays open and these
/// two are declared rather than merely tolerated.
const UNIVERSAL_PARAMS: &[(&str, &str, &str)] = &[
    (
        "help",
        "boolean",
        "Return this verb's description instead of dispatching it. No side effects.",
    ),
    (
        "namespace",
        "string",
        "Attribution namespace for this call. Defaults to the caller's resolved namespace.",
    ),
];

/// Map one declared `param_type` spelling onto a JSON Schema fragment.
///
/// Total over the spellings in use, and deliberately without a wildcard arm: an
/// unknown spelling returns `None` so the registry-wide test can name it and
/// fail. A default arm here would silently give every future spelling whatever
/// the fallback happened to be, which recreates the defect one verb at a time.
pub fn json_schema_type(param_type: &str) -> Option<Value> {
    let schema = match param_type {
        "string" => json!({"type": "string"}),
        "integer" => json!({"type": "integer"}),
        "number" => json!({"type": "number"}),
        "boolean" => json!({"type": "boolean"}),
        "object" => json!({"type": "object"}),
        "array" => json!({"type": "array"}),
        "uuid" => json!({"type": "string", "format": "uuid"}),
        "array of string" => {
            json!({"type": "array", "items": {"type": "string"}})
        }
        "array of object" => {
            json!({"type": "array", "items": {"type": "object"}})
        }
        "array of uuid" => {
            json!({"type": "array", "items": {"type": "string", "format": "uuid"}})
        }
        "object or array of object" => json!({
            "oneOf": [{"type": "object"}, {"type": "array", "items": {"type": "object"}}]
        }),
        "string | array<string>" => json!({
            "oneOf": [{"type": "string"}, {"type": "array", "items": {"type": "string"}}]
        }),
        "string|null" => json!({"type": ["string", "null"]}),
        // A parameter documented as accepting any JSON. The empty schema says
        // exactly that, rather than picking one type and being wrong.
        "JSON value" => json!({}),
        _ => return None,
    };
    Some(schema)
}

/// Build the `input_schema` object for one verb's parameter list.
///
/// Returns `None` when any declared spelling has no mapping, so a verb is never
/// published with a schema that quietly omits one of its parameters. A partial
/// schema is worse than none: a driver would read it as complete.
pub fn derive_input_schema(params: &[ParamDef], described: &[(String, String)]) -> Option<Value> {
    let mut properties = serde_json::Map::new();
    let mut required: Vec<Value> = Vec::new();

    for (index, param) in params.iter().enumerate() {
        let mut schema = json_schema_type(param.param_type)?;
        let description = described
            .get(index)
            .map(|(_, d)| d.clone())
            .unwrap_or_else(|| param.description.to_string());
        if let Value::Object(ref mut map) = schema {
            map.insert("description".to_string(), json!(description));
        }
        properties.insert(param.name.to_string(), schema);
        if param.required {
            required.push(json!(param.name));
        }
    }

    for (name, spelling, description) in UNIVERSAL_PARAMS {
        if properties.contains_key(*name) {
            continue;
        }
        let mut schema = json_schema_type(spelling)?;
        if let Value::Object(ref mut map) = schema {
            map.insert("description".to_string(), json!(description));
        }
        properties.insert((*name).to_string(), schema);
    }

    Some(json!({
        "type": "object",
        "properties": Value::Object(properties),
        "required": Value::Array(required),
        "additionalProperties": true,
    }))
}

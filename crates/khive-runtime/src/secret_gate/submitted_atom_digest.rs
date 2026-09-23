//! A digest-only projection of a rejected atom; never an admission or preview surface.

use khive_types::Hash32;
use serde_json::Value;

use super::{
    extend_across_invisible_bridge, scan_from_with_trigger, EntropyScanContext,
    MAX_MASK_SCAN_WORK_BYTES,
};

const UNSCANNED: &str = "<unscanned>";
const ATOM_FIELDS: &[&str] = &[
    "slug",
    "name",
    "content",
    "tags",
    "properties",
    "source_uri",
    "source_type",
    "finalized",
];

/// Hash the canonical detector-only projection of a submitted persisted candidate.
///
/// The caller supplies the whole normalized candidate, including fields retained by
/// patch semantics. Its fixed-schema root remains a JSON object with sorted keys.
/// Every nested object (including objects nested in properties arrays) is encoded
/// as an array of `[masked_key, masked_value]` pairs, sorted by each pair's compact
/// canonical JSON bytes. Equal pairs are retained, so masking two different keys
/// never drops a field or retains secret-dependent key ordering. Arrays retain
/// their order; numbers, booleans and null are unchanged. A non-schema root uses
/// the same pair representation rather than bypassing masking of unknown keys.
///
/// Each object key and string leaf is scanned independently, as with `check_json`.
/// Every detected span becomes `<secret:DETECTOR>` without credential characters,
/// prefix, length or trigger. Unscanned suffixes become the fixed `<unscanned>`
/// token. Scanner work is divided equally among all keys and string leaves before
/// scanning, so iteration over raw keys cannot influence another field's budget.
///
/// The only output is `blake3:` followed by the full 64 lowercase hex digits of
/// BLAKE3 over that compact JSON. The masked projection is transient, never returned.
/// This function cannot fail admission, consume an exemption, or change existing
/// redaction surfaces. This encoding is `masked_submitted_atom_v1`.
pub fn masked_submitted_atom_digest_v1(candidate: &Value) -> String {
    let quota = MAX_MASK_SCAN_WORK_BYTES / string_units(candidate).max(1);
    let canonical = project_candidate(candidate, quota).to_string();
    format!("blake3:{}", Hash32::from_blake3(canonical.as_bytes()))
}

/// Count structural scan units, never their secret-dependent lengths. Even an
/// empty string or a key which needs no masking keeps its own share of work.
fn string_units(value: &Value) -> usize {
    match value {
        Value::String(_) => 1,
        Value::Array(items) => items.iter().fold(0usize, |total, item| {
            total.saturating_add(string_units(item))
        }),
        Value::Object(fields) => fields.values().fold(fields.len(), |total, value| {
            total.saturating_add(string_units(value))
        }),
        _ => 0,
    }
}

fn project_candidate(candidate: &Value, quota: usize) -> Value {
    match candidate {
        Value::Object(fields) if fields.keys().all(|key| ATOM_FIELDS.contains(&key.as_str())) => {
            let mut projected: Vec<_> = fields
                .iter()
                // Root labels are fixed schema, not caller property keys.
                .map(|(key, value)| (key.clone(), project(value, quota)))
                .collect();
            projected.sort_by(|left, right| left.0.cmp(&right.0));
            // Sorted insertion remains canonical if another workspace member
            // enables serde_json's optional preserve_order feature.
            Value::Object(projected.into_iter().collect())
        }
        _ => project(candidate, quota),
    }
}

fn project(value: &Value, quota: usize) -> Value {
    match value {
        Value::String(text) => Value::String(mask_string(text, quota)),
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| project(item, quota)).collect())
        }
        Value::Object(fields) => {
            let mut pairs: Vec<_> = fields
                .iter()
                .map(|(key, value)| {
                    let pair = Value::Array(vec![
                        Value::String(mask_string(key, quota)),
                        project(value, quota),
                    ]);
                    (pair.to_string().into_bytes(), pair)
                })
                .collect();
            // Include the masked value in the comparison: colliding masked
            // keys must not preserve their original raw-key ordering.
            pairs.sort_by(|left, right| left.0.cmp(&right.0));
            Value::Array(pairs.into_iter().map(|(_, pair)| pair).collect())
        }
        _ => value.clone(),
    }
}

/// Per-string work: context tokenization plus every resumed detector sweep.
/// No partial sweep is attempted, so an unknown suffix is always wholly masked.
struct ScanBudget(usize);

impl ScanBudget {
    fn charge(&mut self, amount: usize) -> bool {
        match self.0.checked_sub(amount) {
            Some(remaining) => {
                self.0 = remaining;
                true
            }
            None => false,
        }
    }
}

fn mask_string(text: &str, quota: usize) -> String {
    let mut budget = ScanBudget(quota);
    if !budget.charge(text.len()) {
        return UNSCANNED.to_owned();
    }
    let context = EntropyScanContext::new(text);
    let base = text.as_ptr() as usize;
    let mut from = 0;
    let mut masked = String::new();
    while from < text.len() {
        // Resuming inside a token revisits its prefix while deriving members.
        // Keep the full original context so an earlier mask cannot erase the
        // only trigger governing a later credential.
        let token_index = context
            .tokens
            .partition_point(|&(offset, raw)| offset + raw.len() <= from);
        let scan_start = context
            .tokens
            .get(token_index)
            .map_or(from, |&(offset, _)| offset.min(from));
        if !budget.charge(text.len() - scan_start) {
            masked.push_str(UNSCANNED);
            break;
        }
        let Some((matched, detector, _trigger)) = scan_from_with_trigger(text, from, &context)
        else {
            masked.push_str(&text[from..]);
            break;
        };
        let start = matched.as_ptr() as usize - base;
        // Preserve structural trailing punctuation exactly as the existing
        // masker does, while consuming an entire invisible bridge as ONE span.
        // Splitting a bridge into multiple markers would retain its shape.
        let core_len = matched
            .trim_end_matches(['"', '\'', '`', '}', ']', ')', ',', ';'])
            .len();
        let end = extend_across_invisible_bridge(text, start + core_len.max(1));
        masked.push_str(&text[from..start]);
        masked.push_str("<secret:");
        masked.push_str(detector);
        masked.push('>');
        from = end;
    }
    masked
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn github_key(fill: char, length: usize) -> String {
        format!("ghp_{}", fill.to_string().repeat(length))
    }

    fn projected(value: &Value) -> Value {
        project_candidate(value, MAX_MASK_SCAN_WORK_BYTES / string_units(value).max(1))
    }

    fn digest(value: &Value) -> String {
        masked_submitted_atom_digest_v1(value)
    }

    #[test]
    fn issue2995_digest_is_full_blake3_of_canonical_root_and_property_pairs() {
        let input = json!({"tags": ["left", "right"], "properties": {"y": "ordinary", "b": false}});
        let expected = br#"{"properties":[["b",false],["y","ordinary"]],"tags":["left","right"]}"#;
        let actual = digest(&input);
        assert_eq!(actual, format!("blake3:{}", Hash32::from_blake3(expected)));
        assert_eq!(actual.len(), "blake3:".len() + 64);
        assert!(actual[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
        assert_eq!(
            digest(
                &json!({"properties": {"b": false, "y": "ordinary"}, "tags": ["left", "right"]})
            ),
            actual
        );
    }

    #[test]
    fn issue2995_digest_masks_every_atom_string_surface_without_secret_lengths() {
        let atom = |key: &str| {
            json!({
                "slug": key,
                "name": format!("label {key}"),
                "content": format!("first {key} then {key} end"),
                "tags": ["ordinary", key],
                "properties": {"nested": [{key: key}, null, true, 42]},
                "source_uri": format!("https://user:{key}@example.test/path"),
                "source_type": key,
                "finalized": false
            })
        };
        let first = atom(&github_key('A', 36));
        let second = atom(&github_key('B', 84));
        assert_eq!(digest(&first), digest(&second));
        let view = projected(&first);
        assert_eq!(view["slug"], "<secret:github-token>");
        assert_eq!(
            view["content"],
            "first <secret:github-token> then <secret:github-token> end"
        );
        assert_eq!(view["source_uri"], "<secret:url-userinfo>");
        assert_eq!(
            view["properties"],
            json!([[
                "nested",
                [
                    [["<secret:github-token>", "<secret:github-token>"]],
                    null,
                    true,
                    42
                ]
            ]])
        );
        assert_eq!(view["finalized"], false);
    }

    #[test]
    fn issue2995_digest_changes_for_unmasked_data_structure_and_detector() {
        let first = json!({"content": github_key('A', 36), "tags": ["left", "right"]});
        for changed in [
            json!({"content": github_key('A', 36), "tags": ["right", "left"]}),
            json!({"content": format!("extra {}", github_key('A', 36)), "tags": ["left", "right"]}),
            json!({"content": "https://user:synthetic@example.test", "tags": ["left", "right"]}),
            json!({"content": github_key('A', 36), "tags": ["left", "right"], "finalized": false}),
        ] {
            assert_ne!(digest(&first), digest(&changed));
        }
    }

    #[test]
    fn issue2995_digest_retains_duplicate_pairs_and_sorts_by_masked_pair_bytes() {
        let first = github_key('A', 36);
        let second = github_key('B', 48);
        let input = json!({"properties": {"aaa": true, first.clone(): "z", second.clone(): "a"}});
        assert_eq!(
            projected(&input)["properties"],
            json!([
                ["<secret:github-token>", "a"],
                ["<secret:github-token>", "z"],
                ["aaa", true]
            ])
        );
        // Reverse which raw secret sorts first: only masked pair order matters.
        assert_eq!(
            digest(&input),
            digest(&json!({"properties": {"aaa": true, first.clone(): "a", second.clone(): "z"}}))
        );
        let duplicates =
            json!({"properties": {first: "same", second: "same", "<secret:github-token>": "same"}});
        assert_eq!(
            projected(&duplicates)["properties"],
            json!([
                ["<secret:github-token>", "same"],
                ["<secret:github-token>", "same"],
                ["<secret:github-token>", "same"]
            ])
        );
        assert_ne!(
            digest(&duplicates),
            digest(&json!({"properties": {"<secret:github-token>": "same"}}))
        );
    }

    #[test]
    fn issue2995_digest_preserves_original_trigger_context_for_every_match() {
        let first = "0123456789abcdef".repeat(2);
        let second = "fedcba9876543210".repeat(2);
        let text = format!("secret={first}/{second}");
        assert!(super::super::check(&text).is_err());
        let projected = projected(&json!({"content": text}));
        assert_eq!(
            projected["content"],
            "secret=<secret:hex-credential-token>/<secret:hex-credential-token>"
        );
    }

    #[test]
    fn issue2995_digest_scans_properties_like_gate_without_sibling_context() {
        let id = "550e8400-e29b-41d4-a716-446655440000";
        let input = json!({"properties": {"password": id}});
        assert!(super::super::check_json(&input).is_ok());
        assert_eq!(projected(&input)["properties"], json!([["password", id]]));
        let blocked = json!({"properties": {"value": format!("password={id}")}});
        assert!(super::super::check_json(&blocked).is_err());
        assert_eq!(
            projected(&blocked)["properties"],
            json!([["value", "password=<secret:uuid-near-trigger>"]])
        );
    }

    #[test]
    fn issue2995_digest_keeps_unicode_prose_but_no_bridge_shape() {
        let first = "0123456789abcdef01";
        let second = "fedcba98765432";
        let input = json!({"content": format!("说明 secret={first}\u{200b}{second} 结束")});
        assert!(super::super::check(input["content"].as_str().unwrap()).is_err());
        assert_eq!(
            projected(&input)["content"],
            "说明 secret=<secret:hex-credential-token> 结束"
        );
        assert_eq!(
            projected(&json!({"content": "说明 redis://:密码@example.test 结束"}))["content"],
            "说明 <secret:url-userinfo> 结束"
        );
    }

    #[test]
    fn issue2995_digest_budget_masks_unknown_suffix_without_partial_data() {
        let secret = github_key('A', 36);
        let text = format!("{secret} {secret}");
        // Permit context construction and one scan, but not the next sweep.
        assert_eq!(
            mask_string(&text, text.len() * 2),
            "<secret:github-token><unscanned>"
        );
        assert_eq!(mask_string("ordinary", 1), UNSCANNED);
        assert_eq!(mask_string("different length", 1), UNSCANNED);
        assert_eq!(mask_string("", 0), "");
        let first = json!({"content": "a".repeat(MAX_MASK_SCAN_WORK_BYTES + 1)});
        let second = json!({"content": "b".repeat(MAX_MASK_SCAN_WORK_BYTES + 33)});
        assert_eq!(projected(&first)["content"], UNSCANNED);
        assert_eq!(digest(&first), digest(&second));
    }

    #[test]
    fn issue2995_digest_budget_allocation_ignores_raw_property_key_order() {
        let first = json!({"properties": {github_key('A', 36): "retained", github_key('B', 48): "changed"}});
        let second = json!({"properties": {github_key('A', 36): "changed", github_key('B', 48): "retained"}});
        // Both keys exceed quota; both ordinary values fit. A shared sequential
        // budget would make iteration order affect which values survive.
        assert_eq!(
            project_candidate(&first, 20),
            project_candidate(&second, 20)
        );
        assert_eq!(
            project_candidate(&first, 20)["properties"],
            json!([["<unscanned>", "changed"], ["<unscanned>", "retained"]])
        );
        // Even unknown root keys never bypass masking or collide in a map.
        assert_eq!(
            project_candidate(
                &json!({github_key('A', 36): true}),
                MAX_MASK_SCAN_WORK_BYTES
            ),
            json!([["<secret:github-token>", true]])
        );
    }
}

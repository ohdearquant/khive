use serde_json::Value;

use crate::types::{ArgValue, ParsedOp};

#[cfg(test)]
use crate::types::{DslError, ExecutionMode, ParsedRequest};

/// Extracts statically knowable, substrate-prefixed write-conflict keys for `op`.
///
/// Missing, dynamic, or non-string targets contribute no key. The result order
/// follows the tool's argument/entry order.
/// See `crates/khive-request/docs/api/write-conflicts.md` for key formats.
pub fn write_keys_for_op_pub(op: &ParsedOp) -> Vec<String> {
    let mut keys = Vec::new();
    match op.tool.as_str() {
        "update" | "delete" => {
            if let Some(ArgValue::Value(Value::String(s))) = op.args.get("id") {
                keys.push(format!("entity:{s}"));
            }
        }
        "merge" => {
            for name in &["into_id", "from_id"] {
                if let Some(ArgValue::Value(Value::String(s))) = op.args.get(*name) {
                    keys.push(format!("entity:{s}"));
                }
            }
        }
        "link" => {
            // Edge keys must not collide with their endpoint entities.
            if let (
                Some(ArgValue::Value(Value::String(s))),
                Some(ArgValue::Value(Value::String(t))),
                Some(ArgValue::Value(Value::String(r))),
            ) = (
                op.args.get("source_id"),
                op.args.get("target_id"),
                op.args.get("relation"),
            ) {
                push_link_key(&mut keys, s, t, r);
            }

            // Bulk and singleton links share natural keys so equivalent writes collide.
            if let Some(ArgValue::Value(Value::Array(links))) = op.args.get("links") {
                for link in links {
                    let Some(obj) = link.as_object() else {
                        continue;
                    };
                    let (Some(s), Some(t), Some(r)) = (
                        obj.get("source_id").and_then(Value::as_str),
                        obj.get("target_id").and_then(Value::as_str),
                        obj.get("relation").and_then(Value::as_str),
                    ) else {
                        continue;
                    };
                    push_link_key(&mut keys, s, t, r);
                }
            }
        }
        _ => {}
    }
    keys
}

/// Adds a natural edge key, ordering endpoints for known symmetric relations.
fn push_link_key(keys: &mut Vec<String>, source: &str, target: &str, relation: &str) {
    let relation_key = canonical_relation_key(relation);
    let (source_key, target_key) = if is_static_symmetric_relation(&relation_key) && target < source
    {
        (target, source)
    } else {
        (source, target)
    };
    keys.push(format!(
        "edge-natural:{source_key}:{target_key}:{relation_key}"
    ));
}

/// Normalizes relation spelling without adding a `khive-types` dependency.
fn canonical_relation_key(relation: &str) -> String {
    let normalized: String = relation
        .chars()
        .map(|c| {
            if c == '-' {
                '_'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    match normalized.as_str() {
        "competeswith" => "competes_with".to_string(),
        "composedwith" => "composed_with".to_string(),
        _ => normalized,
    }
}

/// Returns whether the deliberately conservative local relation set is symmetric.
fn is_static_symmetric_relation(relation: &str) -> bool {
    matches!(relation, "competes_with" | "composed_with")
}

/// Scans a parsed batch for duplicate write keys; ordered chains are exempt.
#[cfg(test)]
pub(crate) fn check_write_key_conflicts(req: &ParsedRequest) -> Result<(), DslError> {
    if req.mode == ExecutionMode::Chain {
        return Ok(());
    }
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for op in &req.ops {
        let keys = write_keys_for_op_pub(op);
        for key in keys {
            if let Some(first) = seen.get(&key) {
                return Err(DslError::WriteKeyConflict {
                    id: key,
                    first_op: first.clone(),
                    second_op: op.tool.clone(),
                });
            }
            seen.insert(key, op.tool.clone());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_request;

    #[test]
    fn no_conflict_on_non_write_ops() {
        let r =
            parse_request(r#"[list(kind="entity"), search(kind="entity", query="x")]"#).unwrap();
        check_write_key_conflicts(&r).unwrap();
    }

    #[test]
    fn update_and_delete_same_id_conflict() {
        let r =
            parse_request(r#"[update(id="abc-123", name="new"), delete(id="abc-123")]"#).unwrap();
        let err = check_write_key_conflicts(&r).unwrap_err();
        assert!(
            matches!(&err, DslError::WriteKeyConflict { id, first_op, second_op }
                if id == "entity:abc-123" && first_op == "update" && second_op == "delete"),
            "expected WriteKeyConflict with entity-prefixed key, got {err:?}"
        );
    }

    #[test]
    fn two_updates_same_id_conflict() {
        let r = parse_request(
            r#"[update(id="uuid-1", name="a"), update(id="uuid-1", description="b")]"#,
        )
        .unwrap();
        let err = check_write_key_conflicts(&r).unwrap_err();
        assert!(
            matches!(&err, DslError::WriteKeyConflict { id, .. } if id == "entity:uuid-1"),
            "expected WriteKeyConflict with entity-prefixed key, got {err:?}"
        );
    }

    #[test]
    fn merge_from_id_conflicts_with_delete() {
        let r =
            parse_request(r#"[merge(into_id="new-id", from_id="old-id"), delete(id="old-id")]"#)
                .unwrap();
        let err = check_write_key_conflicts(&r).unwrap_err();
        assert!(
            matches!(&err, DslError::WriteKeyConflict { id, .. } if id == "entity:old-id"),
            "expected WriteKeyConflict with entity-prefixed key, got {err:?}"
        );
    }

    #[test]
    fn different_ids_no_conflict() {
        let r = parse_request(
            r#"[update(id="id-1", name="a"), delete(id="id-2"), update(id="id-3", name="c")]"#,
        )
        .unwrap();
        check_write_key_conflicts(&r).unwrap();
    }

    #[test]
    fn chain_mode_skips_conflict_detection() {
        let r = parse_request(r#"update(id="same-id", name="a") | delete(id="same-id")"#).unwrap();
        assert_eq!(r.mode, ExecutionMode::Chain);
        check_write_key_conflicts(&r).unwrap();
    }

    #[test]
    fn link_source_id_does_not_conflict_with_entity_update() {
        let r = parse_request(
            r#"[update(id="node-1", name="x"), link(source_id="node-1", target_id="node-2", relation="extends")]"#,
        )
        .unwrap();
        check_write_key_conflicts(&r).unwrap();
    }

    #[test]
    fn two_links_same_natural_key_conflict() {
        let r = parse_request(
            r#"[link(source_id="a", target_id="b", relation="extends"), link(source_id="a", target_id="b", relation="extends")]"#,
        )
        .unwrap();
        let err = check_write_key_conflicts(&r).unwrap_err();
        assert!(
            matches!(&err, DslError::WriteKeyConflict { id, .. }
                if id == "edge-natural:a:b:extends"),
            "expected WriteKeyConflict on edge-natural key, got {err:?}"
        );
    }

    #[test]
    fn single_write_op_no_conflict() {
        let r = parse_request(r#"delete(id="solo-id")"#).unwrap();
        assert_eq!(r.mode, ExecutionMode::Single);
        check_write_key_conflicts(&r).unwrap();
    }

    #[test]
    fn bulk_link_and_singleton_same_natural_key_conflict() {
        let r = parse_request(
            r#"[link(links=[{"source_id":"a","target_id":"b","relation":"extends","weight":0.1}]), link(source_id="a", target_id="b", relation="extends", weight=0.9)]"#,
        )
        .unwrap();
        let err = check_write_key_conflicts(&r).unwrap_err();
        assert!(
            matches!(&err, DslError::WriteKeyConflict { id, .. }
                if id == "edge-natural:a:b:extends"),
            "expected WriteKeyConflict on edge-natural key, got {err:?}"
        );
    }

    #[test]
    fn reversed_symmetric_links_conflict() {
        let r = parse_request(
            r#"[link(source_id="b", target_id="a", relation="competes_with"), link(source_id="a", target_id="b", relation="competes_with")]"#,
        )
        .unwrap();
        let err = check_write_key_conflicts(&r).unwrap_err();
        assert!(
            matches!(&err, DslError::WriteKeyConflict { id, .. }
                if id == "edge-natural:a:b:competes_with"),
            "expected WriteKeyConflict on canonicalized symmetric key, got {err:?}"
        );
    }

    #[test]
    fn reversed_non_symmetric_links_do_not_conflict() {
        let r = parse_request(
            r#"[link(source_id="b", target_id="a", relation="extends"), link(source_id="a", target_id="b", relation="extends")]"#,
        )
        .unwrap();
        check_write_key_conflicts(&r).unwrap();
    }

    #[test]
    fn write_keys_for_op_pub_bulk_link_extracts_all_edges() {
        let r = parse_request(
            r#"link(links=[{"source_id":"a","target_id":"b","relation":"extends"},{"source_id":"c","target_id":"d","relation":"extends"}])"#,
        )
        .unwrap();
        let keys = write_keys_for_op_pub(&r.ops[0]);
        assert_eq!(
            keys,
            vec![
                "edge-natural:a:b:extends".to_string(),
                "edge-natural:c:d:extends".to_string(),
            ]
        );
    }
}

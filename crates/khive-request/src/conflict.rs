use serde_json::Value;

use crate::types::{ArgValue, ParsedOp};

#[cfg(test)]
use crate::types::{DslError, ExecutionMode, ParsedRequest};

/// Extract substrate-prefixed write-conflict keys from one op for parallel-batch preflight.
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
            // `link` writes an edge, not an entity. Use a natural-key format so
            // update(id="X") + link(source_id="X", ...) do NOT conflict (they target
            // different substrates).
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

            // Bulk form: `link(links=[{source_id, target_id, relation, ...}, ...])`.
            // Each contained natural edge must contribute its own key so a bulk
            // entry conflicts with a colliding singleton `link` (or another bulk
            // entry) instead of silently racing through storage.
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

/// Build the substrate-prefixed natural-edge write key for one link entry,
/// canonicalizing endpoint order for statically-known symmetric relations so
/// reversed singleton links (e.g. `link(A,B,competes_with)` vs
/// `link(B,A,competes_with)`) collide on the same key.
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

/// Normalize relation spelling (case/hyphen variants) without depending on
/// `khive-types`, so bulk and singleton entries produce identical keys.
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

/// Relations known to be symmetric (endpoint order does not matter) without
/// consulting the full relation registry. Kept conservative and local to
/// avoid coupling `khive-request` to `khive-types`.
fn is_static_symmetric_relation(relation: &str) -> bool {
    matches!(relation, "competes_with" | "composed_with")
}

/// Scan a parsed batch for write-key conflicts; skips chain mode (sequential by design).
#[cfg(test)]
pub(crate) fn check_write_key_conflicts(req: &ParsedRequest) -> Result<(), DslError> {
    // Chain mode is sequentially ordered; skip conflict detection.
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

    // ── write-key conflict detection ──────────────────────────────────────────

    #[test]
    fn no_conflict_on_non_write_ops() {
        // search + list ops share no write keys; must pass.
        let r =
            parse_request(r#"[list(kind="entity"), search(kind="entity", query="x")]"#).unwrap();
        check_write_key_conflicts(&r).unwrap();
    }

    #[test]
    fn update_and_delete_same_id_conflict() {
        // Two ops targeting the same UUID should be rejected.
        // Keys are substrate-prefixed: entity:<uuid>.
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
        // merge's from_id overlaps a delete's id — both are entity writes.
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
        // Each op has a distinct UUID; no conflict.
        let r = parse_request(
            r#"[update(id="id-1", name="a"), delete(id="id-2"), update(id="id-3", name="c")]"#,
        )
        .unwrap();
        check_write_key_conflicts(&r).unwrap();
    }

    #[test]
    fn chain_mode_skips_conflict_detection() {
        // Chain ops run sequentially; write-key preflight is skipped.
        let r = parse_request(r#"update(id="same-id", name="a") | delete(id="same-id")"#).unwrap();
        assert_eq!(r.mode, ExecutionMode::Chain);
        // Must not return an error even though the same id appears in both ops.
        check_write_key_conflicts(&r).unwrap();
    }

    #[test]
    fn link_source_id_does_not_conflict_with_entity_update() {
        // update(id="X") + link(source_id="X", ...) must NOT conflict — `link` writes an
        // edge record, not the entity at "X". Substrate-prefixed keys distinguish them:
        // entity:X vs edge-natural:X:Y:rel.
        let r = parse_request(
            r#"[update(id="node-1", name="x"), link(source_id="node-1", target_id="node-2", relation="extends")]"#,
        )
        .unwrap();
        check_write_key_conflicts(&r).unwrap();
    }

    #[test]
    fn two_links_same_natural_key_conflict() {
        // Two link ops targeting the same (source, target, relation) triple conflict
        // because they would produce duplicate edges.
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
        // A bulk `links=[...]` entry must contribute the same write key as an
        // equivalent singleton `link` op so they are detected as conflicting.
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
        // competes_with is symmetric: A->B and B->A must collide on one key.
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
        // `extends` is directional: reversed endpoints must NOT collide.
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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;

use khive_types::EdgeRelation;
use serde_json::Value;

use crate::types::{ArgValue, ParsedOp};

#[cfg(test)]
use crate::types::{DslError, ExecutionMode, ParsedRequest};

/// Direct conflict participants for each flat-batch operation, including itself.
///
/// Lists are sorted and unique. Repeated key occurrences within one operation
/// retain the existing flat-batch refusal and produce a self-only list. This
/// reports static write keys; it does not resolve IDs or change admission.
pub fn write_key_conflict_ops(ops: &[ParsedOp]) -> Vec<Vec<usize>> {
    participants_for_keys(
        ops.len(),
        key_claims(ops)
            .into_values()
            .filter(|claims| claims.len() > 1),
    )
}

/// Direct conflict participants for each global leaf of a parallel batch of chains.
///
/// Only keys shared across different units count. For such a key, every claiming
/// leaf is included, even repeated claims within a unit. An innocent leaf in a
/// refused unit has an empty list. Lists never expand through transitive conflicts.
/// `ranges` must partition `ops`, as guaranteed by the request parser.
pub fn unit_write_key_conflict_ops(ops: &[ParsedOp], ranges: &[Range<usize>]) -> Vec<Vec<usize>> {
    let mut unit_of = vec![0; ops.len()];
    for (unit, range) in ranges.iter().enumerate() {
        for leaf in range.clone() {
            unit_of[leaf] = unit;
        }
    }
    participants_for_keys(
        ops.len(),
        key_claims(ops).into_values().filter(|claims| {
            claims
                .iter()
                .any(|&leaf| unit_of[leaf] != unit_of[claims[0]])
        }),
    )
}

fn key_claims(ops: &[ParsedOp]) -> HashMap<String, Vec<usize>> {
    let mut claims: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, op) in ops.iter().enumerate() {
        for key in write_keys_for_op_pub(op) {
            claims.entry(key).or_default().push(index);
        }
    }
    claims
}

fn participants_for_keys(count: usize, keys: impl Iterator<Item = Vec<usize>>) -> Vec<Vec<usize>> {
    let mut participants = vec![BTreeSet::new(); count];
    for claims in keys {
        let claims: BTreeSet<_> = claims.into_iter().collect();
        for &index in &claims {
            participants[index].extend(claims.iter().copied());
        }
    }
    participants
        .into_iter()
        .map(|indices| indices.into_iter().collect())
        .collect()
}

/// One cross-unit static write-key conflict: `leaf` (inside the unit this
/// entry is filed under) claims the same statically knowable write key as
/// `other_leaf`, which belongs to a *different* unit, `other_unit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitKeyConflict {
    pub key: String,
    pub leaf: usize,
    pub other_unit: usize,
    pub other_leaf: usize,
}

/// Finds every statically knowable write-key collision between *different*
/// units of a bracketed batch of chains (ADR-016 Amendment 2).
///
/// A key repeated by two leaves of the *same* unit is legal, because a chain's own
/// leaves are already ordered, and is never reported. A key claimed by leaves
/// in two different units is reported once per affected unit (symmetric:
/// both units get an entry), each naming the other unit's conflicting global
/// leaf position, so a caller can refuse both units before any of their
/// leaves dispatch. Units absent from the returned map share no cross-unit
/// key with any other unit.
pub fn unit_write_key_conflicts(
    ops: &[ParsedOp],
    ranges: &[Range<usize>],
) -> BTreeMap<usize, Vec<UnitKeyConflict>> {
    // Per unit, collapse repeated in-unit keys to their first-occurring leaf
    // (deterministic: ranges are walked in leaf order regardless of the
    // arbitrary hash order `write_keys_for_op_pub` or a later `HashMap`
    // iteration might otherwise introduce).
    let mut per_unit_keys: Vec<HashMap<String, usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        let mut keys: HashMap<String, usize> = HashMap::new();
        for leaf in range.clone() {
            for key in write_keys_for_op_pub(&ops[leaf]) {
                keys.entry(key).or_insert(leaf);
            }
        }
        per_unit_keys.push(keys);
    }

    let mut first_claim: HashMap<String, (usize, usize)> = HashMap::new();
    let mut conflicts: BTreeMap<usize, Vec<UnitKeyConflict>> = BTreeMap::new();
    for (unit_index, keys) in per_unit_keys.iter().enumerate() {
        for (key, &leaf) in keys {
            match first_claim.get(key) {
                Some(&(other_unit, other_leaf)) if other_unit != unit_index => {
                    conflicts
                        .entry(unit_index)
                        .or_default()
                        .push(UnitKeyConflict {
                            key: key.clone(),
                            leaf,
                            other_unit,
                            other_leaf,
                        });
                    conflicts
                        .entry(other_unit)
                        .or_default()
                        .push(UnitKeyConflict {
                            key: key.clone(),
                            leaf: other_leaf,
                            other_unit: unit_index,
                            other_leaf: leaf,
                        });
                }
                Some(_) => {}
                None => {
                    first_claim.insert(key.clone(), (unit_index, leaf));
                }
            }
        }
    }
    for entries in conflicts.values_mut() {
        entries.sort_by_key(|c| (c.leaf, c.other_leaf));
        entries.dedup();
    }
    conflicts
}

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
            // A dry-run merge reads the pair and returns a prediction, so it targets
            // no stored record and declares no key. Only a literal `true` reads as a
            // preview: absent, `false` and a non-boolean value keep the keys, on the
            // same static-knowability rule the rest of this builder follows, so an
            // argument this cannot read as a preview stays conservative.
            let previews_without_writing = matches!(
                op.args.get("dry_run"),
                Some(ArgValue::Value(Value::Bool(true)))
            );
            if !previews_without_writing {
                for name in &["into_id", "from_id"] {
                    if let Some(ArgValue::Value(Value::String(s))) = op.args.get(*name) {
                        keys.push(format!("entity:{s}"));
                    }
                }
            }
        }
        "link" => {
            // Edge keys must not collide with their endpoint entities.
            // `source`, `target` and `kind` are accepted spellings of `source_id`,
            // `target_id` and `relation`. Each field reads the canonical name first
            // and the alias only when the canonical name is absent, so both
            // spellings of one edge claim the same key. A call giving both
            // spellings of a field is refused by the handler, so which one this
            // reads for it does not matter.
            let link_arg = |canonical: &str, alias: &str| {
                op.args.get(canonical).or_else(|| op.args.get(alias))
            };
            if let (
                Some(ArgValue::Value(Value::String(s))),
                Some(ArgValue::Value(Value::String(t))),
                Some(ArgValue::Value(Value::String(r))),
            ) = (
                link_arg("source_id", "source"),
                link_arg("target_id", "target"),
                link_arg("relation", "kind"),
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
    let parsed = relation.parse::<EdgeRelation>().ok();
    let relation_key = parsed.as_ref().map_or(relation, |r| r.as_str());
    let (source_key, target_key) =
        parsed.map_or((source, target), |r| r.canonical_endpoints(source, target));
    keys.push(format!(
        "edge-natural:{source_key}:{target_key}:{relation_key}"
    ));
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
                    conflict_ops: key_claims(&req.ops)[&key]
                        .iter()
                        .copied()
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect(),
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
#[path = "conflict_tests.rs"]
mod tests;

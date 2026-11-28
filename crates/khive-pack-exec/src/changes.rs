//! Bounded change lists (ADR-181 Amendment 18). A reply carries a prefix of
//! the full list, cut before the first change that would pass
//! `changed_limit` changes or [`CHANGED_BYTE_BOUND`] bytes of the list's
//! JSON-lines form, beside three fields: `changed_count` (the full length),
//! `changed_truncated` and `changed_ref`, the blob that holds the full list
//! as JSON lines. The persisted receipt keeps the full list; only replies are
//! bounded.

use serde_json::{json, Value};

use khive_runtime::secret_gate::{mask_for_redaction_surface, RedactionSurface};
use khive_runtime::{KhiveRuntime, RuntimeError};

use crate::tree;

/// Changes a reply carries when the call does not set `changed_limit`.
pub const DEFAULT_CHANGED_LIMIT: usize = 1000;
/// The largest `changed_limit` a call may set. A larger one is refused, never clamped.
pub const MAX_CHANGED_LIMIT: usize = 10_000;
/// Bytes of change data one bounded list carries, counted in the JSON-lines
/// form: each change's compact JSON and its newline. No argument raises it.
pub const CHANGED_BYTE_BOUND: usize = 262_144;

/// Read `changed_limit`. Absent or null is [`DEFAULT_CHANGED_LIMIT`]; a value
/// above [`MAX_CHANGED_LIMIT`] is refused naming it, and a negative or
/// non-integer value is refused.
pub fn changed_limit(params: &Value) -> Result<usize, RuntimeError> {
    let value = match params.get("changed_limit") {
        None | Some(Value::Null) => return Ok(DEFAULT_CHANGED_LIMIT),
        Some(value) => value,
    };
    let limit = value.as_u64().ok_or_else(|| {
        RuntimeError::InvalidInput(format!(
            "changed_limit must be an integer from 0 to 10,000; got {}",
            mask_for_redaction_surface(RedactionSurface::GateProbe, &value.to_string())
        ))
    })?;
    match usize::try_from(limit) {
        Ok(limit) if limit <= MAX_CHANGED_LIMIT => Ok(limit),
        _ => Err(RuntimeError::InvalidInput(format!(
            "changed_limit {limit} exceeds the maximum of 10,000"
        ))),
    }
}

/// One change's line in the JSON-lines form: the compact JSON the reply uses,
/// then a newline.
fn line(change: &Value) -> String {
    let mut line = change.to_string();
    line.push('\n');
    line
}

/// The full list as JSON lines, in the list's own (path) order.
pub fn json_lines(changes: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    for change in changes {
        out.extend_from_slice(line(change).as_bytes());
    }
    out
}

/// Where a list was cut for one reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prefix {
    /// Changes the reply carries.
    pub kept: usize,
    /// Bytes of JSON lines those changes take.
    pub bytes: usize,
    /// The byte budget, not the count, stopped the list.
    pub cut_by_bytes: bool,
}

/// The prefix a reply carries: it stops before the first change that would
/// pass `limit` changes or `budget` bytes.
pub fn prefix(changes: &[Value], limit: usize, budget: usize) -> Prefix {
    let mut bytes = 0usize;
    for (index, change) in changes.iter().enumerate() {
        if index == limit {
            return Prefix {
                kept: index,
                bytes,
                cut_by_bytes: false,
            };
        }
        let cost = change.to_string().len() + 1;
        if bytes + cost > budget {
            return Prefix {
                kept: index,
                bytes,
                cut_by_bytes: true,
            };
        }
        bytes += cost;
    }
    Prefix {
        kept: changes.len(),
        bytes,
        cut_by_bytes: false,
    }
}

/// Write the full list as one JSON-lines blob and return its reference. An
/// empty list writes nothing and has no reference. The blob is content
/// addressed, so one list has one reference whichever verb wrote it.
pub async fn store_list(
    rt: &KhiveRuntime,
    changes: &[Value],
) -> Result<Option<String>, RuntimeError> {
    if changes.is_empty() {
        return Ok(None);
    }
    let stored = tree::blob_store(rt)?.put(json_lines(changes)).await?;
    Ok(Some(stored.as_str().to_string()))
}

/// Set `target`'s `changed` to the prefix of `full` that fits `limit` and the
/// remaining `budget`, with the three fields beside it, and spend the bytes.
/// A list the byte budget cut spends what is left of it, so every later list
/// drawn from the same budget is empty.
pub fn bound_into(
    target: &mut Value,
    full: &[Value],
    changed_ref: Option<&str>,
    limit: usize,
    budget: &mut usize,
) {
    let cut = prefix(full, limit, *budget);
    *budget = if cut.cut_by_bytes {
        0
    } else {
        *budget - cut.bytes
    };
    target["changed"] = Value::Array(full[..cut.kept].to_vec());
    target["changed_count"] = json!(full.len());
    target["changed_truncated"] = json!(cut.kept < full.len());
    target["changed_ref"] = json!(changed_ref);
}

/// [`bound_into`] with a whole byte budget of its own: the single-list replies.
pub fn bound_reply(target: &mut Value, full: &[Value], changed_ref: Option<&str>, limit: usize) {
    let mut budget = CHANGED_BYTE_BOUND;
    bound_into(target, full, changed_ref, limit, &mut budget);
}

/// Bound a stored receipt's list in place. The reference comes from the row:
/// a receipt stored before Amendment 18 has none, so a cut list of such a row
/// reads `changed_truncated: true` with `changed_ref: null`. A row that holds
/// no change list at all is returned as stored: there is nothing to bound, and
/// older stored receipts stay unchanged.
pub fn bound_stored(receipt: &mut Value, limit: usize, budget: &mut usize) {
    let full = match receipt.get_mut("changed") {
        Some(Value::Array(items)) => std::mem::take(items),
        _ => return,
    };
    let changed_ref = receipt
        .get("changed_ref")
        .and_then(Value::as_str)
        .map(str::to_string);
    bound_into(receipt, &full, changed_ref.as_deref(), limit, budget);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(path: &str) -> Value {
        json!({"path": path, "op": "deleted", "ref": null, "base_ref": "00"})
    }

    fn cost(change: &Value) -> usize {
        change.to_string().len() + 1
    }

    #[test]
    fn the_limit_cuts_before_the_bytes_do() {
        let list: Vec<Value> = (0..5).map(|i| change(&format!("p{i}"))).collect();
        let cut = prefix(&list, 3, CHANGED_BYTE_BOUND);
        assert_eq!(cut.kept, 3);
        assert!(!cut.cut_by_bytes);
        assert_eq!(cut.bytes, list[..3].iter().map(cost).sum::<usize>());
        assert_eq!(prefix(&list, 0, CHANGED_BYTE_BOUND).kept, 0);
        assert_eq!(prefix(&list, 10, CHANGED_BYTE_BOUND).kept, 5);
    }

    #[test]
    fn the_bytes_cut_before_the_change_that_would_cross_them() {
        let list: Vec<Value> = (0..5).map(|i| change(&format!("p{i}"))).collect();
        let two = cost(&list[0]) + cost(&list[1]);
        // Exactly two lines fit; one byte short of the third.
        let cut = prefix(&list, 10, two + cost(&list[2]) - 1);
        assert_eq!((cut.kept, cut.bytes, cut.cut_by_bytes), (2, two, true));
        // The bound is inclusive: a line that ends exactly on it is kept.
        let cut = prefix(&list, 10, two);
        assert_eq!((cut.kept, cut.bytes), (2, two));
    }

    #[test]
    fn the_json_lines_form_is_each_reply_object_and_a_newline() {
        let list = vec![change("a"), change("b")];
        let text = String::from_utf8(json_lines(&list)).unwrap();
        let lines: Vec<&str> = text.split_terminator('\n').collect();
        assert_eq!(lines.len(), 2);
        for (line, change) in lines.iter().zip(&list) {
            assert_eq!(serde_json::from_str::<Value>(line).unwrap(), *change);
            assert_eq!(*line, change.to_string());
        }
        assert!(text.ends_with('\n'));
        assert!(json_lines(&[]).is_empty());
    }

    #[test]
    fn one_budget_across_rows_leaves_later_rows_empty_with_their_fields() {
        let long = "x".repeat(300);
        let row_list: Vec<Value> = (0..1000)
            .map(|i| change(&format!("{i:04}{long}")))
            .collect();
        let mut rows: Vec<Value> = (0..3)
            .map(|i| {
                json!({"id": i, "changed": row_list.clone(), "changed_count": 1000, "changed_ref": format!("ref{i}")})
            })
            .collect();
        let mut budget = CHANGED_BYTE_BOUND;
        for row in &mut rows {
            bound_stored(row, 1000, &mut budget);
        }
        let spent: usize = rows
            .iter()
            .flat_map(|row| row["changed"].as_array().unwrap().iter().map(cost))
            .sum();
        assert!(spent <= CHANGED_BYTE_BOUND, "{spent}");
        let first = rows[0]["changed"].as_array().unwrap();
        assert!(!first.is_empty() && first.len() < 1000);
        assert_eq!(first[..], row_list[..first.len()]);
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(row["changed_count"], 1000);
            assert_eq!(row["changed_truncated"], true);
            assert_eq!(row["changed_ref"], format!("ref{index}"));
            if index > 0 {
                assert_eq!(row["changed"], json!([]));
            }
        }
    }

    #[test]
    fn a_stored_row_without_a_reference_reads_truncated_with_a_null_reference() {
        let list: Vec<Value> = (0..3).map(|i| change(&format!("p{i}"))).collect();
        let mut legacy = json!({"id": "r", "changed": list});
        let mut budget = CHANGED_BYTE_BOUND;
        bound_stored(&mut legacy, 2, &mut budget);
        assert_eq!(legacy["changed"].as_array().unwrap().len(), 2);
        assert_eq!(legacy["changed_count"], 3);
        assert_eq!(legacy["changed_truncated"], true);
        assert_eq!(legacy["changed_ref"], Value::Null);

        let listless = json!({"id": "r", "success": true});
        let mut read = listless.clone();
        bound_stored(&mut read, 2, &mut budget);
        assert_eq!(read, listless, "a row without a list is returned as stored");
    }

    #[test]
    fn changed_limit_refuses_rather_than_clamps() {
        assert_eq!(changed_limit(&json!({})).unwrap(), DEFAULT_CHANGED_LIMIT);
        assert_eq!(
            changed_limit(&json!({"changed_limit": null})).unwrap(),
            1000
        );
        assert_eq!(changed_limit(&json!({"changed_limit": 0})).unwrap(), 0);
        assert_eq!(
            changed_limit(&json!({"changed_limit": 10_000})).unwrap(),
            10_000
        );
        let over = changed_limit(&json!({"changed_limit": 10_001})).unwrap_err();
        assert!(over.to_string().contains("10,000"), "{over}");
        for refused in [json!(-1), json!("5"), json!(2.5), json!(true)] {
            assert!(changed_limit(&json!({ "changed_limit": refused })).is_err());
        }
    }
}

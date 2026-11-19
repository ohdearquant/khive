use super::{EntityDedupMergePolicy, HashSet, RuntimeError, RuntimeResult, Value};

/// Keep executable schedule intent behind the schedule pack's state-machine verbs.
///
/// `scheduled_event` notes carry both replay payloads and lifecycle state. Allowing
/// generic note update/merge to rewrite either would turn the immutable creator event
/// into a bearer credential for attacker-selected work: replay would attribute the
/// changed row to its original creator. Schedule's own transitions use its private
/// note-store CAS helpers and therefore do not pass through this generic curation seam.
pub(super) fn reject_pack_managed_schedule_mutation(
    note: &khive_storage::note::Note,
    operation: &str,
) -> RuntimeResult<()> {
    if note.kind == "scheduled_event" {
        return Err(RuntimeError::InvalidInput(format!(
            "cannot {operation} a schedule-managed `scheduled_event` note through generic KG \
             mutation; use schedule.cancel or create a replacement schedule"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Merge helpers (pure functions — easier to unit test)
// ---------------------------------------------------------------------------

/// `pub(crate)` so `crate::atomic_prepare::prepare_merge` can reuse this exact
/// field-fold semantics for atomic/non-atomic parity.
pub(crate) fn merge_string_field(
    into: &str,
    from: &str,
    strategy: EntityDedupMergePolicy,
) -> String {
    match strategy {
        EntityDedupMergePolicy::PreferInto | EntityDedupMergePolicy::Union => into.to_string(),
        EntityDedupMergePolicy::PreferFrom => from.to_string(),
    }
}

/// Property keys on a pack-owned note that the owning pack establishes and
/// then reads back to decide something structural about the record.
///
/// The test for membership is that both halves hold: the key is written
/// under the owner's authority rather than from caller input, AND its value
/// is read to decide identity, grouping, routing, lifecycle, visibility,
/// authorization, deduplication, or membership. `from_actor`, `direction` and
/// `sent_at` answer "who wrote this, in which direction, when"; `outbound_ref`
/// and `thread_id` answer "which record is this one's author-side original,
/// and which conversation does it belong to". `subject` is reproduced
/// verbatim when a record is re-emitted; `wire_message_id` and `external_id`
/// are the author-side citation and correlation key a reply is routed
/// against. The set is therefore not "keys that identify a party" — it is
/// "keys the owner established and later trusts".
///
/// Naming one of these in a caller-supplied `properties` patch is refused by
/// `update` on a pack-owned kind (see [`owner_established_property_named_in`]).
///
/// `to_actor` belongs here alongside `from_actor`: comm establishes it at
/// send time from the `to=` param, and `comm.read` trusts a present string
/// value to decide whether the caller is the addressee, failing open only
/// when the key is absent or non-string. A caller must not be able to
/// retarget a delivered message's addressee via a patch that names no other
/// currently-protected key.
///
/// Membership here governs writes to an EXISTING record only. Introducing one
/// of these keys at create time is a separate question and is not addressed
/// by this constant.
pub(crate) const OWNER_ESTABLISHED_PROPERTIES: &[&str] = &[
    "from_actor",
    "to_actor",
    "direction",
    "sent_at",
    "outbound_ref",
    "thread_id",
    "subject",
    "wire_message_id",
    "external_id",
];

/// Kind-specific identity that generic updates cannot patch and merges must
/// retain from the surviving record. Message transport evidence belongs to
/// `comm.ingest`; health coordinates determine the UUID used by `comm.heartbeat`.
/// Unlike OWNER_ESTABLISHED_PROPERTIES, these names remain ordinary metadata
/// on other kinds, including tasks and memories.
const KIND_OWNED_PROPERTIES: &[(&str, &[&str])] = &[
    (
        "message",
        &[
            "quarantined",
            "channel_kind",
            "channel_slug",
            "delivery_hold",
            "delivery_hold_reason",
            "delivery_hold_at",
            "external_id_diagnostic_note_id",
        ],
    ),
    ("channel_health", &["channel_kind", "channel_slug"]),
];

pub(crate) fn kind_owned_properties(kind: &str) -> &'static [&'static str] {
    KIND_OWNED_PROPERTIES
        .iter()
        .find_map(|(owned_kind, keys)| (*owned_kind == kind).then_some(*keys))
        .unwrap_or(&[])
}

/// Whether a stored message note carries a live quarantine disposition.
///
/// The marker is written by transports as JSON `true` and by some channel
/// adapters as the string `"true"`; both spellings are live in stored data
/// (`comm.health` counts both). Any present value other than an explicit
/// boolean `false` or string `"false"` reads as quarantined, so an unexpected
/// encoding fails closed.
pub(super) fn message_is_quarantined(note: &khive_storage::note::Note) -> bool {
    let Some(Value::Object(map)) = note.properties.as_ref() else {
        return false;
    };
    match map.get("quarantined") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::String(value)) => value != "false",
        Some(_) => true,
    }
}

/// The first [`OWNER_ESTABLISHED_PROPERTIES`] key a caller-supplied
/// `properties` patch names, if any.
///
/// Naming a key is the whole test: `update_note` folds the patch with
/// `PreferFrom`, so a named key overwrites the stored value and an unnamed one
/// leaves it untouched. A non-object patch names nothing.
pub(crate) fn owner_established_property_named_in(patch: &Value) -> Option<&'static str> {
    let Value::Object(map) = patch else {
        return None;
    };
    OWNER_ESTABLISHED_PROPERTIES
        .iter()
        .copied()
        .find(|key| map.contains_key(*key))
}

/// Restore the into-note's [`OWNER_ESTABLISHED_PROPERTIES`] into `merged`
/// after a property fold.
///
/// A key absent on the into-note is removed from `merged` rather than left as
/// the from-note's value: a record that carried no owner-established value
/// must not acquire one by being merged into. That applies to grouping as much
/// as to attribution — a note with no `thread_id` must not join a conversation
/// because another note was folded into it.
///
/// A fold can also yield a value that is not an object at all: `merge_json`
/// applies a non-object `from` directly under `PreferFrom`, replacing the
/// into-note's whole object with a scalar. A scalar cannot carry the
/// owner-established keys, so there is nothing to restore them into and they
/// would be erased. The into-note's properties are kept instead — the scalar
/// contributes no key that could coexist with them, so nothing the fold
/// intended is lost.
///
/// This function only restores values; callers that need to report how many
/// properties genuinely survived a merge should diff the final result
/// against the into-note's pre-merge properties (see
/// [`count_new_property_keys`]) rather than try to track the restoration as
/// a correction to the fold's own count — a nested owner-established value
/// (an object) makes that correction ill-defined, since the fold's flat
/// "keys contributed" number cannot express a partial reversal of a nested
/// contribution.
pub(crate) fn preserve_owner_established_properties(
    into: &Option<Value>,
    merged: &mut Option<Value>,
) {
    preserve_property_keys(OWNER_ESTABLISHED_PROPERTIES, into, merged);
}

pub(super) fn preserve_property_keys(
    keys: &[&str],
    into: &Option<Value>,
    merged: &mut Option<Value>,
) {
    if !matches!(merged, Some(Value::Object(_))) {
        let Some(Value::Object(into_map)) = into else {
            return;
        };
        let owned_on_into = keys.iter().any(|key| into_map.contains_key(*key));
        if owned_on_into {
            *merged = into.clone();
        }
        return;
    }
    let Some(Value::Object(merged_map)) = merged.as_mut() else {
        return;
    };
    let into_map = match into {
        Some(Value::Object(m)) => Some(m),
        _ => None,
    };
    for key in keys {
        match into_map.and_then(|m| m.get(*key)) {
            Some(value) => {
                // Already present on `into` — restore it verbatim.
                merged_map.insert((*key).to_string(), value.clone());
            }
            None => {
                // Absent from `into` — a value here came from `from` and
                // must not survive the merge.
                merged_map.remove(*key);
            }
        }
    }
}

/// Count properties present in `final_value` that are new relative to
/// `original` — the same "did this key actually get added" question
/// [`merge_json`]'s fold answers, but computed from what the record finally
/// holds rather than carried forward through the fold-then-restore pipeline.
///
/// A key present in both `original` and `final_value` is never counted, even
/// when its value changed — this matches `merge_json`'s own rule that an
/// overwrite of a key already present on `into` is not a merged addition.
/// Nested objects recurse only when the key exists on both sides (mirroring
/// `merge_json`'s `Union` recursion); a key that is wholly new at some level
/// counts once for that level, not once per leaf beneath it.
pub(crate) fn count_new_property_keys(
    original: Option<&Value>,
    final_value: Option<&Value>,
    strategy: EntityDedupMergePolicy,
) -> usize {
    match (original, final_value) {
        (_, None) => 0,
        (None, Some(Value::Object(map))) => map.len(),
        (None, Some(_)) => 1,
        (Some(Value::Object(orig_map)), Some(Value::Object(final_map))) => {
            count_new_keys_within_object(orig_map, final_map, strategy)
        }
        // The record ended up holding an object where it previously held
        // something else. `merge_json` scores that replacement as ONE
        // contribution however many keys the new object carries, and this arm
        // keeps that rule rather than counting the keys — the alternative
        // silently changes `properties_merged` for ordinary notes, which never
        // enter the restoration path and were being reported correctly by the
        // fold. The rule here is: an empty final object has no contribution
        // left to report, whatever emptied it — restoration removing every
        // owner-established key is one way that happens, but an ordinary
        // `PreferFrom` replacement with an empty object reaches this same arm.
        (Some(_), Some(Value::Object(final_map))) => usize::from(!final_map.is_empty()),
        // Whole-value replacement by a non-object. `merge_json` scores a
        // `PreferFrom` fold that replaces one properties value with a
        // differently-shaped one as a single contribution, and that is the right
        // answer: what the record now holds came from the from-note. A bare 0
        // here would under-report every such replacement, including on note
        // kinds that have no owner-established properties and never enter the
        // restoration path at all. Equal values mean nothing was contributed,
        // which is the `properties: Some(a)` merged with `properties: None`
        // case.
        (Some(orig), Some(final_val)) => usize::from(orig != final_val),
    }
}

/// Per-key counting inside a properties object.
///
/// Deliberately NOT the same rule as the top level: within an object, a key that
/// already exists and is merely overwritten counts 0, matching `merge_json`'s
/// rule that only keys absent from the into-note are counted as added.
///
/// Recursion is STRATEGY-AWARE, and it has to be, because `merge_json` only
/// descends into a same-named nested object under [`Union`]. Under `PreferFrom`
/// an existing top-level key is replaced wholesale, and under `PreferInto` it is
/// kept wholesale; in neither case is anything merged *beneath* that key, so
/// descending here would count a nested value that the fold never treated as a
/// separate contribution. Counting `{"meta":{"old":1}}` merged with
/// `{"meta":{"new":2}}` under `PreferFrom` as 1 is exactly that mistake — one
/// existing property was replaced, none was added.
///
/// [`Union`]: EntityDedupMergePolicy::Union
fn count_new_keys_within_object(
    orig_map: &serde_json::Map<String, Value>,
    final_map: &serde_json::Map<String, Value>,
    strategy: EntityDedupMergePolicy,
) -> usize {
    final_map
        .iter()
        .map(|(key, value)| match orig_map.get(key) {
            None => 1,
            Some(Value::Object(nested_orig))
                if matches!(strategy, EntityDedupMergePolicy::Union) =>
            {
                match value {
                    Value::Object(nested_final) => {
                        count_new_keys_within_object(nested_orig, nested_final, strategy)
                    }
                    _ => 0,
                }
            }
            Some(_) => 0,
        })
        .sum()
}

/// Merge two property objects. Returns (merged, count_of_fields_from_from_that_were_added).
/// `pub(crate)` so `crate::atomic_prepare` can reuse this exact properties-merge
/// semantics when building an `update` write plan's row statement, matching
/// `update_entity`/`update_note`'s own patch behavior byte-for-byte.
pub(crate) fn merge_properties(
    into: &Option<Value>,
    from: &Option<Value>,
    strategy: EntityDedupMergePolicy,
) -> (Option<Value>, usize) {
    match (into, from) {
        (None, None) => (None, 0),
        (Some(a), None) => (Some(a.clone()), 0),
        (None, Some(b)) => {
            let count = if let Value::Object(m) = b { m.len() } else { 1 };
            (Some(b.clone()), count)
        }
        (Some(into_val), Some(from_val)) => {
            let (merged, added) = merge_json(into_val, from_val, strategy);
            (Some(merged), added)
        }
    }
}

/// Compare note-update values using the semantics exposed by note readers.
/// `serde_json::Value` already compares objects without depending on insertion
/// order; the top-level `properties.tags` array is compared as an
/// order-independent multiset because readers treat it as a set while
/// preserving duplicate entries as a meaningful representation change.
pub(super) fn note_update_values_equal(left: &Option<Value>, right: &Option<Value>) -> bool {
    fn equal(left: &Value, right: &Value, is_tags_field: bool, is_properties_object: bool) -> bool {
        match (left, right) {
            (Value::Object(a), Value::Object(b)) => {
                a.len() == b.len()
                    && a.iter().all(|(key, value)| {
                        b.get(key).is_some_and(|other| {
                            equal(value, other, is_properties_object && key == "tags", false)
                        })
                    })
            }
            (Value::Array(a), Value::Array(b)) if is_tags_field => {
                if a.len() != b.len() {
                    return false;
                }
                let mut left = a.iter().map(Value::to_string).collect::<Vec<_>>();
                let mut right = b.iter().map(Value::to_string).collect::<Vec<_>>();
                left.sort_unstable();
                right.sort_unstable();
                left == right
            }
            (Value::Array(a), Value::Array(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .zip(b)
                        .all(|(left, right)| equal(left, right, false, false))
            }
            _ => left == right,
        }
    }

    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => equal(left, right, false, true),
        _ => false,
    }
}

/// Deep-merge two JSON values per strategy. Returns (merged, keys_contributed_by_from).
pub(super) fn merge_json(
    into: &Value,
    from: &Value,
    strategy: EntityDedupMergePolicy,
) -> (Value, usize) {
    match (into, from, strategy) {
        (Value::Object(a), Value::Object(b), EntityDedupMergePolicy::Union) => {
            let mut result = a.clone();
            let mut added = 0usize;
            for (k, v_from) in b {
                if let Some(v_into) = a.get(k) {
                    let (merged, sub_added) =
                        merge_json(v_into, v_from, EntityDedupMergePolicy::Union);
                    result.insert(k.clone(), merged);
                    added += sub_added;
                } else {
                    result.insert(k.clone(), v_from.clone());
                    added += 1;
                }
            }
            (Value::Object(result), added)
        }
        (Value::Object(a), Value::Object(b), EntityDedupMergePolicy::PreferInto) => {
            let mut result = a.clone();
            let mut added = 0usize;
            for (k, v) in b {
                if !a.contains_key(k) {
                    result.insert(k.clone(), v.clone());
                    added += 1;
                }
            }
            (Value::Object(result), added)
        }
        (Value::Object(a), Value::Object(b), EntityDedupMergePolicy::PreferFrom) => {
            let mut result = a.clone();
            let mut added = 0usize;
            for (k, v) in b {
                result.insert(k.clone(), v.clone());
                if !a.contains_key(k) {
                    added += 1;
                }
            }
            (Value::Object(result), added)
        }
        // Non-object scalars: apply strategy directly.
        (_into_val, from_val, EntityDedupMergePolicy::PreferFrom) => (from_val.clone(), 1),
        _ => (into.clone(), 0),
    }
}

/// `pub(crate)` so `crate::atomic_prepare::prepare_merge` can reuse this for
/// atomic/non-atomic parity.
pub(crate) fn union_tags(into: &[String], from: &[String]) -> (Vec<String>, usize) {
    let mut seen: HashSet<&str> = into.iter().map(|s| s.as_str()).collect();
    let mut result: Vec<String> = into.to_vec();
    let mut added = 0usize;
    for tag in from {
        if seen.insert(tag.as_str()) {
            result.push(tag.clone());
            added += 1;
        }
    }
    (result, added)
}

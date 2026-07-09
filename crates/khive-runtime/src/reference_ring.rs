//! Recently-referenced ring: a bounded, per-`(namespace, actor)` cache of ids
//! this actor recently touched by name, held in daemon-warm memory only.
//!
//! No schema, no persistence, no migration — a daemon restart empties it, and
//! that is fine: its miss path is the hybrid-search fallback in
//! `reference_resolution::resolve_reference`. Admission is gated by the
//! dispatch boundary (`pack.rs::dispatch_with_identity`) under a strict rule:
//! only by-id touches (create/get/update/delete/merge/link) admit an id.
//! `search`/`list` result-sets never enter the ring — the anaphora signal is
//! the sparsity, so admitting every search hit would drown "the old record"
//! in noise.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;
use uuid::Uuid;

/// Default ring size per `(namespace, actor)` key.
pub const DEFAULT_RING_CAPACITY: usize = 64;

/// Default eviction age.
pub const DEFAULT_RING_TTL: Duration = Duration::from_secs(30 * 60);

/// Maximum distinct `(namespace, actor)` keys held at once. The per-key ring
/// is capacity/TTL-bounded (above), but the outer map itself has no such
/// bound by default — a daemon serving many transient actor ids needs an
/// eviction rule for the map too, or it grows without limit. LRU by
/// most-recent touch, sized generously above the 64-entry inner bound since
/// this is the actor-fanout axis, not the per-actor recency axis.
pub const DEFAULT_MAX_OUTER_KEYS: usize = 4096;

/// One admitted id: the id itself, a best-effort display name (drawn from the
/// dispatch result JSON — never a fresh fetch, to keep admission cheap on the
/// Tier-1 latency budget), and the instant it was touched.
#[derive(Clone, Debug)]
pub struct RingEntry {
    pub id: Uuid,
    pub name: Option<String>,
    pub touched_at: Instant,
}

type RingKey = (String, String);

#[derive(Default)]
struct RingState {
    rings: HashMap<RingKey, VecDeque<RingEntry>>,
}

/// Daemon-warm, actor-scoped recently-referenced ring (ADR draft "unified-verb" Slice 1).
///
/// Privacy is structural: rings are keyed by `(namespace, actor)` and a
/// snapshot for one actor never observes another actor's entries, even
/// within the same namespace.
pub struct ReferenceRing {
    state: Mutex<RingState>,
    capacity: usize,
    ttl: Duration,
    max_outer_keys: usize,
}

impl Default for ReferenceRing {
    fn default() -> Self {
        Self::new()
    }
}

impl ReferenceRing {
    pub fn new() -> Self {
        Self::with_bounds(DEFAULT_RING_CAPACITY, DEFAULT_RING_TTL)
    }

    pub fn with_bounds(capacity: usize, ttl: Duration) -> Self {
        Self::with_bounds_and_outer_limit(capacity, ttl, DEFAULT_MAX_OUTER_KEYS)
    }

    pub fn with_bounds_and_outer_limit(
        capacity: usize,
        ttl: Duration,
        max_outer_keys: usize,
    ) -> Self {
        Self {
            state: Mutex::new(RingState::default()),
            capacity,
            ttl,
            max_outer_keys,
        }
    }

    fn key(namespace: &str, actor: &str) -> RingKey {
        (namespace.to_owned(), actor.to_owned())
    }

    /// Lock the shared state, recovering from mutex poisoning instead of
    /// panicking. Admission/lookup here is best-effort daemon-warm cache
    /// maintenance riding on the back of an already-successful dispatch — a
    /// panic in some unrelated ring-holding call must never turn a
    /// successful op's return into a panic for every future caller. On
    /// poison, log once at warn and take the inner (possibly
    /// mid-mutation-but-structurally-valid, since std collections don't
    /// leave torn state on panic) state and carry on.
    fn lock_state(&self) -> std::sync::MutexGuard<'_, RingState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::warn!(
                    "reference ring mutex poisoned by a prior panic; recovering inner state \
                     (ring admission/lookup is best-effort and must never fail a dispatch)"
                );
                poisoned.into_inner()
            }
        }
    }

    /// Evict entries older than `ttl` from the front of `ring` (front = oldest,
    /// since admission always pushes to the back with the current instant).
    fn evict_stale(ring: &mut VecDeque<RingEntry>, ttl: Duration, now: Instant) {
        while let Some(front) = ring.front() {
            if now.duration_since(front.touched_at) > ttl {
                ring.pop_front();
            } else {
                break;
            }
        }
    }

    /// Drop outer-map keys whose ring is empty or fully aged out, then, if
    /// still over `max_outer_keys`, evict the globally least-recently-touched
    /// surviving keys (by each key's newest entry) until back within budget.
    /// `exempt` (the key that triggered this admission) is never evicted by
    /// the budget pass — admitting an id must never immediately evict the
    /// ring it was just admitted to.
    fn prune_outer_map(
        rings: &mut HashMap<RingKey, VecDeque<RingEntry>>,
        ttl: Duration,
        max_outer_keys: usize,
        now: Instant,
        exempt: &RingKey,
    ) {
        rings.retain(|_, ring| {
            Self::evict_stale(ring, ttl, now);
            !ring.is_empty()
        });
        if rings.len() <= max_outer_keys {
            return;
        }
        let mut by_recency: Vec<(RingKey, Instant)> = rings
            .iter()
            .filter(|(k, _)| *k != exempt)
            .filter_map(|(k, ring)| ring.back().map(|e| (k.clone(), e.touched_at)))
            .collect();
        by_recency.sort_by_key(|(_, touched_at)| *touched_at);
        let overflow = rings.len().saturating_sub(max_outer_keys);
        for (key, _) in by_recency.into_iter().take(overflow) {
            rings.remove(&key);
        }
    }

    /// Admit `id` into the `(namespace, actor)` ring, touching it to "now".
    ///
    /// Re-admitting an id already present moves it to the back (most-recent)
    /// instead of duplicating the entry. Eviction is size-or-age: stale
    /// entries are dropped first, then the oldest surviving entry is dropped
    /// if the ring is still over capacity. The outer `(namespace, actor)` map
    /// is pruned on every admission (see `prune_outer_map`) so a daemon
    /// serving many transient actor ids never grows it without bound.
    pub fn admit(&self, namespace: &str, actor: &str, id: Uuid, name: Option<String>) {
        let now = Instant::now();
        let mut state = self.lock_state();
        let key = Self::key(namespace, actor);
        {
            let ring = state.rings.entry(key.clone()).or_default();
            Self::evict_stale(ring, self.ttl, now);
            ring.retain(|e| e.id != id);
            ring.push_back(RingEntry {
                id,
                name,
                touched_at: now,
            });
            while ring.len() > self.capacity {
                ring.pop_front();
            }
        }
        Self::prune_outer_map(&mut state.rings, self.ttl, self.max_outer_keys, now, &key);
    }

    /// Snapshot the live (non-stale) entries for `(namespace, actor)`,
    /// most-recently-touched first. Returns an empty vec for an unknown or
    /// empty key — never an error, since a ring miss is always a legitimate
    /// state (fresh session, restarted daemon, cross-actor query).
    ///
    /// Also runs `prune_outer_map` over the WHOLE outer map, not just the
    /// queried key (review r2 finding, 2026-07-09): a daemon that only ever
    /// reads (never admits) would otherwise never prune any OTHER actor's
    /// stale key — `admit` was the sole `prune_outer_map` call site, so a
    /// read-only period left every unqueried stale key sitting in the map
    /// forever. Snapshotting now sweeps the whole map on every read, same as
    /// every write.
    pub fn snapshot(&self, namespace: &str, actor: &str) -> Vec<RingEntry> {
        let now = Instant::now();
        let mut state = self.lock_state();
        let key = Self::key(namespace, actor);
        let snap: Vec<RingEntry> = match state.rings.get_mut(&key) {
            Some(ring) => {
                Self::evict_stale(ring, self.ttl, now);
                ring.iter().rev().cloned().collect()
            }
            None => Vec::new(),
        };
        Self::prune_outer_map(&mut state.rings, self.ttl, self.max_outer_keys, now, &key);
        snap
    }
}

/// Display name extracted from a dispatch result's `name` field. S1's ring
/// contract is entity ids only (see `substrate_admits_as_entity`), so there
/// is no note-content fallback here — a note is never admitted in the first
/// place, and using its `content` as a ring name would let free text stand
/// in for an entity's actual name.
fn display_name(result: &Value) -> Option<String> {
    let name = result.get("name").and_then(Value::as_str)?;
    let trimmed = name.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The nine closed entity kinds (ADR-001 base 8 + ADR-048 `resource`).
/// Duplicated here (rather than depending on khive-pack-kg's vocab, which
/// would invert the crate dependency direction) because this check only
/// needs the closed set's membership, not the pack's full vocabulary.
const ENTITY_KINDS: [&str; 9] = [
    "concept", "document", "dataset", "project", "person", "org", "artifact", "service", "resource",
];

fn is_entity_kind_value(v: &str) -> bool {
    v == "entity" || ENTITY_KINDS.contains(&v)
}

/// Whether a `create`/`get`/`update`/`delete` result JSON denotes an entity
/// (S1's ring contract: entity ids only, plus `link` endpoints — see module
/// docs). No extra storage read: the discriminator is read off the shape
/// khive-pack-kg's handlers already return.
///
/// - `edge`/`event` results carry an explicit top-level `"kind": "edge"` /
///   `"kind": "event"` (injected by `flatten_get_result`) — never entities.
/// - `create`/`get`/`update` on an entity or note return the raw storage
///   record: entities always serialize an `entity_type` key (even as
///   `null`), notes always serialize a `content` key — the two shapes never
///   overlap, so key presence alone is a reliable substrate discriminator.
/// - `delete` returns a synthetic `{deleted, id, kind}` summary carrying
///   neither field; its `kind` is the caller-supplied request param verbatim
///   (may be the generic `"entity"`/`"note"` keyword, a specific closed
///   entity kind, a specific note kind, or absent/`null` when the caller let
///   `delete` infer it). Absent or ambiguous `kind` never admits — a delete
///   admission is a nice-to-have, not required for correctness, and S1's
///   "never mutates, never guesses" bias makes skipping always the safe
///   choice.
fn substrate_admits_as_entity(obj: &serde_json::Map<String, Value>) -> bool {
    if matches!(
        obj.get("kind").and_then(Value::as_str),
        Some("edge") | Some("event")
    ) {
        return false;
    }
    if obj.contains_key("content") {
        return false;
    }
    if obj.contains_key("entity_type") {
        return true;
    }
    obj.get("kind")
        .and_then(Value::as_str)
        .is_some_and(is_entity_kind_value)
}

/// Compute the `(id, name)` pairs a successful `verb` dispatch admits to the
/// ring, from its already-serialized JSON `result` alone — no extra storage
/// reads, so admission stays cheap on the Tier-1 latency budget.
///
/// Strict admission rule (gate condition, 2026-07-09): only singleton by-id
/// touches admit — `create`, `get`, `update`, `delete`, `merge`, and `link`
/// (both endpoints). Bulk shapes (`items=[...]`, `links=[...]`) are
/// identifiable by an `attempted` count in their response and are excluded
/// from S1's scope: they name multiple ids, not the one-caller-named-id
/// semantic the ring exists to serve. `search`/`list` never reach this
/// function — they are not in the verb match below.
pub(crate) fn ring_admissions_for(verb: &str, result: &Value) -> Vec<(Uuid, Option<String>)> {
    let Some(obj) = result.as_object() else {
        return Vec::new();
    };
    if obj.contains_key("attempted") {
        return Vec::new();
    }
    let parse_id = |key: &str| -> Option<Uuid> {
        obj.get(key)
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
    };
    match verb {
        "create" | "get" | "update" | "delete" => {
            if !substrate_admits_as_entity(obj) {
                return Vec::new();
            }
            match parse_id("id") {
                Some(id) => vec![(id, display_name(result))],
                None => Vec::new(),
            }
        }
        // merge is entity-only by construction (v0.1 scope, ADR-046) — no
        // substrate check needed; `MergeSummary` carries no `kind` field to
        // check even if one were wanted.
        "merge" => match parse_id("kept_id") {
            Some(id) => vec![(id, None)],
            None => Vec::new(),
        },
        "link" => {
            let mut out = Vec::new();
            if let Some(id) = parse_id("source_id") {
                out.push((id, None));
            }
            if let Some(id) = parse_id("target_id") {
                out.push((id, None));
            }
            out
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn admits_by_id_ops_and_extracts_name() {
        let ring = ReferenceRing::new();
        ring.admit("local", "actor:a", Uuid::nil(), Some("Alpha".into()));
        let snap = ring.snapshot("local", "actor:a");
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, Uuid::nil());
        assert_eq!(snap[0].name.as_deref(), Some("Alpha"));
    }

    #[test]
    fn ring_admissions_for_search_and_list_is_empty() {
        let result = json!([{"id": Uuid::nil().to_string(), "name": "hit"}]);
        assert!(ring_admissions_for("search", &result).is_empty());
        assert!(ring_admissions_for("list", &result).is_empty());
    }

    #[test]
    fn ring_admissions_for_get_extracts_id_and_name() {
        let id = Uuid::new_v4();
        // Real entity get/create/update responses always carry `entity_type`
        // (even as `null`) — that key's presence is the entity-substrate
        // discriminator `substrate_admits_as_entity` checks for.
        let result = json!({"id": id.to_string(), "name": "Concept", "entity_type": null});
        let admissions = ring_admissions_for("get", &result);
        assert_eq!(admissions, vec![(id, Some("Concept".to_string()))]);
    }

    #[test]
    fn ring_admissions_for_note_result_is_empty() {
        let id = Uuid::new_v4();
        // Real note get/create/update responses carry `content`, never
        // `entity_type` — S1's ring contract is entity ids only.
        let result = json!({"id": id.to_string(), "name": "a note", "content": "body text"});
        assert!(ring_admissions_for("create", &result).is_empty());
        assert!(ring_admissions_for("get", &result).is_empty());
        assert!(ring_admissions_for("update", &result).is_empty());
        assert!(ring_admissions_for("delete", &result).is_empty());
    }

    #[test]
    fn ring_admissions_for_edge_and_event_kind_is_empty() {
        let id = Uuid::new_v4();
        let edge_result = json!({"id": id.to_string(), "kind": "edge"});
        assert!(ring_admissions_for("get", &edge_result).is_empty());
        let event_result = json!({"id": id.to_string(), "kind": "event"});
        assert!(ring_admissions_for("get", &event_result).is_empty());
    }

    #[test]
    fn ring_admissions_for_delete_uses_echoed_kind_param() {
        let id = Uuid::new_v4();
        // delete's synthetic summary has neither `content` nor `entity_type`;
        // it falls back to the caller-echoed `kind` param.
        let entity_delete = json!({"deleted": true, "id": id.to_string(), "kind": "concept"});
        assert_eq!(
            ring_admissions_for("delete", &entity_delete),
            vec![(id, None)]
        );
        let generic_entity_delete =
            json!({"deleted": true, "id": id.to_string(), "kind": "entity"});
        assert_eq!(
            ring_admissions_for("delete", &generic_entity_delete),
            vec![(id, None)]
        );
        let note_delete = json!({"deleted": true, "id": id.to_string(), "kind": "observation"});
        assert!(ring_admissions_for("delete", &note_delete).is_empty());
        let unspecified_delete = json!({"deleted": true, "id": id.to_string(), "kind": null});
        assert!(ring_admissions_for("delete", &unspecified_delete).is_empty());
    }

    #[test]
    fn display_name_never_falls_back_to_content() {
        let result = json!({"id": Uuid::new_v4().to_string(), "content": "some note body"});
        assert_eq!(display_name(&result), None);
    }

    #[test]
    fn ring_admissions_for_link_extracts_both_endpoints() {
        let source = Uuid::new_v4();
        let target = Uuid::new_v4();
        let result = json!({
            "id": Uuid::new_v4().to_string(),
            "source_id": source.to_string(),
            "target_id": target.to_string(),
        });
        let admissions = ring_admissions_for("link", &result);
        assert_eq!(admissions, vec![(source, None), (target, None)]);
    }

    #[test]
    fn ring_admissions_for_merge_uses_kept_id() {
        let kept = Uuid::new_v4();
        let removed = Uuid::new_v4();
        let result = json!({"kept_id": kept.to_string(), "removed_id": removed.to_string()});
        let admissions = ring_admissions_for("merge", &result);
        assert_eq!(admissions, vec![(kept, None)]);
    }

    #[test]
    fn ring_admissions_for_bulk_shapes_is_empty() {
        let bulk_create = json!({"attempted": 3, "created": 3});
        assert!(ring_admissions_for("create", &bulk_create).is_empty());
        let bulk_link = json!({"attempted": 2, "created": 2, "skipped": 0, "failed": 0});
        assert!(ring_admissions_for("link", &bulk_link).is_empty());
    }

    #[test]
    fn admission_bounds_by_size() {
        let ring = ReferenceRing::with_bounds(3, DEFAULT_RING_TTL);
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        for id in &ids {
            ring.admit("local", "actor:a", *id, None);
        }
        let snap = ring.snapshot("local", "actor:a");
        assert_eq!(snap.len(), 3);
        // Most-recently-touched first; the two oldest (ids[0], ids[1]) were evicted.
        assert_eq!(snap[0].id, ids[4]);
        assert_eq!(snap[1].id, ids[3]);
        assert_eq!(snap[2].id, ids[2]);
    }

    #[test]
    fn admission_bounds_by_age() {
        let ring = ReferenceRing::with_bounds(64, Duration::from_millis(20));
        let old = Uuid::new_v4();
        ring.admit("local", "actor:a", old, None);
        std::thread::sleep(Duration::from_millis(40));
        let fresh = Uuid::new_v4();
        ring.admit("local", "actor:a", fresh, None);
        let snap = ring.snapshot("local", "actor:a");
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, fresh);
    }

    #[test]
    fn actor_isolation_never_crosses_boundary() {
        let ring = ReferenceRing::new();
        let id_a = Uuid::new_v4();
        ring.admit("local", "actor:a", id_a, Some("A-only".into()));
        let snap_b = ring.snapshot("local", "actor:b");
        assert!(snap_b.is_empty(), "actor b must never see actor a's ring");
        let snap_a = ring.snapshot("local", "actor:a");
        assert_eq!(snap_a.len(), 1);
    }

    #[test]
    fn namespace_isolation_is_independent_of_actor_isolation() {
        let ring = ReferenceRing::new();
        let id = Uuid::new_v4();
        ring.admit("tenant-a", "actor:a", id, None);
        assert!(ring.snapshot("tenant-b", "actor:a").is_empty());
        assert_eq!(ring.snapshot("tenant-a", "actor:a").len(), 1);
    }

    #[test]
    fn re_admitting_an_id_moves_it_to_most_recent_without_duplicating() {
        let ring = ReferenceRing::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        ring.admit("local", "actor:a", a, Some("A".into()));
        ring.admit("local", "actor:a", b, Some("B".into()));
        ring.admit("local", "actor:a", a, Some("A-renamed".into()));
        let snap = ring.snapshot("local", "actor:a");
        assert_eq!(snap.len(), 2, "re-admission must not duplicate the entry");
        assert_eq!(snap[0].id, a, "re-admitted id must be most-recent");
        assert_eq!(snap[0].name.as_deref(), Some("A-renamed"));
    }

    #[test]
    fn snapshot_prunes_a_key_that_ages_out_entirely() {
        let ring = ReferenceRing::with_bounds(64, Duration::from_millis(20));
        ring.admit("local", "actor:a", Uuid::new_v4(), None);
        std::thread::sleep(Duration::from_millis(40));
        // The read itself must observe (and clean up) the now-fully-stale key.
        assert!(ring.snapshot("local", "actor:a").is_empty());
        let state = ring.state.lock().unwrap();
        assert!(
            !state
                .rings
                .contains_key(&("local".to_string(), "actor:a".to_string())),
            "a key whose ring emptied via TTL eviction must not linger in the outer map"
        );
    }

    /// Regression (review r2 finding, 2026-07-09): `snapshot` must sweep the
    /// WHOLE outer map, not just the queried key — otherwise a read-only
    /// daemon (one that only ever calls `snapshot`, never `admit`) never
    /// prunes any OTHER actor's stale key at all. Two actors age out; only
    /// `actor:queried` is snapshotted, but `actor:other`'s stale key must be
    /// removed too.
    #[test]
    fn snapshot_prunes_other_stale_keys_it_did_not_query() {
        let ring = ReferenceRing::with_bounds(64, Duration::from_millis(20));
        ring.admit("local", "actor:queried", Uuid::new_v4(), None);
        ring.admit("local", "actor:other", Uuid::new_v4(), None);
        std::thread::sleep(Duration::from_millis(40));

        // Only `actor:queried` is ever snapshotted directly.
        assert!(ring.snapshot("local", "actor:queried").is_empty());

        let state = ring.state.lock().unwrap();
        assert!(
            !state
                .rings
                .contains_key(&("local".to_string(), "actor:other".to_string())),
            "snapshotting one actor must also prune OTHER actors' fully-stale keys, \
             not just the one queried"
        );
    }

    #[test]
    fn outer_map_evicts_least_recently_touched_keys_over_budget() {
        let ring = ReferenceRing::with_bounds_and_outer_limit(64, DEFAULT_RING_TTL, 3);
        // Admit four distinct actors in order; the outer-key budget is 3, so
        // the least-recently-touched one (actor:0) must be evicted once the
        // fourth actor is admitted.
        for i in 0..4 {
            ring.admit(
                "local",
                &format!("actor:{i}"),
                Uuid::new_v4(),
                Some(format!("actor {i}")),
            );
        }
        assert!(
            ring.snapshot("local", "actor:0").is_empty(),
            "the least-recently-touched key must be evicted once the outer-key budget is exceeded"
        );
        for i in 1..4 {
            assert!(
                !ring.snapshot("local", &format!("actor:{i}")).is_empty(),
                "actor:{i} must survive the budget eviction"
            );
        }
    }

    #[test]
    fn outer_map_budget_eviction_never_evicts_the_key_just_admitted() {
        let ring = ReferenceRing::with_bounds_and_outer_limit(64, DEFAULT_RING_TTL, 1);
        ring.admit("local", "actor:a", Uuid::new_v4(), None);
        // Admitting actor:b pushes the outer map to 2 keys against a budget
        // of 1; the key that triggered this admission (actor:b) must survive
        // even though it is, by construction, also the most-recently-touched.
        ring.admit("local", "actor:b", Uuid::new_v4(), None);
        assert!(!ring.snapshot("local", "actor:b").is_empty());
    }

    /// A prior panic while holding the ring's mutex must never turn a
    /// later, unrelated `admit`/`snapshot` call into a panic too — admission
    /// is best-effort cache maintenance riding on an already-successful
    /// dispatch (finding 4, 2026-07-09 fix round).
    #[test]
    fn admit_and_snapshot_recover_from_poisoned_mutex() {
        let ring = std::sync::Arc::new(ReferenceRing::new());
        let poison_ring = ring.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poison_ring.state.lock().unwrap();
            panic!("deliberately poisoning the reference ring mutex for the recovery test");
        })
        .join();

        // Both calls must complete normally (not panic) despite the poison.
        ring.admit(
            "local",
            "actor:a",
            Uuid::new_v4(),
            Some("post-poison".into()),
        );
        let snap = ring.snapshot("local", "actor:a");
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].name.as_deref(), Some("post-poison"));
    }
}

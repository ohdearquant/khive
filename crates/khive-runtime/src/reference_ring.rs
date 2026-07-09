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
        Self {
            state: Mutex::new(RingState::default()),
            capacity,
            ttl,
        }
    }

    fn key(namespace: &str, actor: &str) -> RingKey {
        (namespace.to_owned(), actor.to_owned())
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

    /// Admit `id` into the `(namespace, actor)` ring, touching it to "now".
    ///
    /// Re-admitting an id already present moves it to the back (most-recent)
    /// instead of duplicating the entry. Eviction is size-or-age: stale
    /// entries are dropped first, then the oldest surviving entry is dropped
    /// if the ring is still over capacity.
    pub fn admit(&self, namespace: &str, actor: &str, id: Uuid, name: Option<String>) {
        let now = Instant::now();
        let mut state = self.state.lock().expect("reference ring mutex poisoned");
        let ring = state.rings.entry(Self::key(namespace, actor)).or_default();
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

    /// Snapshot the live (non-stale) entries for `(namespace, actor)`,
    /// most-recently-touched first. Returns an empty vec for an unknown or
    /// empty key — never an error, since a ring miss is always a legitimate
    /// state (fresh session, restarted daemon, cross-actor query).
    pub fn snapshot(&self, namespace: &str, actor: &str) -> Vec<RingEntry> {
        let now = Instant::now();
        let mut state = self.state.lock().expect("reference ring mutex poisoned");
        let Some(ring) = state.rings.get_mut(&Self::key(namespace, actor)) else {
            return Vec::new();
        };
        Self::evict_stale(ring, self.ttl, now);
        ring.iter().rev().cloned().collect()
    }
}

/// Best-effort display name extracted from a dispatch result: prefer `name`
/// (entities), fall back to a `content` snippet (notes), else `None`.
fn display_name(result: &Value) -> Option<String> {
    if let Some(name) = result.get("name").and_then(Value::as_str) {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    if let Some(content) = result.get("content").and_then(Value::as_str) {
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.chars().take(60).collect());
        }
    }
    None
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
        "create" | "get" | "update" | "delete" => match parse_id("id") {
            Some(id) => vec![(id, display_name(result))],
            None => Vec::new(),
        },
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
        let result = json!({"id": id.to_string(), "name": "Concept"});
        let admissions = ring_admissions_for("get", &result);
        assert_eq!(admissions, vec![(id, Some("Concept".to_string()))]);
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
}

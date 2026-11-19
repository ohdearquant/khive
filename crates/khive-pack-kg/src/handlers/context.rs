//! `context` verb handler (ADR-089): entity-anchored graph context in one call.
//!
//! Composes `hybrid_search` (anchor selection from a query) and
//! `neighbors_with_query` / `neighbors_with_query_directed` (bounded 1/2-hop
//! expansion) — both runtime ops are reused unchanged. Two handler-local
//! decisions fill gaps the runtime ops don't cover, documented at their call
//! sites below:
//!   1. A plain `NeighborHit` carries no `direction` field, so an effective
//!      direction of `Both` uses `neighbors_with_query_directed`, which fetches
//!      both directions in a single storage query (`UNION ALL` with a
//!      direction literal per arm) and returns each hit tagged `Out`/`In`.
//!   3. An edge endpoint is any record kind, so the neighbour walk returns
//!      notes as readily as entities, while record metadata lives in two
//!      stores. The handler hydrates entities and then the remainder from the
//!      note store rather than reading one store and dropping what it cannot
//!      find, which is what made a note neighbour disappear with nothing in
//!      the response saying it had.
//!   2. Symmetric relations (`competes_with`, `composed_with`) force
//!      `Direction::Both` inside `neighbors_with_query` regardless of the
//!      direction requested (existing op behavior) — the handler mirrors
//!      that check to avoid double-counting and tags those neighbors
//!      `"both"` rather than guessing outgoing/incoming for an undirected
//!      relation.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{
    KgNeighborRead, KhiveRuntime, MailboxView, Namespace, NamespaceToken, Resolved, RuntimeError,
    VerbRegistry,
};
use khive_storage::types::{Direction, NeighborHit, NeighborQuery, PageRequest};
use khive_storage::{EdgeRelation, Entity, EntityFilter};

use super::common::{deser, parse_direction, parse_relation, ContextParams, DoublingWindowIter};
use super::message_scope::{message_neighbor_permitted, resolve_mailbox_graph_id};
use crate::KgPack;

static CONTEXT_CALL_ID: AtomicU64 = AtomicU64::new(0);

/// What a neighbour block needs about the record it points at, independent of
/// which store holds that record.
struct NeighborMeta {
    substrate: &'static str,
    kind: String,
    name: Option<String>,
    description: Option<String>,
}

/// A note's body stands in for an entity's description, bounded so that one
/// long note cannot crowd every other neighbour out of the response budget.
const NOTE_SNIPPET_CHARS: usize = 200;

/// Truncation is by character and never by byte: a byte cut can land inside a
/// UTF-8 sequence, and the result is a panic on a note nobody thought was
/// unusual.
fn note_snippet(content: &str) -> Option<String> {
    if content.is_empty() {
        return None;
    }
    let mut out: String = content.chars().take(NOTE_SNIPPET_CHARS).collect();
    if content.chars().nth(NOTE_SNIPPET_CHARS).is_some() {
        out.push('\u{2026}');
    }
    Some(out)
}

fn context_profile_enabled(runtime: &khive_runtime::KhiveRuntime) -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let enabled = *ENABLED.get_or_init(|| std::env::var("KHIVE_CONTEXT_PROFILE").is_ok());
    runtime.record_config_locked("KHIVE_CONTEXT_PROFILE", || enabled.to_string());
    enabled
}

fn plog(call_id: u64, stage: &str, us: u128) {
    eprintln!(r#"{{"c":{call_id},"s":"{stage}","us":{us}}}"#);
}

const DEFAULT_HOPS: i64 = 1;
const MIN_HOPS: i64 = 0;
const MAX_HOPS: i64 = 2;

const DEFAULT_BUDGET: i64 = 4096;
const MIN_BUDGET: i64 = 256;
const MAX_BUDGET: i64 = 65536;

const DEFAULT_LIMIT: u32 = 5;
const MIN_LIMIT: u32 = 1;
const MAX_LIMIT: u32 = 20;

const DEFAULT_FANOUT: u32 = 10;
const MIN_FANOUT: u32 = 1;
const MAX_FANOUT: u32 = 50;

/// One expansion record — either hop-1 (`via: None`) or hop-2 (`via: Some(parent)`).
struct NeighborRecord {
    id: Uuid,
    relation: EdgeRelation,
    direction: &'static str,
    weight: f64,
    hop: u8,
    via: Option<Uuid>,
}

struct NeighborCandidate {
    id: Uuid,
    edge_id: Uuid,
    relation: EdgeRelation,
    weight: f64,
    direction: &'static str,
}

struct AnchorEdgeRecord {
    edge_id: Uuid,
    source_id: Uuid,
    target_id: Uuid,
    relation: EdgeRelation,
    weight: f64,
}

fn context_edge_endpoints(parent_id: Uuid, neighbor_id: Uuid, direction: &str) -> (Uuid, Uuid) {
    // Incoming preserves neighbor -> parent; outgoing and symmetric both use parent -> neighbor.
    if direction == "incoming" {
        (neighbor_id, parent_id)
    } else {
        (parent_id, neighbor_id)
    }
}

/// True iff every relation in the filter is symmetric. See `docs/api/context-verb.md`.
fn relations_all_symmetric(relations: Option<&[EdgeRelation]>) -> bool {
    match relations {
        None => false,
        Some([]) => false,
        Some(rels) => rels
            .iter()
            .all(|r| matches!(r, EdgeRelation::CompetesWith | EdgeRelation::ComposedWith)),
    }
}

struct ContextNeighborRead<'a> {
    direction: &'a Direction,
    relations: Option<&'a [EdgeRelation]>,
    fanout: u32,
}

/// Fetch one namespace's neighbor window without replacing the caller token.
async fn fetch_directed_neighbors(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    token: &NamespaceToken,
    node_id: Uuid,
    namespace: &Namespace,
    options: &ContextNeighborRead<'_>,
) -> Result<Vec<(NeighborHit, &'static str)>, RuntimeError> {
    let symmetric = relations_all_symmetric(options.relations);
    let query = NeighborQuery {
        direction: if symmetric {
            Direction::Both
        } else {
            options.direction.clone()
        },
        relations: options.relations.map(|r| r.to_vec()),
        limit: Some(options.fanout),
        min_weight: None,
    };
    if !symmetric && *options.direction == Direction::Both {
        let hits = registry
            .directed_neighbors_for_kg_read(runtime, token, node_id, query, Some(namespace))
            .await?;
        return Ok(hits
            .into_iter()
            .map(|(h, dir)| {
                let tag = if h.relation.is_symmetric() {
                    "both"
                } else if dir == Direction::Out {
                    "outgoing"
                } else {
                    "incoming"
                };
                (h, tag)
            })
            .collect());
    }
    let hits = registry
        .neighbors_for_kg_read(
            runtime,
            token,
            node_id,
            KgNeighborRead {
                query,
                after: None,
                neighbor_kinds: None,
                enrich: true,
                namespace: Some(namespace.clone()),
            },
        )
        .await?;
    let tag = if symmetric {
        "both"
    } else if *options.direction == Direction::Out {
        "outgoing"
    } else {
        "incoming"
    };
    Ok(hits
        .into_iter()
        .map(|h| {
            let tag = if h.relation.is_symmetric() {
                "both"
            } else {
                tag
            };
            (h, tag)
        })
        .collect())
}

async fn fetch_mailbox_neighbors(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    token: &NamespaceToken,
    view: &MailboxView,
    node_id: Uuid,
    options: &ContextNeighborRead<'_>,
    permitted: &mut HashMap<Uuid, bool>,
) -> Result<(Vec<NeighborCandidate>, bool), RuntimeError> {
    const SCAN_CAP: u32 = 10_000;
    let mut merged: Vec<(NeighborHit, &'static str)> = Vec::new();
    let mut scan_incomplete = false;
    for namespace in token.visible_namespaces() {
        let mut windows = DoublingWindowIter::new(options.fanout, SCAN_CAP);
        while let Some(window) = windows.next() {
            let raw = missing_neighbor_anchor_as_empty(
                fetch_directed_neighbors(
                    runtime,
                    registry,
                    token,
                    node_id,
                    namespace,
                    &ContextNeighborRead {
                        direction: options.direction,
                        relations: options.relations,
                        fanout: window,
                    },
                )
                .await,
            )?;
            let raw_count = raw.len();
            let mut admitted = Vec::with_capacity(raw_count);
            for hit in raw {
                let allowed = match permitted.get(&hit.0.node_id) {
                    Some(allowed) => *allowed,
                    None => {
                        let allowed =
                            message_neighbor_permitted(runtime, registry, token, view, &hit.0)
                                .await?;
                        permitted.insert(hit.0.node_id, allowed);
                        allowed
                    }
                };
                if allowed {
                    admitted.push(hit);
                }
            }
            if admitted.len() >= options.fanout as usize || raw_count < window as usize {
                admitted.truncate(options.fanout as usize);
                merged.extend(admitted);
                break;
            }
            if window >= SCAN_CAP {
                merged.extend(admitted);
                scan_incomplete = true;
                break;
            }
            windows.advance(window, 0, true);
        }
    }
    let direction_rank = |tag: &str| match tag {
        "outgoing" => 0,
        "incoming" => 1,
        _ => 2,
    };
    merged.sort_by_key(|(hit, tag)| (hit.node_id, hit.edge_id, direction_rank(tag)));
    merged.dedup_by_key(|(hit, tag)| (hit.node_id, hit.edge_id, direction_rank(tag)));
    merged.sort_by(compare_context_neighbors);
    Ok((
        merged
            .into_iter()
            .map(|(hit, direction)| NeighborCandidate {
                id: hit.node_id,
                edge_id: hit.edge_id,
                relation: hit.relation,
                weight: hit.weight,
                direction,
            })
            .collect(),
        scan_incomplete,
    ))
}

fn missing_neighbor_anchor_as_empty<T>(
    result: Result<Vec<T>, RuntimeError>,
) -> Result<Vec<T>, RuntimeError> {
    match result {
        Err(RuntimeError::NotFound(_)) => Ok(Vec::new()),
        other => other,
    }
}

fn compact_len(v: &Value) -> Result<usize, RuntimeError> {
    let s =
        serde_json::to_string(v).map_err(|e| RuntimeError::Internal(format!("serialize: {e}")))?;
    Ok(s.chars().count())
}

/// The two projections of one discovery step share a single admission cost.
struct ContextPair {
    neighbor: Value,
    edge: Value,
    size: usize,
}

impl ContextPair {
    fn new(neighbor: Value, edge: Value) -> Result<Self, RuntimeError> {
        let size = compact_len(&neighbor)? + compact_len(&edge)?;
        Ok(Self {
            neighbor,
            edge,
            size,
        })
    }
}

struct AnchorBlock {
    entity_json: Value,
    entity_size: usize,
    pairs: Vec<ContextPair>,
}

// Only the library unit-test target has this private, namespace-scoped barrier.
// No state or public test API is compiled into production or integration targets.
#[cfg(test)]
mod hydration_pause {
    use super::*;
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;
    use tokio::sync::oneshot;

    struct Pause {
        id: Uuid,
        entered: oneshot::Sender<()>,
        resume: oneshot::Receiver<()>,
    }

    fn pauses() -> &'static Mutex<HashMap<String, Pause>> {
        static PAUSES: OnceLock<Mutex<HashMap<String, Pause>>> = OnceLock::new();
        PAUSES.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub(super) struct Arm {
        namespace: String,
        id: Uuid,
        entered: Option<oneshot::Receiver<()>>,
        resume: Option<oneshot::Sender<()>>,
    }

    impl Arm {
        pub(super) async fn reached(&mut self) {
            tokio::time::timeout(Duration::from_secs(10), self.entered.take().unwrap())
                .await
                .expect("context expansion watchdog")
                .expect("context must reach the hydration barrier");
        }

        pub(super) fn release(&mut self) {
            if let Some(resume) = self.resume.take() {
                let _ = resume.send(());
            }
        }
    }

    impl Drop for Arm {
        fn drop(&mut self) {
            self.release();
            let mut entries = pauses().lock().expect("hydration pause lock");
            if entries
                .get(&self.namespace)
                .is_some_and(|pause| pause.id == self.id)
            {
                entries.remove(&self.namespace);
            }
        }
    }

    pub(super) fn arm(namespace: &str) -> Arm {
        let id = Uuid::new_v4();
        let (entered, reached) = oneshot::channel();
        let (resume, released) = oneshot::channel();
        let mut entries = pauses().lock().expect("hydration pause lock");
        assert!(!entries.contains_key(namespace), "one pause per namespace");
        entries.insert(
            namespace.to_owned(),
            Pause {
                id,
                entered,
                resume: released,
            },
        );
        Arm {
            namespace: namespace.to_owned(),
            id,
            entered: Some(reached),
            resume: Some(resume),
        }
    }

    pub(super) async fn wait(namespace: &str) {
        let pause = {
            pauses()
                .lock()
                .expect("hydration pause lock")
                .remove(namespace)
        };
        if let Some(pause) = pause {
            let _ = pause.entered.send(());
            // Arm drop releases the read even if the mutation/assertion panics.
            let _ = tokio::time::timeout(Duration::from_secs(10), pause.resume)
                .await
                .expect("context hydration release watchdog");
        }
    }
}

impl KgPack {
    pub(crate) async fn handle_context(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let call_id = CONTEXT_CALL_ID.fetch_add(1, Ordering::Relaxed);
        let prof = context_profile_enabled(&self.runtime);
        let mailbox_view = self
            .runtime
            .authorize_mailbox_view(token, "context", None, &params)?;
        let p: ContextParams = deser(params)?;

        let has_query = p.query.as_deref().is_some_and(|s| !s.trim().is_empty());
        let has_ids = p.entity_ids.as_ref().is_some_and(|v| !v.is_empty());
        if !has_query && !has_ids {
            return Err(RuntimeError::InvalidInput(
                "context requires at least one of `query` or `entity_ids`".into(),
            ));
        }

        let hops = p.hops.unwrap_or(DEFAULT_HOPS).clamp(MIN_HOPS, MAX_HOPS);
        let budget_effective = p
            .budget
            .unwrap_or(DEFAULT_BUDGET)
            .clamp(MIN_BUDGET, MAX_BUDGET);
        let budget = budget_effective as usize;
        let requested_limit = p.limit;
        let limit = requested_limit
            .unwrap_or(DEFAULT_LIMIT)
            .clamp(MIN_LIMIT, MAX_LIMIT);
        let fanout = p
            .fanout
            .unwrap_or(DEFAULT_FANOUT)
            .clamp(MIN_FANOUT, MAX_FANOUT);
        // Every clamped number reports back under its own name, and only when
        // the caller supplied it: a default is not a clamp.
        let clamp_reports: [(&str, Option<i64>, i64); 4] = [
            ("hops", p.hops, hops),
            ("budget", p.budget, budget_effective),
            ("limit", requested_limit.map(i64::from), i64::from(limit)),
            ("fanout", p.fanout.map(i64::from), i64::from(fanout)),
        ];
        let direction = parse_direction(p.direction.as_deref())?;
        let relations: Option<Vec<EdgeRelation>> = p
            .relations
            .as_ref()
            .map(|v| {
                v.iter()
                    .map(|s| parse_relation(s))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;

        // ---- Stage 1: anchor resolution ----
        let t0 = if prof { Some(Instant::now()) } else { None };
        let mut anchor_ids: Vec<Uuid> = Vec::new();
        let mut seen: HashSet<Uuid> = HashSet::new();
        let mut explicit_ids: Vec<Uuid> = Vec::new();
        if let Some(ids) = &p.entity_ids {
            for s in ids {
                let uuid =
                    resolve_mailbox_graph_id(&self.runtime, registry, token, &mailbox_view, s)
                        .await?;
                explicit_ids.push(uuid);
                if seen.insert(uuid) {
                    anchor_ids.push(uuid);
                }
            }
        }
        // ADR-089 §1 explicit-anchor contract; fail loudly on bad ids instead of
        // silently vanishing them in Stage 4. See docs/api/context-verb.md.
        if !explicit_ids.is_empty() {
            let mut dedup_explicit = explicit_ids.clone();
            dedup_explicit.sort_unstable();
            dedup_explicit.dedup();
            let page = self
                .runtime
                .entities(token)?
                .query_entities(
                    token.namespace().as_str(),
                    EntityFilter {
                        ids: dedup_explicit.clone(),
                        namespaces: token
                            .visible_namespace_strs()
                            .iter()
                            .map(|s| s.to_string())
                            .collect(),
                        ..EntityFilter::default()
                    },
                    PageRequest {
                        offset: 0,
                        limit: dedup_explicit.len() as u32,
                    },
                )
                .await
                .map_err(RuntimeError::Storage)?;
            let found: HashSet<Uuid> = page.items.iter().map(|e| e.id).collect();
            let missing: Vec<String> = dedup_explicit
                .iter()
                .filter(|id| !found.contains(id))
                .map(|id| id.to_string())
                .collect();
            if !missing.is_empty() {
                return Err(RuntimeError::NotFound(format!(
                    "entity_ids must name existing, visible entities; not found or not an \
                     entity: {}",
                    missing.join(", ")
                )));
            }
        }
        if let Some(t) = t0 {
            plog(call_id, "anchor_ids", t.elapsed().as_micros());
        }

        let t1 = if prof { Some(Instant::now()) } else { None };
        if has_query {
            let q = p.query.as_deref().unwrap();
            // Overfetch so entity_ids-overlap doesn't under-fill (ADR-089 §1); see
            // docs/api/context-verb.md.
            const QUERY_FILL_WINDOW_MULTIPLIER: u32 = 4;
            let fetch_n = limit
                .saturating_add(explicit_ids.len() as u32)
                .saturating_mul(QUERY_FILL_WINDOW_MULTIPLIER)
                .max(limit);
            let hits = self
                .runtime
                .hybrid_search(token, q, None, fetch_n, None, None, &[], None)
                .await?;
            let mut added = 0u32;
            for h in hits {
                if added >= limit {
                    break;
                }
                if seen.insert(h.entity_id) {
                    anchor_ids.push(h.entity_id);
                    added += 1;
                }
            }
        }
        if let Some(t) = t1 {
            plog(call_id, "anchor_search", t.elapsed().as_micros());
        }

        // ---- Stage 2: expansion ----
        let t2 = if prof { Some(Instant::now()) } else { None };
        // Anchors seed the visited set so an anchor already shown at the top
        // level never also appears inside another anchor's neighbor list.
        let mut visited: HashSet<Uuid> = anchor_ids.iter().copied().collect();
        let anchor_id_set: HashSet<Uuid> = anchor_ids.iter().copied().collect();
        let mut seen_anchor_edge_ids = HashSet::new();
        let mut anchor_edge_records = Vec::new();
        let mut per_anchor_neighbors: Vec<Vec<NeighborRecord>> =
            Vec::with_capacity(anchor_ids.len());
        let mut permitted = HashMap::new();
        let mut scan_incomplete = false;
        let neighbor_options = ContextNeighborRead {
            direction: &direction,
            relations: relations.as_deref(),
            fanout,
        };

        for &anchor in &anchor_ids {
            let mut recs: Vec<NeighborRecord> = Vec::new();
            let mut hop1_parents: Vec<Uuid> = Vec::new();

            // hops=0 means anchors only — skip expansion entirely.
            if hops >= 1 {
                let (hop1_raw, incomplete) = fetch_mailbox_neighbors(
                    &self.runtime,
                    registry,
                    token,
                    &mailbox_view,
                    anchor,
                    &neighbor_options,
                    &mut permitted,
                )
                .await?;
                scan_incomplete |= incomplete;

                for hit in hop1_raw {
                    let id = hit.id;
                    if anchor_id_set.contains(&id) {
                        if seen_anchor_edge_ids.insert(hit.edge_id) {
                            let (source_id, target_id) =
                                context_edge_endpoints(anchor, id, hit.direction);
                            anchor_edge_records.push(AnchorEdgeRecord {
                                edge_id: hit.edge_id,
                                source_id,
                                target_id,
                                relation: hit.relation,
                                weight: hit.weight,
                            });
                        }
                        continue;
                    }
                    if !visited.insert(id) {
                        continue;
                    }
                    recs.push(NeighborRecord {
                        id,
                        relation: hit.relation,
                        direction: hit.direction,
                        weight: hit.weight,
                        hop: 1,
                        via: None,
                    });
                    hop1_parents.push(id);
                }
            }

            if hops == 2 {
                let mut hop2_pool: Vec<(Uuid, Uuid, EdgeRelation, f64, &'static str)> = Vec::new();
                for parent in &hop1_parents {
                    let (hop2_raw, incomplete) = fetch_mailbox_neighbors(
                        &self.runtime,
                        registry,
                        token,
                        &mailbox_view,
                        *parent,
                        &neighbor_options,
                        &mut permitted,
                    )
                    .await?;
                    scan_incomplete |= incomplete;
                    for hit in hop2_raw {
                        hop2_pool.push((*parent, hit.id, hit.relation, hit.weight, hit.direction));
                    }
                }
                // Deterministic hop-1 stratum ordering; see docs/api/context-verb.md.
                hop2_pool.sort_by(compare_hop2_neighbors);
                for (parent, id, relation, weight, dir) in hop2_pool {
                    if !visited.insert(id) {
                        continue;
                    }
                    recs.push(NeighborRecord {
                        id,
                        relation,
                        direction: dir,
                        weight,
                        hop: 2,
                        via: Some(parent),
                    });
                }
            }

            per_anchor_neighbors.push(recs);
        }
        if let Some(t) = t2 {
            plog(call_id, "expand", t.elapsed().as_micros());
        }

        #[cfg(test)]
        hydration_pause::wait(token.namespace().as_str()).await;

        // ---- Stage 3: batch entity metadata fetch (anchors + all neighbors) ----
        let t3 = if prof { Some(Instant::now()) } else { None };
        let mut all_ids: Vec<Uuid> = anchor_ids.clone();
        for recs in &per_anchor_neighbors {
            for r in recs {
                all_ids.push(r.id);
            }
        }
        all_ids.sort_unstable();
        all_ids.dedup();

        let entity_meta: HashMap<Uuid, Entity> = if all_ids.is_empty() {
            HashMap::new()
        } else {
            let page = self
                .runtime
                .entities(token)?
                .query_entities(
                    token.namespace().as_str(),
                    EntityFilter {
                        ids: all_ids.clone(),
                        namespaces: token
                            .visible_namespace_strs()
                            .iter()
                            .map(|s| s.to_string())
                            .collect(),
                        ..EntityFilter::default()
                    },
                    PageRequest {
                        offset: 0,
                        limit: all_ids.len() as u32,
                    },
                )
                .await
                .map_err(RuntimeError::Storage)?;
            page.items.into_iter().map(|e| (e.id, e)).collect()
        };

        // The neighbour walk returns edge endpoints of every record kind, and
        // the query above answers for entities alone. Without this second
        // fetch a note neighbour missed the stage 4 lookup and was skipped
        // before `assemble_within_budget` ever saw it, so the response could
        // report an empty neighbour list beside `dropped.neighbors == 0` and
        // both halves were true. Preserve the local batch read, then resolve
        // only missing handles across pack backends.
        let visible: HashSet<String> = token
            .visible_namespace_strs()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let unresolved: Vec<Uuid> = all_ids
            .iter()
            .copied()
            .filter(|id| !entity_meta.contains_key(id))
            .collect();
        let local_notes = self
            .runtime
            .notes(token)?
            .get_notes_batch(&unresolved)
            .await
            .map_err(RuntimeError::Storage)?;
        let local_ids: HashSet<Uuid> = local_notes.iter().map(|note| note.id).collect();
        let mut note_meta: HashMap<Uuid, khive_storage::note::Note> = local_notes
            .into_iter()
            .filter(|note| {
                visible.contains(&note.namespace) && mailbox_view.permits_message_note(token, note)
            })
            .map(|note| (note.id, note))
            .collect();
        for id in unresolved.into_iter().filter(|id| !local_ids.contains(id)) {
            if let Some(Resolved::Note(note)) = registry
                .resolve_kg_read_by_id(&self.runtime, token, id, false)
                .await?
            {
                if visible.contains(&note.namespace)
                    && mailbox_view.permits_message_note(token, &note)
                {
                    note_meta.insert(id, note);
                }
            }
        }
        let meta_for = |id: &Uuid| -> Option<NeighborMeta> {
            if let Some(e) = entity_meta.get(id) {
                return Some(NeighborMeta {
                    substrate: "entity",
                    kind: e.kind.clone(),
                    name: Some(e.name.clone()),
                    description: e.description.clone(),
                });
            }
            note_meta.get(id).map(|n| NeighborMeta {
                substrate: "note",
                kind: n.kind.clone(),
                name: n.name.clone(),
                description: note_snippet(&n.content),
            })
        };
        if let Some(t) = t3 {
            plog(call_id, "record_fetch", t.elapsed().as_micros());
        }

        // ---- Stage 4: assembly with budget enforcement ----
        let t4 = if prof { Some(Instant::now()) } else { None };
        // An anchor that resolves to nothing here is the residual delete race
        // (see docs/api/context-verb.md). Anchors stay entity-only by the
        // verb's own contract, which refuses a note id up front with a named
        // error; it is the NEIGHBOURS that are substrate-free, and this
        // `continue` used to drop every one of them that was not an entity.
        let mut blocks: Vec<AnchorBlock> = Vec::with_capacity(anchor_ids.len());
        for (i, anchor) in anchor_ids.iter().enumerate() {
            let Some(e) = entity_meta.get(anchor) else {
                continue;
            };
            let entity_json = json!({
                "id": e.id.to_string(),
                "name": e.name,
                "kind": e.kind,
                "description": e.description,
                "properties": e.properties,
            });
            let entity_size = compact_len(&entity_json)?;

            let mut pairs = Vec::with_capacity(per_anchor_neighbors[i].len());
            for rec in &per_anchor_neighbors[i] {
                let Some(ne) = meta_for(&rec.id) else {
                    continue;
                };
                let parent_id = rec.via.unwrap_or(*anchor);
                let Some(parent) = meta_for(&parent_id) else {
                    // Separate scoped reads can lose a parent after discovery.
                    // Skip both projections before budgeting; retain visited ownership.
                    continue;
                };
                let nj = json!({
                    "id": rec.id.to_string(),
                    "name": ne.name,
                    "kind": ne.kind,
                    "substrate": ne.substrate,
                    "relation": rec.relation.as_str(),
                    "direction": rec.direction,
                    "weight": rec.weight,
                    "hop": rec.hop,
                    "via": rec.via.map(|v| v.to_string()),
                    "description": ne.description,
                });
                let (source_id, target_id) =
                    context_edge_endpoints(parent_id, rec.id, rec.direction);
                let source_name = if source_id == parent_id {
                    parent.name.as_deref()
                } else {
                    ne.name.as_deref()
                };
                let target_name = if target_id == parent_id {
                    parent.name.as_deref()
                } else {
                    ne.name.as_deref()
                };
                let edge = json!({
                    "source_id": source_id.to_string(),
                    "source_name": source_name,
                    "target_id": target_id.to_string(),
                    "target_name": target_name,
                    "relation": rec.relation.as_str(),
                    "weight": rec.weight,
                    "direction": rec.direction,
                    "hop": rec.hop,
                    "via": rec.via.map(|v| v.to_string()),
                });
                pairs.push(ContextPair::new(nj, edge)?);
            }
            blocks.push(AnchorBlock {
                entity_json,
                entity_size,
                pairs,
            });
        }

        let (out_anchors, out_edges, mut truncated, dropped_anchors, dropped_neighbors, mut used) =
            assemble_within_budget(&blocks, budget);

        anchor_edge_records.sort_by_key(|edge| edge.edge_id);
        let candidates: Vec<Value> = anchor_edge_records
            .into_iter()
            .filter_map(|edge| {
                let source = entity_meta.get(&edge.source_id)?;
                let target = entity_meta.get(&edge.target_id)?;
                Some(json!({
                    "source_id": edge.source_id.to_string(),
                    "source_name": source.name,
                    "target_id": edge.target_id.to_string(),
                    "target_name": target.name,
                    "relation": edge.relation.as_str(),
                    "weight": edge.weight,
                }))
            })
            .collect();
        let admitted_anchors: HashSet<&str> = out_anchors
            .iter()
            .filter_map(|anchor| anchor["entity"]["id"].as_str())
            .collect();
        let mut anchor_edges = Vec::with_capacity(candidates.len());
        let mut dropped_anchor_edges = 0usize;
        for edge in candidates {
            let endpoints_admitted = ["source_id", "target_id"].iter().all(|field| {
                edge[*field]
                    .as_str()
                    .is_some_and(|id| admitted_anchors.contains(id))
            });
            let size = compact_len(&edge)?;
            if !endpoints_admitted || size > budget - used {
                dropped_anchor_edges += 1;
                truncated = true;
                continue;
            }
            used += size;
            anchor_edges.push(edge);
        }

        if let Some(t) = t4 {
            plog(call_id, "assembly", t.elapsed().as_micros());
        }

        let mut response = json!({
            "anchors": out_anchors,
            "edges": out_edges,
            "anchor_edges": anchor_edges,
            "truncated": truncated,
            // `stage` is additive: every drop this handler produces originates in the
            // Stage-4 budget walk — `fanout`/`hops` bound the neighbor candidate pool
            // upstream, before assembly, so they never show up as a "drop" here today.
            "dropped": {
                "anchors": dropped_anchors,
                "neighbors": dropped_neighbors,
                "edges": dropped_neighbors,
                "anchor_edges": dropped_anchor_edges,
                "stage": "budget",
            },
        });
        let fields = response
            .as_object_mut()
            .expect("context response is an object");
        if scan_incomplete {
            fields.insert("scan_incomplete".into(), json!(true));
        }
        for (name, requested, effective) in clamp_reports {
            let Some(requested) = requested else {
                continue;
            };
            fields.insert(format!("requested_{name}"), json!(requested));
            fields.insert(format!("effective_{name}"), json!(effective));
            fields.insert(format!("{name}_clamped"), json!(requested != effective));
        }
        Ok(response)
    }
}

/// Two-pass deterministic budget walk over anchors + neighbors. See
/// `docs/api/context-verb.md`.
///
/// Pass 1 reserves budget for every anchor's own entity record, in rank
/// order, ignoring neighbors entirely. Pass 2 then fills neighbors for the
/// anchors that made it through pass 1, again in rank order, spending
/// whatever budget remains.
///
/// A single-pass walk (entity+neighbors per anchor before moving to the
/// next) lets one anchor's neighbor fan-out (driven by `fanout`/`hops`,
/// unrelated to how relevant that anchor is) exhaust the entire budget and
/// starve every anchor ranked after it — including anchors that are more
/// relevant to the query and would otherwise have made it into the result.
/// Reserving entity space first guarantees every anchor that fits gets
/// *some* representation before any anchor's neighbor list is allowed to
/// consume shared budget.
fn assemble_within_budget(
    blocks: &[AnchorBlock],
    budget: usize,
) -> (Vec<Value>, Vec<Value>, bool, usize, usize, usize) {
    let mut running = 0usize;
    let mut truncated = false;
    let mut included: Vec<bool> = vec![false; blocks.len()];

    for (i, block) in blocks.iter().enumerate() {
        if running + block.entity_size > budget {
            truncated = true;
            break;
        }
        running += block.entity_size;
        included[i] = true;
    }

    let mut out_anchors: Vec<Value> = Vec::with_capacity(blocks.len());
    let mut out_edges: Vec<Value> = Vec::new();
    let mut committed_neighbors: Vec<usize> = vec![0; blocks.len()];
    for (i, block) in blocks.iter().enumerate() {
        if !included[i] {
            continue;
        }
        let mut neighbor_out: Vec<Value> = Vec::new();
        for pair in &block.pairs {
            if running + pair.size > budget {
                truncated = true;
                break;
            }
            running += pair.size;
            neighbor_out.push(pair.neighbor.clone());
            out_edges.push(pair.edge.clone());
            committed_neighbors[i] += 1;
        }
        out_anchors.push(json!({
            "entity": block.entity_json.clone(),
            "neighbors": neighbor_out,
        }));
    }

    let committed_anchor_entities = out_anchors.len();
    let dropped_anchors = blocks.len() - committed_anchor_entities;
    let dropped_neighbors: usize = blocks
        .iter()
        .enumerate()
        .map(|(i, b)| b.pairs.len() - committed_neighbors[i])
        .sum();

    (
        out_anchors,
        out_edges,
        truncated,
        dropped_anchors,
        dropped_neighbors,
        running,
    )
}

fn compare_context_neighbors(
    a: &(NeighborHit, &str),
    b: &(NeighborHit, &str),
) -> std::cmp::Ordering {
    khive_score::cmp_desc_then_id_f64(
        a.0.weight,
        &(a.0.node_id, a.0.edge_id),
        b.0.weight,
        &(b.0.node_id, b.0.edge_id),
    )
}

fn compare_hop2_neighbors(
    a: &(Uuid, Uuid, EdgeRelation, f64, &str),
    b: &(Uuid, Uuid, EdgeRelation, f64, &str),
) -> std::cmp::Ordering {
    khive_score::cmp_desc_then_id_f64(a.3, &(a.1, a.0), b.3, &(b.1, b.0))
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "context_ordering_tests.rs"]
mod ordering_tests;

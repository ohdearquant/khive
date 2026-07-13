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

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::types::{Direction, NeighborQuery, PageRequest};
use khive_storage::{EdgeRelation, Entity, EntityFilter};

use super::common::{deser, parse_direction, parse_relation, resolve_uuid_async, ContextParams};
use crate::KgPack;

static CONTEXT_CALL_ID: AtomicU64 = AtomicU64::new(0);

fn context_profile_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        let enabled = std::env::var("KHIVE_CONTEXT_PROFILE").is_ok();
        khive_runtime::config_ledger::record_config_locked(
            "KHIVE_CONTEXT_PROFILE",
            enabled.to_string(),
        );
        enabled
    })
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

/// True iff every relation in the filter is a symmetric relation. Mirrors
/// `normalize_symmetric_direction` in `khive-runtime/src/operations.rs`
/// (private to that crate) — kept in lockstep because `neighbors_with_query`
/// forces `Direction::Both` under this exact condition regardless of the
/// direction actually requested, and the handler must know that happened to
/// tag direction correctly instead of issuing a second, redundant call.
fn relations_all_symmetric(relations: Option<&[EdgeRelation]>) -> bool {
    match relations {
        None => false,
        Some([]) => false,
        Some(rels) => rels
            .iter()
            .all(|r| matches!(r, EdgeRelation::CompetesWith | EdgeRelation::ComposedWith)),
    }
}

/// Fetch up to `fanout` neighbors of `node_id`, each tagged with its actual
/// direction relative to `node_id`. See module docs for why this can't just
/// trust a `direction` field on a plain `NeighborHit`.
async fn fetch_directed_neighbors(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    node_id: Uuid,
    requested_direction: &Direction,
    relations: Option<&[EdgeRelation]>,
    fanout: u32,
) -> Result<Vec<(Uuid, EdgeRelation, f64, &'static str)>, RuntimeError> {
    let relations_vec = relations.map(|r| r.to_vec());

    if relations_all_symmetric(relations) {
        let hits = runtime
            .neighbors_with_query(
                token,
                node_id,
                NeighborQuery {
                    direction: Direction::Both,
                    relations: relations_vec,
                    limit: Some(fanout),
                    min_weight: None,
                },
            )
            .await?;
        return Ok(hits
            .into_iter()
            .map(|h| (h.node_id, h.relation, h.weight, "both"))
            .collect());
    }

    match requested_direction {
        Direction::Out => {
            let hits = runtime
                .neighbors_with_query(
                    token,
                    node_id,
                    NeighborQuery {
                        direction: Direction::Out,
                        relations: relations_vec,
                        limit: Some(fanout),
                        min_weight: None,
                    },
                )
                .await?;
            Ok(hits
                .into_iter()
                .map(|h| (h.node_id, h.relation, h.weight, "outgoing"))
                .collect())
        }
        Direction::In => {
            let hits = runtime
                .neighbors_with_query(
                    token,
                    node_id,
                    NeighborQuery {
                        direction: Direction::In,
                        relations: relations_vec,
                        limit: Some(fanout),
                        min_weight: None,
                    },
                )
                .await?;
            Ok(hits
                .into_iter()
                .map(|h| (h.node_id, h.relation, h.weight, "incoming"))
                .collect())
        }
        Direction::Both => {
            // Single UNION ALL query for both directions (ADR-089 context-verb
            // optimization) instead of two separate direction-scoped calls —
            // halves the storage neighbor SELECT count for this branch. The
            // op already returns hits in global weight-descending,
            // node_id-ascending order truncated to `fanout`, so no local
            // re-sort/truncate is needed.
            let hits = runtime
                .neighbors_with_query_directed(
                    token,
                    node_id,
                    NeighborQuery {
                        direction: Direction::Both,
                        relations: relations_vec,
                        limit: Some(fanout),
                        min_weight: None,
                    },
                )
                .await?;
            Ok(hits
                .into_iter()
                .map(|(h, dir)| {
                    // `neighbors_with_query_directed` only ever tags hits `Out`/`In`
                    // (see `DirectedNeighborHit` doc comment) — `Both` never appears.
                    let tag = if dir == Direction::Out {
                        "outgoing"
                    } else {
                        "incoming"
                    };
                    (h.node_id, h.relation, h.weight, tag)
                })
                .collect())
        }
    }
}

fn compact_len(v: &Value) -> Result<usize, RuntimeError> {
    let s =
        serde_json::to_string(v).map_err(|e| RuntimeError::Internal(format!("serialize: {e}")))?;
    Ok(s.chars().count())
}

struct AnchorBlock {
    entity_json: Value,
    entity_size: usize,
    neighbor_jsons: Vec<(Value, usize)>,
}

impl KgPack {
    pub(crate) async fn handle_context(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let call_id = CONTEXT_CALL_ID.fetch_add(1, Ordering::Relaxed);
        let prof = context_profile_enabled();
        let p: ContextParams = deser(params)?;

        let has_query = p.query.as_deref().is_some_and(|s| !s.trim().is_empty());
        let has_ids = p.entity_ids.as_ref().is_some_and(|v| !v.is_empty());
        if !has_query && !has_ids {
            return Err(RuntimeError::InvalidInput(
                "context requires at least one of `query` or `entity_ids`".into(),
            ));
        }

        let hops = p.hops.unwrap_or(DEFAULT_HOPS).clamp(MIN_HOPS, MAX_HOPS);
        let budget = p
            .budget
            .unwrap_or(DEFAULT_BUDGET)
            .clamp(MIN_BUDGET, MAX_BUDGET) as usize;
        let limit = p.limit.unwrap_or(DEFAULT_LIMIT).clamp(MIN_LIMIT, MAX_LIMIT);
        let fanout = p
            .fanout
            .unwrap_or(DEFAULT_FANOUT)
            .clamp(MIN_FANOUT, MAX_FANOUT);
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
                let uuid = resolve_uuid_async(s, &self.runtime, token).await?;
                explicit_ids.push(uuid);
                if seen.insert(uuid) {
                    anchor_ids.push(uuid);
                }
            }
        }
        // `entity_ids` is an explicit entity-anchor contract (ADR-089 §1: "honored
        // in full"). `resolve_uuid_async` accepts any syntactically valid UUID
        // without checking substrate or existence, so a random UUID, a note UUID,
        // or an edge UUID would otherwise resolve here and then silently vanish
        // from the response in Stage 4's lenient "missing entity" fallback. Fail
        // loudly instead: one batch existence check naming every offending id
        // (High-2).
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
            // Fetch a larger candidate window than `limit` so that anchors which
            // collapse into `entity_ids` duplicates don't under-fill the query
            // leg: ADR-089 §1 promises search "fills up to `limit` additional
            // anchors" after explicit ids, which requires looking past the first
            // `limit` hits when some of them overlap explicit anchors. Bounded
            // by a documented cap so a pathological
            // overlap can't turn into an unbounded search.
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
        let mut per_anchor_neighbors: Vec<Vec<NeighborRecord>> =
            Vec::with_capacity(anchor_ids.len());

        for &anchor in &anchor_ids {
            let mut recs: Vec<NeighborRecord> = Vec::new();
            let mut hop1_parents: Vec<Uuid> = Vec::new();

            // hops=0 means anchors only — skip expansion entirely.
            if hops >= 1 {
                let hop1_raw = fetch_directed_neighbors(
                    &self.runtime,
                    token,
                    anchor,
                    &direction,
                    relations.as_deref(),
                    fanout,
                )
                .await?;

                for (id, relation, weight, dir) in hop1_raw {
                    if !visited.insert(id) {
                        continue;
                    }
                    recs.push(NeighborRecord {
                        id,
                        relation,
                        direction: dir,
                        weight,
                        hop: 1,
                        via: None,
                    });
                    hop1_parents.push(id);
                }
            }

            if hops == 2 {
                let mut hop2_pool: Vec<(Uuid, Uuid, EdgeRelation, f64, &'static str)> = Vec::new();
                for parent in &hop1_parents {
                    let hop2_raw = fetch_directed_neighbors(
                        &self.runtime,
                        token,
                        *parent,
                        &direction,
                        relations.as_deref(),
                        fanout,
                    )
                    .await?;
                    for (id, relation, weight, dir) in hop2_raw {
                        hop2_pool.push((*parent, id, relation, weight, dir));
                    }
                }
                // One stratum across all hop-1 parents under this anchor: sort by
                // weight desc, then neighbor id, then parent id (the last key only
                // arbitrates true ties — same neighbor, same weight, different
                // parent — so the "first discovering parent" is deterministic).
                hop2_pool.sort_by(|a, b| {
                    b.3.partial_cmp(&a.3)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.1.cmp(&b.1))
                        .then_with(|| a.0.cmp(&b.0))
                });
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
        if let Some(t) = t3 {
            plog(call_id, "entity_fetch", t.elapsed().as_micros());
        }

        // ---- Stage 4: assembly with budget enforcement ----
        let t4 = if prof { Some(Instant::now()) } else { None };
        // Explicit `entity_ids` anchors are already verified to exist in Stage 1
        // (High-2), so this only guards the residual race of an
        // anchor deleted concurrently between resolution and this fetch, or a
        // neighbor entity that vanished the same way. Neighbors get the same
        // lenient "missing node reads as absent" convention `neighbors_with_query`
        // already applies (it returns an empty Vec rather than erroring on a
        // nonexistent `node_id`) — they never enter the budget accounting below.
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

            let mut neighbor_jsons = Vec::with_capacity(per_anchor_neighbors[i].len());
            for rec in &per_anchor_neighbors[i] {
                let Some(ne) = entity_meta.get(&rec.id) else {
                    continue;
                };
                let nj = json!({
                    "id": rec.id.to_string(),
                    "name": ne.name,
                    "relation": rec.relation.as_str(),
                    "direction": rec.direction,
                    "weight": rec.weight,
                    "hop": rec.hop,
                    "via": rec.via.map(|v| v.to_string()),
                    "description": ne.description,
                });
                let size = compact_len(&nj)?;
                neighbor_jsons.push((nj, size));
            }
            blocks.push(AnchorBlock {
                entity_json,
                entity_size,
                neighbor_jsons,
            });
        }

        let (out_anchors, truncated, dropped_anchors, dropped_neighbors) =
            assemble_within_budget(&blocks, budget);

        if let Some(t) = t4 {
            plog(call_id, "assembly", t.elapsed().as_micros());
        }

        Ok(json!({
            "anchors": out_anchors,
            "truncated": truncated,
            "dropped": { "anchors": dropped_anchors, "neighbors": dropped_neighbors },
        }))
    }
}

/// Deterministic-order budget walk: append anchor entity records and their
/// neighbor records (each already produced in final display order) until the
/// next record's compact-JSON Unicode-scalar length would push the running
/// total past `budget`. Returns (assembled anchors, truncated, dropped
/// anchors, dropped neighbors).
fn assemble_within_budget(
    blocks: &[AnchorBlock],
    budget: usize,
) -> (Vec<Value>, bool, usize, usize) {
    let mut committed_anchor_entities = 0usize;
    let mut committed_neighbors: Vec<usize> = vec![0; blocks.len()];
    let mut running = 0usize;
    let mut truncated = false;
    let mut out_anchors: Vec<Value> = Vec::with_capacity(blocks.len());

    'outer: for (i, block) in blocks.iter().enumerate() {
        if running + block.entity_size > budget {
            truncated = true;
            break 'outer;
        }
        running += block.entity_size;
        committed_anchor_entities += 1;

        let mut neighbor_out: Vec<Value> = Vec::new();
        let mut hit_budget_mid_anchor = false;
        for (nj, size) in &block.neighbor_jsons {
            if running + size > budget {
                truncated = true;
                hit_budget_mid_anchor = true;
                break;
            }
            running += size;
            neighbor_out.push(nj.clone());
            committed_neighbors[i] += 1;
        }
        out_anchors.push(json!({
            "entity": block.entity_json.clone(),
            "neighbors": neighbor_out,
        }));
        if hit_budget_mid_anchor {
            break 'outer;
        }
    }

    let dropped_anchors = blocks.len() - committed_anchor_entities;
    let dropped_neighbors: usize = blocks
        .iter()
        .enumerate()
        .map(|(i, b)| b.neighbor_jsons.len() - committed_neighbors[i])
        .sum();

    (out_anchors, truncated, dropped_anchors, dropped_neighbors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor_block(
        name: &str,
        entity_size_filler: usize,
        neighbor_sizes: &[usize],
    ) -> AnchorBlock {
        let entity_json = json!({ "id": name, "filler": "x".repeat(entity_size_filler) });
        let entity_size = compact_len(&entity_json).unwrap();
        let neighbor_jsons = neighbor_sizes
            .iter()
            .enumerate()
            .map(|(idx, &sz)| {
                let nj = json!({ "id": format!("{name}-n{idx}"), "filler": "x".repeat(sz) });
                let size = compact_len(&nj).unwrap();
                (nj, size)
            })
            .collect();
        AnchorBlock {
            entity_json,
            entity_size,
            neighbor_jsons,
        }
    }

    #[test]
    fn relations_all_symmetric_true_for_only_symmetric_relations() {
        assert!(relations_all_symmetric(Some(&[EdgeRelation::CompetesWith])));
        assert!(relations_all_symmetric(Some(&[
            EdgeRelation::CompetesWith,
            EdgeRelation::ComposedWith
        ])));
    }

    #[test]
    fn relations_all_symmetric_false_for_mixed_or_absent() {
        assert!(!relations_all_symmetric(None));
        assert!(!relations_all_symmetric(Some(&[])));
        assert!(!relations_all_symmetric(Some(&[EdgeRelation::Extends])));
        assert!(!relations_all_symmetric(Some(&[
            EdgeRelation::CompetesWith,
            EdgeRelation::Extends
        ])));
    }

    #[test]
    fn assemble_within_budget_no_truncation_when_everything_fits() {
        let blocks = vec![anchor_block("a1", 4, &[4, 4]), anchor_block("a2", 4, &[4])];
        let total: usize = blocks
            .iter()
            .map(|b| b.entity_size + b.neighbor_jsons.iter().map(|(_, s)| s).sum::<usize>())
            .sum();
        let (out, truncated, d_anchors, d_neighbors) = assemble_within_budget(&blocks, total);
        assert!(!truncated);
        assert_eq!(d_anchors, 0);
        assert_eq!(d_neighbors, 0);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["neighbors"].as_array().unwrap().len(), 2);
        assert_eq!(out[1]["neighbors"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn assemble_within_budget_exact_boundary_is_not_truncated() {
        // A budget exactly equal to the cumulative size must NOT truncate —
        // the spec's stop condition is "would push the running total PAST
        // budget", i.e. a record landing exactly on the boundary still fits.
        let blocks = vec![anchor_block("a1", 4, &[4])];
        let exact_total = blocks[0].entity_size + blocks[0].neighbor_jsons[0].1;
        let (out, truncated, d_anchors, d_neighbors) = assemble_within_budget(&blocks, exact_total);
        assert!(
            !truncated,
            "exact-fit budget must not be reported as truncated"
        );
        assert_eq!(d_anchors, 0);
        assert_eq!(d_neighbors, 0);
        assert_eq!(out[0]["neighbors"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn assemble_within_budget_one_char_over_boundary_truncates_the_overflowing_record() {
        let blocks = vec![anchor_block("a1", 4, &[4, 4])];
        let entity_size = blocks[0].entity_size;
        let first_neighbor_size = blocks[0].neighbor_jsons[0].1;
        // Budget fits the anchor entity plus the first neighbor exactly, but not
        // a single Unicode scalar more — the second neighbor must be dropped.
        let budget = entity_size + first_neighbor_size;
        let (out, truncated, d_anchors, d_neighbors) = assemble_within_budget(&blocks, budget);
        assert!(truncated);
        assert_eq!(d_anchors, 0, "the anchor's entity record itself fit");
        assert_eq!(
            d_neighbors, 1,
            "exactly the overflowing neighbor is dropped"
        );
        assert_eq!(out[0]["neighbors"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn assemble_within_budget_anchor_entity_overflow_drops_whole_anchor_and_all_after_it() {
        let blocks = vec![anchor_block("a1", 4, &[4]), anchor_block("a2", 4, &[4])];
        // Budget too small even for the first anchor's entity record.
        let budget = blocks[0].entity_size - 1;
        let (out, truncated, d_anchors, d_neighbors) = assemble_within_budget(&blocks, budget);
        assert!(truncated);
        assert!(out.is_empty(), "no anchor entity fit at all");
        assert_eq!(d_anchors, 2, "both anchors dropped");
        assert_eq!(d_neighbors, 2, "both anchors' single neighbor each dropped");
    }

    #[test]
    fn assemble_within_budget_second_anchor_fully_dropped_when_budget_exhausted_by_first() {
        let blocks = vec![anchor_block("a1", 4, &[4]), anchor_block("a2", 4, &[4, 4])];
        let budget = blocks[0].entity_size + blocks[0].neighbor_jsons[0].1;
        let (out, truncated, d_anchors, d_neighbors) = assemble_within_budget(&blocks, budget);
        assert!(truncated);
        assert_eq!(out.len(), 1, "only the first anchor was assembled");
        assert_eq!(d_anchors, 1, "second anchor dropped entirely");
        assert_eq!(d_neighbors, 2, "second anchor's two neighbors both dropped");
    }

    #[test]
    fn assemble_within_budget_hop_and_via_pass_through_untouched() {
        let hop1 = json!({ "id": "n1", "hop": 1, "via": Value::Null });
        let hop2 = json!({ "id": "n2", "hop": 2, "via": "n1" });
        let sz1 = compact_len(&hop1).unwrap();
        let sz2 = compact_len(&hop2).unwrap();
        let block = AnchorBlock {
            entity_json: json!({ "id": "a1" }),
            entity_size: compact_len(&json!({ "id": "a1" })).unwrap(),
            neighbor_jsons: vec![(hop1, sz1), (hop2, sz2)],
        };
        let budget = block.entity_size + sz1 + sz2;
        let (out, truncated, ..) = assemble_within_budget(&[block], budget);
        assert!(!truncated);
        let neighbors = out[0]["neighbors"].as_array().unwrap();
        assert_eq!(neighbors[0]["hop"], 1);
        assert_eq!(neighbors[0]["via"], Value::Null);
        assert_eq!(neighbors[1]["hop"], 2);
        assert_eq!(neighbors[1]["via"], "n1");
    }
}

//! Deserialization structs for KG verb handler parameters.

use serde::{Deserialize, Deserializer};
use serde_json::Value;

// ---- Param structs ----

#[derive(Deserialize)]
pub(crate) struct EdgeSpec {
    pub(crate) target_id: String,
    pub(crate) relation: String,
    pub(crate) weight: Option<f64>,
}

/// A single entry in a bulk `create(items=[...])` request.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BulkCreateEntry {
    pub(crate) kind: String,
    pub(crate) name: Option<String>,
    pub(crate) entity_kind: Option<String>,
    pub(crate) note_kind: Option<String>,
    pub(crate) entity_type: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) content: Option<String>,
    pub(crate) salience: Option<f64>,
    pub(crate) annotates: Option<Vec<String>>,
    pub(crate) properties: Option<Value>,
    pub(crate) tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub(crate) struct CreateParams {
    pub(crate) kind: String,
    pub(crate) entity_type: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) content: Option<String>,
    pub(crate) salience: Option<f64>,
    pub(crate) annotates: Option<Vec<String>>,
    pub(crate) properties: Option<Value>,
    pub(crate) tags: Option<Vec<String>>,
    pub(crate) skip_dedup_check: Option<bool>,
    pub(crate) edges: Option<Vec<EdgeSpec>>,
    /// Singleton-note-only vector-embedding input override (issue #764). When
    /// present, `content` is still stored/FTS-indexed in full; only the text
    /// sent to the embedder is replaced with this value.
    pub(crate) embedding_content: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GetParams {
    pub(crate) id: String,
    pub(crate) include_deleted: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListParams {
    pub(crate) kind: String,
    pub(crate) limit: Option<u32>,
    pub(crate) offset: Option<u32>,
    pub(crate) entity_kind: Option<String>,
    pub(crate) entity_type: Option<String>,
    pub(crate) tags: Option<Vec<String>>,
    pub(crate) source_id: Option<String>,
    pub(crate) target_id: Option<String>,
    pub(crate) relations: Option<Vec<String>>,
    pub(crate) min_weight: Option<f64>,
    pub(crate) max_weight: Option<f64>,
    /// Keyset cursor for `kind="edge"`: the `id` of the last edge from the
    /// previous page. When set, the response is `{edges, next_after}` instead
    /// of a bare array — see the `list` verb description.
    pub(crate) after: Option<String>,
    pub(crate) note_kind: Option<String>,
    pub(crate) thread_id: Option<String>,
    pub(crate) direction: Option<String>,
    pub(crate) from: Option<String>,
    pub(crate) to: Option<String>,
    pub(crate) read: Option<bool>,
    #[serde(default)]
    pub(crate) delivered: Option<bool>,
    pub(crate) verb: Option<String>,
    pub(crate) verbs: Option<Vec<String>>,
    pub(crate) outcome: Option<String>,
    pub(crate) actor: Option<String>,
    pub(crate) substrate: Option<String>,
    pub(crate) since: Option<i64>,
    pub(crate) until: Option<i64>,
    pub(crate) event_kind: Option<String>,
    pub(crate) event_kinds: Option<Vec<String>>,
    pub(crate) session_id: Option<String>,
    pub(crate) observed: Option<Vec<String>>,
    pub(crate) selected: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct StatsParams {}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct WhoamiParams {}

/// ADR-099 B3: `pub` so kkernel's `--atomic` seam can deserialize through this same
/// canonical struct, reproducing `deny_unknown_fields` rejection. Fields stay
/// `pub(crate)` — the atomic seam only needs the `Result<_, _>` outcome.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateParams {
    pub(crate) id: String,
    pub(crate) kind: Option<String>,
    pub(crate) name: Option<Value>,
    pub(crate) description: Option<Value>,
    pub(crate) content: Option<String>,
    #[serde(default, deserialize_with = "tri_f64")]
    pub(crate) salience: Option<Option<f64>>,
    #[serde(default, deserialize_with = "tri_f64")]
    pub(crate) decay_factor: Option<Option<f64>>,
    pub(crate) properties: Option<Value>,
    pub(crate) tags: Option<Vec<String>>,
    pub(crate) relation: Option<String>,
    pub(crate) weight: Option<f64>,
    pub(crate) entity_kind: Option<Value>,
}

/// ADR-099 B3: `pub` for the same reason as `UpdateParams` above — reused
/// by the atomic seam to validate `delete` args.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteParams {
    pub(crate) id: String,
    pub(crate) kind: Option<String>,
    pub(crate) hard: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MergeParams {
    #[serde(alias = "winner_id", alias = "target_id")]
    pub(crate) into_id: String,
    #[serde(alias = "loser_id", alias = "source_id")]
    pub(crate) from_id: String,
    pub(crate) kind: Option<String>,
    pub(crate) strategy: Option<String>,
    pub(crate) content_strategy: Option<String>,
    pub(crate) dry_run: Option<bool>,
    pub(crate) force: Option<bool>,
    #[allow(dead_code)]
    pub(crate) verbose: Option<bool>,
    pub(crate) reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchParams {
    /// Required, but kept `Option` at the wire boundary so a caller who omits
    /// it entirely gets the enumerated-valid-kinds error from
    /// `missing_kind_error` instead of a raw serde "missing field" message.
    pub(crate) kind: Option<String>,
    pub(crate) query: String,
    pub(crate) limit: Option<u32>,
    pub(crate) entity_kind: Option<String>,
    pub(crate) entity_type: Option<String>,
    pub(crate) note_kind: Option<String>,
    pub(crate) include_superseded: Option<bool>,
    pub(crate) properties: Option<Value>,
    pub(crate) tags: Option<Vec<String>>,
    pub(crate) min_score: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BulkLinkEntry {
    pub(crate) source_id: String,
    pub(crate) target_id: String,
    pub(crate) relation: String,
    pub(crate) weight: Option<f64>,
    pub(crate) metadata: Option<Value>,
    pub(crate) dependency_kind: Option<String>,
}

/// ADR-099 B3: `pub` for the same reason as `UpdateParams` above — reused
/// by the atomic seam to validate `link` args. `BulkLinkEntry` (the type of
/// `links` below) stays `pub(crate)`: the `links` field itself is
/// `pub(crate)`, so it never appears in `LinkParams`'s public surface.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkParams {
    pub(crate) source_id: Option<String>,
    pub(crate) target_id: Option<String>,
    pub(crate) relation: Option<String>,
    pub(crate) weight: Option<f64>,
    pub(crate) metadata: Option<Value>,
    pub(crate) dependency_kind: Option<String>,
    pub(crate) verbose: Option<bool>,
    pub(crate) links: Option<Vec<BulkLinkEntry>>,
    pub(crate) atomic: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NeighborsParams {
    #[serde(alias = "node_id")]
    pub(crate) id: String,
    pub(crate) direction: Option<String>,
    pub(crate) limit: Option<u32>,
    pub(crate) min_weight: Option<f64>,
    pub(crate) relations: Option<Vec<String>>,
    /// When true, each neighbor in the result carries its `entity_type` field.
    /// Absent or false: result shape is identical to today (no `entity_type` key).
    pub(crate) include_entity_type: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TraverseParams {
    #[serde(alias = "ids", alias = "start_ids")]
    pub(crate) roots: Vec<String>,
    pub(crate) max_depth: Option<usize>,
    pub(crate) direction: Option<String>,
    pub(crate) relations: Option<Vec<String>>,
    pub(crate) min_weight: Option<f64>,
    pub(crate) limit: Option<u32>,
    pub(crate) include_roots: Option<bool>,
    /// When true, each path node in the result carries the entity `properties` map.
    /// Absent or false: result shape is identical to today (no `properties` key).
    pub(crate) include_properties: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ContextParams {
    pub(crate) query: Option<String>,
    pub(crate) entity_ids: Option<Vec<String>>,
    pub(crate) hops: Option<i64>,
    pub(crate) budget: Option<i64>,
    pub(crate) relations: Option<Vec<String>>,
    pub(crate) direction: Option<String>,
    pub(crate) limit: Option<u32>,
    pub(crate) fanout: Option<u32>,
}

pub(crate) const HARD_CAP: usize = 10_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueryParams {
    pub(crate) query: String,
    #[serde(default)]
    pub(crate) limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProposeParams {
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) changeset: Value,
    #[serde(default)]
    pub(crate) reviewers: Vec<String>,
    pub(crate) expiry: Option<i64>,
    pub(crate) parent_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewParams {
    pub(crate) id: String,
    pub(crate) decision: String,
    pub(crate) comment: Option<String>,
    pub(crate) max_new_entries: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WithdrawParams {
    pub(crate) id: String,
    pub(crate) rationale: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListProposalsParams {
    pub(crate) status: Option<String>,
    pub(crate) proposer: Option<String>,
    pub(crate) actor: Option<String>,
    pub(crate) limit: Option<u32>,
    pub(crate) offset: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResolveParams {
    pub(crate) refs: Vec<String>,
    pub(crate) kind: Option<String>,
    pub(crate) limit: Option<u32>,
}

pub(crate) fn tri_f64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<f64>>, D::Error> {
    Ok(Some(Option::deserialize(d)?))
}

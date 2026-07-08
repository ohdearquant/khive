//! The five typed change-set operations and their stage-time payloads.

use std::collections::BTreeMap;

use khive_types::{EdgeRelation, Entity, EntityKind, Id128, Link, Namespace, Note, PropertyValue};
use serde::{Deserialize, Deserializer, Serialize};

use crate::strict::{StrictEntity, StrictLink, StrictNote};

/// One staged mutation. Tagged internally by `"op"` so every NDJSON-delta
/// line self-describes its kind without a wrapping envelope.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Create(CreateOp),
    Link(LinkOp),
    Update(UpdateOp),
    Delete(DeleteOp),
    Merge(MergeOp),
}

// ---- create -----------------------------------------------------------

/// Create a new entity or note. `id` is minted at stage time and is stable
/// across however long the change-set sits before it is applied.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateOp {
    pub id: Id128,
    pub namespace: Namespace,
    pub target: CreateTarget,
}

/// The substrate a `create` op targets, with its own field surface.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CreateTarget {
    Entity(EntityCreateFields),
    Note(NoteCreateFields),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityCreateFields {
    pub entity_kind: EntityKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_type: Option<String>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub properties: BTreeMap<String, PropertyValue>,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoteCreateFields {
    /// Pack-declared note kind string (e.g. `"observation"`); not a closed enum.
    pub note_kind: String,
    pub content: String,
    #[serde(default)]
    pub properties: BTreeMap<String, PropertyValue>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub salience: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decay_factor: Option<f64>,
}

// ---- link ---------------------------------------------------------------

/// Create a new directed, typed edge. `id` is minted at stage time; `source`
/// and `target` may resolve to another op's stage-time `id` in the same or a
/// different change-set.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(into = "LinkOpRaw")]
pub struct LinkOp {
    pub id: Id128,
    pub namespace: Namespace,
    pub source: Id128,
    pub target: Id128,
    pub relation: EdgeRelation,
    pub weight: f64,
    pub properties: BTreeMap<String, PropertyValue>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkOpRaw {
    id: Id128,
    namespace: Namespace,
    source: Id128,
    target: Id128,
    relation: EdgeRelation,
    weight: f64,
    #[serde(default)]
    properties: BTreeMap<String, PropertyValue>,
}

impl From<LinkOp> for LinkOpRaw {
    fn from(l: LinkOp) -> Self {
        Self {
            id: l.id,
            namespace: l.namespace,
            source: l.source,
            target: l.target,
            relation: l.relation,
            weight: l.weight,
            properties: l.properties,
        }
    }
}

impl TryFrom<LinkOpRaw> for LinkOp {
    type Error = String;

    fn try_from(raw: LinkOpRaw) -> Result<Self, Self::Error> {
        if !raw.weight.is_finite() {
            return Err(format!("LinkOp weight must be finite, got {}", raw.weight));
        }
        if !(0.0..=1.0).contains(&raw.weight) {
            return Err(format!(
                "LinkOp weight must be in [0.0, 1.0], got {}",
                raw.weight
            ));
        }
        Ok(LinkOp {
            id: raw.id,
            namespace: raw.namespace,
            source: raw.source,
            target: raw.target,
            relation: raw.relation,
            weight: raw.weight,
            properties: raw.properties,
        })
    }
}

impl<'de> Deserialize<'de> for LinkOp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = LinkOpRaw::deserialize(deserializer)?;
        LinkOp::try_from(raw).map_err(serde::de::Error::custom)
    }
}

// ---- update ---------------------------------------------------------------

/// Patch an existing entity, note, or edge's mutable fields. Carries no
/// preimage; see `README.md` for why and the known gap that leaves open.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateOp {
    pub target_id: Id128,
    pub patch: UpdatePatch,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum UpdatePatch {
    Entity(EntityPatch),
    Note(NotePatch),
    Edge(EdgePatch),
}

/// Absent field = unchanged. `description` distinguishes explicit-null
/// (clear) from absent (unchanged) via `Option<Option<String>>`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "opt_opt")]
    pub description: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<BTreeMap<String, PropertyValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "opt_opt")]
    pub salience: Option<Option<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "opt_opt")]
    pub decay_factor: Option<Option<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<BTreeMap<String, PropertyValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

/// `weight`, when present, must be finite and in `[0.0, 1.0]` — the same
/// invariant `LinkOp`/`khive_types::Link` enforce, so a staged edge update
/// can never target a weight the live graph would itself reject.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(into = "EdgePatchRaw")]
pub struct EdgePatch {
    pub relation: Option<EdgeRelation>,
    pub weight: Option<f64>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EdgePatchRaw {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    relation: Option<EdgeRelation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    weight: Option<f64>,
}

impl From<EdgePatch> for EdgePatchRaw {
    fn from(p: EdgePatch) -> Self {
        Self {
            relation: p.relation,
            weight: p.weight,
        }
    }
}

impl TryFrom<EdgePatchRaw> for EdgePatch {
    type Error = String;

    fn try_from(raw: EdgePatchRaw) -> Result<Self, Self::Error> {
        if let Some(w) = raw.weight {
            if !w.is_finite() {
                return Err(format!("EdgePatch weight must be finite, got {w}"));
            }
            if !(0.0..=1.0).contains(&w) {
                return Err(format!("EdgePatch weight must be in [0.0, 1.0], got {w}"));
            }
        }
        Ok(EdgePatch {
            relation: raw.relation,
            weight: raw.weight,
        })
    }
}

impl<'de> Deserialize<'de> for EdgePatch {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = EdgePatchRaw::deserialize(deserializer)?;
        EdgePatch::try_from(raw).map_err(serde::de::Error::custom)
    }
}

/// Serde helper for `Option<Option<T>>`: distinguishes absent (unchanged)
/// from explicit `null` (clear) on partial-update patch fields.
mod opt_opt {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<T, S>(val: &Option<Option<T>>, s: S) -> Result<S::Ok, S::Error>
    where
        T: Serialize,
        S: Serializer,
    {
        match val {
            None => unreachable!("skip_serializing_if guards the None case"),
            Some(inner) => inner.serialize(s),
        }
    }

    pub fn deserialize<'de, T, D>(d: D) -> Result<Option<Option<T>>, D::Error>
    where
        T: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        let opt: Option<T> = Option::deserialize(d)?;
        Ok(Some(opt))
    }
}

// ---- delete ---------------------------------------------------------------

/// Remove an existing entity, note, or edge. `preimage` captures the full
/// prior record state at stage time — required, not optional, so a `delete`
/// op without a captured preimage is unrepresentable. `target_id` is
/// validated against the embedded preimage's own record id at deserialize
/// time; a mismatched pair is a parse error, not a silently-accepted op.
#[derive(Clone, Debug, Serialize)]
pub struct DeleteOp {
    pub target_id: Id128,
    pub hard: bool,
    pub preimage: DeletePreimage,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteOpRaw {
    target_id: Id128,
    hard: bool,
    preimage: DeletePreimage,
}

impl<'de> Deserialize<'de> for DeleteOp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = DeleteOpRaw::deserialize(deserializer)?;
        let preimage_id = raw.preimage.record_id();
        if raw.target_id != preimage_id {
            return Err(serde::de::Error::custom(format!(
                "DeleteOp target_id {} does not match preimage record id {preimage_id}",
                raw.target_id
            )));
        }
        Ok(DeleteOp {
            target_id: raw.target_id,
            hard: raw.hard,
            preimage: raw.preimage,
        })
    }
}

// `Entity` and `Note` each already serialize their own `kind` field
// (entity kind / note kind), so the discriminant tag here is `substrate`
// (matching `khive_types::SubstrateKind`'s vocabulary) rather than `kind`,
// which would collide and produce a "duplicate field `kind`" parse error.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "substrate", rename_all = "snake_case")]
pub enum DeletePreimage {
    Entity(Box<Entity>),
    Note(Box<Note>),
    Edge(Box<Link>),
}

impl DeletePreimage {
    /// The identifier of the record this preimage was captured from.
    fn record_id(&self) -> Id128 {
        match self {
            DeletePreimage::Entity(e) => e.header.id,
            DeletePreimage::Note(n) => n.header.id,
            DeletePreimage::Edge(l) => l.id,
        }
    }
}

/// Deserialize-only mirror of [`DeletePreimage`], substituting each
/// substrate's strict (`deny_unknown_fields`) wire-shape mirror for the
/// looser `khive_types` deserializer.
#[derive(Deserialize)]
#[serde(tag = "substrate", rename_all = "snake_case")]
enum DeletePreimageRaw {
    Entity(StrictEntity),
    Note(StrictNote),
    Edge(StrictLink),
}

impl<'de> Deserialize<'de> for DeletePreimage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = DeletePreimageRaw::deserialize(deserializer)?;
        Ok(match raw {
            DeletePreimageRaw::Entity(e) => DeletePreimage::Entity(Box::new(e.into())),
            DeletePreimageRaw::Note(n) => {
                DeletePreimage::Note(Box::new(n.try_into().map_err(serde::de::Error::custom)?))
            }
            DeletePreimageRaw::Edge(l) => {
                DeletePreimage::Edge(Box::new(l.try_into().map_err(serde::de::Error::custom)?))
            }
        })
    }
}

// ---- merge ------------------------------------------------------------

/// Merge two entities. `preimage` captures both prior entities and the
/// incident edges the merge will rewire — required, not optional, so a
/// `merge` op without a captured preimage is unrepresentable. `into_id` /
/// `from_id` are validated against `preimage.into` / `preimage.from`'s own
/// record ids, and every incident edge is validated to actually touch one
/// of the two merge participants, at deserialize time.
#[derive(Clone, Debug, Serialize)]
pub struct MergeOp {
    pub into_id: Id128,
    pub from_id: Id128,
    pub preimage: MergePreimage,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MergeOpRaw {
    into_id: Id128,
    from_id: Id128,
    preimage: MergePreimage,
}

impl<'de> Deserialize<'de> for MergeOp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = MergeOpRaw::deserialize(deserializer)?;
        if raw.into_id != raw.preimage.into.header.id {
            return Err(serde::de::Error::custom(format!(
                "MergeOp into_id {} does not match preimage.into record id {}",
                raw.into_id, raw.preimage.into.header.id
            )));
        }
        if raw.from_id != raw.preimage.from.header.id {
            return Err(serde::de::Error::custom(format!(
                "MergeOp from_id {} does not match preimage.from record id {}",
                raw.from_id, raw.preimage.from.header.id
            )));
        }
        for edge in &raw.preimage.incident_edges {
            let touches_into = edge.source == raw.into_id || edge.target == raw.into_id;
            let touches_from = edge.source == raw.from_id || edge.target == raw.from_id;
            if !(touches_into || touches_from) {
                return Err(serde::de::Error::custom(format!(
                    "MergeOp incident edge {} references neither into_id {} nor from_id {}",
                    edge.id, raw.into_id, raw.from_id
                )));
            }
        }
        Ok(MergeOp {
            into_id: raw.into_id,
            from_id: raw.from_id,
            preimage: raw.preimage,
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct MergePreimage {
    pub into: Box<Entity>,
    pub from: Box<Entity>,
    pub incident_edges: Vec<Link>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MergePreimageRaw {
    into: StrictEntity,
    from: StrictEntity,
    incident_edges: Vec<StrictLink>,
}

impl<'de> Deserialize<'de> for MergePreimage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = MergePreimageRaw::deserialize(deserializer)?;
        let incident_edges = raw
            .incident_edges
            .into_iter()
            .map(|l| l.try_into().map_err(serde::de::Error::custom))
            .collect::<Result<Vec<Link>, D::Error>>()?;
        Ok(MergePreimage {
            into: Box::new(raw.into.into()),
            from: Box::new(raw.from.into()),
            incident_edges,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_op_rejects_out_of_range_weight() {
        let json = serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "namespace": "local",
            "source": "00000000-0000-0000-0000-000000000002",
            "target": "00000000-0000-0000-0000-000000000003",
            "relation": "extends",
            "weight": 1.5,
            "properties": {}
        });
        let result: Result<LinkOp, _> = serde_json::from_value(json);
        assert!(result.is_err());
    }

    #[test]
    fn create_op_rejects_unknown_field() {
        let json = serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "namespace": "local",
            "target": {
                "kind": "entity",
                "entity_kind": "concept",
                "name": "X",
                "surprise": true
            }
        });
        let result: Result<CreateOp, _> = serde_json::from_value(json);
        assert!(result.is_err());
    }

    #[test]
    fn entity_patch_distinguishes_absent_and_null_description() {
        let absent: EntityPatch = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(absent.description, None);

        let cleared: EntityPatch =
            serde_json::from_value(serde_json::json!({ "description": null })).unwrap();
        assert_eq!(cleared.description, Some(None));

        let set: EntityPatch =
            serde_json::from_value(serde_json::json!({ "description": "new" })).unwrap();
        assert_eq!(set.description, Some(Some("new".to_string())));
    }

    #[test]
    fn delete_op_requires_preimage() {
        let json = serde_json::json!({
            "target_id": "00000000-0000-0000-0000-000000000001",
            "hard": false
        });
        let result: Result<DeleteOp, _> = serde_json::from_value(json);
        assert!(result.is_err());
    }

    #[test]
    fn merge_op_requires_preimage() {
        let json = serde_json::json!({
            "into_id": "00000000-0000-0000-0000-000000000001",
            "from_id": "00000000-0000-0000-0000-000000000002"
        });
        let result: Result<MergeOp, _> = serde_json::from_value(json);
        assert!(result.is_err());
    }
}

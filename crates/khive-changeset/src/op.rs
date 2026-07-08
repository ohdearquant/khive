//! The five typed change-set operations and their stage-time payloads.

use std::collections::BTreeMap;

use khive_types::{EdgeRelation, Entity, EntityKind, Id128, Link, Namespace, Note, PropertyValue};
use serde::{Deserialize, Serialize};

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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation: Option<EdgeRelation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<f64>,
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
/// op without a captured preimage is unrepresentable.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteOp {
    pub target_id: Id128,
    pub hard: bool,
    pub preimage: DeletePreimage,
}

// `Entity` and `Note` each already serialize their own `kind` field
// (entity kind / note kind), so the discriminant tag here is `substrate`
// (matching `khive_types::SubstrateKind`'s vocabulary) rather than `kind`,
// which would collide and produce a "duplicate field `kind`" parse error.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "substrate", rename_all = "snake_case")]
pub enum DeletePreimage {
    Entity(Box<Entity>),
    Note(Box<Note>),
    Edge(Box<Link>),
}

// ---- merge ------------------------------------------------------------

/// Merge two entities. `preimage` captures both prior entities and the
/// incident edges the merge will rewire — required, not optional, so a
/// `merge` op without a captured preimage is unrepresentable.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeOp {
    pub into_id: Id128,
    pub from_id: Id128,
    pub preimage: MergePreimage,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergePreimage {
    pub into: Box<Entity>,
    pub from: Box<Entity>,
    pub incident_edges: Vec<Link>,
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

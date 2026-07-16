//! The five typed change-set operations and their stage-time payloads.

use std::collections::BTreeMap;

use khive_types::{EdgeRelation, Entity, EntityKind, Id128, Link, Namespace, Note, PropertyValue};
use serde::{Deserialize, Deserializer, Serialize};

use crate::strict::{StrictEntity, StrictLink, StrictNote};

/// One staged mutation, internally tagged by the snake-case `op` field.
///
/// See `crates/khive-changeset/docs/api/create-and-link.md` for wire examples.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Create(CreateOp),
    Link(LinkOp),
    Update(UpdateOp),
    Delete(DeleteOp),
    Merge(MergeOp),
}

/// Creates an entity or note with an ID minted and stabilized at stage time.
///
/// See `crates/khive-changeset/docs/api/create-and-link.md` for field semantics.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateOp {
    pub id: Id128,
    pub namespace: Namespace,
    pub target: CreateTarget,
}

/// Substrate-specific create fields, tagged by `kind`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CreateTarget {
    Entity(EntityCreateFields),
    Note(NoteCreateFields),
}

/// Fields staged for a new entity.
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

/// Fields staged for a new note.
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

/// Creates a directed edge whose ID is minted at stage time.
///
/// Endpoints may reference stage-time IDs from this or another change-set.
/// `weight` must be finite and in `[0.0, 1.0]` when deserialized.
/// See `crates/khive-changeset/docs/api/create-and-link.md` for field semantics.
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

/// Patches one record with a stage-time preimage of exactly the touched fields.
///
/// Patch and preimage must target the same substrate and have identical field
/// presence; both construction and deserialization enforce this invariant.
/// See `crates/khive-changeset/docs/api/update.md` for nullable-field semantics.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UpdateOp {
    target_id: Id128,
    patch: UpdatePatch,
    preimage: UpdatePreimage,
}

impl UpdateOp {
    /// Constructs an update after validating patch/preimage congruence.
    ///
    /// # Errors
    ///
    /// Returns an error for differing substrates or field sets, or for an
    /// invalid captured salience, decay factor, or edge weight.
    pub fn new(
        target_id: Id128,
        patch: UpdatePatch,
        preimage: UpdatePreimage,
    ) -> Result<Self, String> {
        validate_update_congruence(&patch, &preimage)?;
        Ok(UpdateOp {
            target_id,
            patch,
            preimage,
        })
    }

    /// The identifier of the record this update targets.
    pub fn target_id(&self) -> Id128 {
        self.target_id
    }

    /// The staged patch.
    pub fn patch(&self) -> &UpdatePatch {
        &self.patch
    }

    /// The field-scoped prior value the patch touches.
    pub fn preimage(&self) -> &UpdatePreimage {
        &self.preimage
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateOpRaw {
    target_id: Id128,
    patch: UpdatePatch,
    preimage: UpdatePreimage,
}

impl<'de> Deserialize<'de> for UpdateOp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = UpdateOpRaw::deserialize(deserializer)?;
        UpdateOp::new(raw.target_id, raw.patch, raw.preimage).map_err(serde::de::Error::custom)
    }
}

/// Substrate-specific patch tagged by `target`.
///
/// See `crates/khive-changeset/docs/api/update.md` for field-presence semantics.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum UpdatePatch {
    Entity(EntityPatch),
    Note(NotePatch),
    Edge(EdgePatch),
}

/// Entity fields to mutate; absent means unchanged and `Some(None)` clears description.
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

/// Note fields to mutate; nested options distinguish unchanged, clear, and set.
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

/// Edge fields to mutate; weight must be finite and in `[0.0, 1.0]`.
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

/// Distinguishes an absent patch field from an explicit `null` clear.
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

/// Prior entity values for exactly the fields touched by [`EntityPatch`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityPreimage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "opt_opt")]
    pub description: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<BTreeMap<String, PropertyValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

/// Prior note values for exactly the fields touched by [`NotePatch`].
///
/// Captured salience is finite in `[0.0, 1.0]`; decay factor is finite and non-negative.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotePreimage {
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

/// Prior edge values for exactly the fields touched by [`EdgePatch`].
///
/// Captured weight must be finite and in `[0.0, 1.0]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgePreimage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation: Option<EdgeRelation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<f64>,
}

/// Field-scoped preimage tagged with the same `target` as [`UpdatePatch`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum UpdatePreimage {
    Entity(EntityPreimage),
    Note(NotePreimage),
    Edge(EdgePreimage),
}

/// Enforces substrate, exact field-set, and captured numeric-value invariants.
fn validate_update_congruence(
    patch: &UpdatePatch,
    preimage: &UpdatePreimage,
) -> Result<(), String> {
    match (patch, preimage) {
        (UpdatePatch::Entity(p), UpdatePreimage::Entity(pre)) => {
            check_touched("name", p.name.is_some(), pre.name.is_some())?;
            check_touched(
                "description",
                p.description.is_some(),
                pre.description.is_some(),
            )?;
            check_touched(
                "properties",
                p.properties.is_some(),
                pre.properties.is_some(),
            )?;
            check_touched("tags", p.tags.is_some(), pre.tags.is_some())?;
            Ok(())
        }
        (UpdatePatch::Note(p), UpdatePreimage::Note(pre)) => {
            check_touched("content", p.content.is_some(), pre.content.is_some())?;
            check_touched("salience", p.salience.is_some(), pre.salience.is_some())?;
            check_touched(
                "decay_factor",
                p.decay_factor.is_some(),
                pre.decay_factor.is_some(),
            )?;
            check_touched(
                "properties",
                p.properties.is_some(),
                pre.properties.is_some(),
            )?;
            check_touched("tags", p.tags.is_some(), pre.tags.is_some())?;
            if let Some(Some(s)) = pre.salience {
                if !s.is_finite() || !(0.0..=1.0).contains(&s) {
                    return Err(format!(
                        "UpdateOp note preimage salience must be finite and in [0.0, 1.0], got {s}"
                    ));
                }
            }
            if let Some(Some(d)) = pre.decay_factor {
                if !d.is_finite() || d < 0.0 {
                    return Err(format!(
                        "UpdateOp note preimage decay_factor must be finite and non-negative, got {d}"
                    ));
                }
            }
            Ok(())
        }
        (UpdatePatch::Edge(p), UpdatePreimage::Edge(pre)) => {
            check_touched("relation", p.relation.is_some(), pre.relation.is_some())?;
            check_touched("weight", p.weight.is_some(), pre.weight.is_some())?;
            if let Some(w) = pre.weight {
                if !w.is_finite() || !(0.0..=1.0).contains(&w) {
                    return Err(format!(
                        "UpdateOp edge preimage weight must be finite and in [0.0, 1.0], got {w}"
                    ));
                }
            }
            Ok(())
        }
        _ => Err(format!(
            "UpdateOp patch target ({}) does not match preimage target ({})",
            patch_target_name(patch),
            preimage_target_name(preimage)
        )),
    }
}

fn check_touched(field: &str, patch_touched: bool, preimage_present: bool) -> Result<(), String> {
    match (patch_touched, preimage_present) {
        (true, false) => Err(format!(
            "UpdateOp preimage is missing `{field}`, which the patch sets or clears"
        )),
        (false, true) => Err(format!(
            "UpdateOp preimage carries `{field}`, which the patch leaves unchanged"
        )),
        _ => Ok(()),
    }
}

fn patch_target_name(patch: &UpdatePatch) -> &'static str {
    match patch {
        UpdatePatch::Entity(_) => "entity",
        UpdatePatch::Note(_) => "note",
        UpdatePatch::Edge(_) => "edge",
    }
}

fn preimage_target_name(preimage: &UpdatePreimage) -> &'static str {
    match preimage {
        UpdatePreimage::Entity(_) => "entity",
        UpdatePreimage::Note(_) => "note",
        UpdatePreimage::Edge(_) => "edge",
    }
}

/// Removes one record with its required, full stage-time preimage.
///
/// Deserialization requires `target_id` to equal the embedded record ID.
/// See `crates/khive-changeset/docs/api/delete-and-merge.md` for strictness rules.
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

/// Full prior record state, tagged by `substrate` to avoid its embedded `kind` field.
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

/// Strict deserialize-only mirror of [`DeletePreimage`].
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

/// Merges two entities with required participant and incident-edge preimages.
///
/// Deserialization matches both IDs and requires every incident edge to touch
/// at least one participant.
/// See `crates/khive-changeset/docs/api/delete-and-merge.md` for the full contract.
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

/// Full prior state required to validate and rewire an entity merge.
///
/// See `crates/khive-changeset/docs/api/delete-and-merge.md` for edge coverage rules.
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

    #[test]
    fn update_op_rejects_preimage_missing_touched_field() {
        let json = serde_json::json!({
            "target_id": "00000000-0000-0000-0000-000000000001",
            "patch": { "target": "entity", "name": "new-name" },
            "preimage": { "target": "entity" }
        });
        let result: Result<UpdateOp, _> = serde_json::from_value(json);
        assert!(result.is_err());
    }

    #[test]
    fn update_op_rejects_preimage_with_extra_untouched_field() {
        let json = serde_json::json!({
            "target_id": "00000000-0000-0000-0000-000000000001",
            "patch": { "target": "entity", "tags": ["a"] },
            "preimage": {
                "target": "entity",
                "name": "stale-prior-name",
                "tags": ["prior-a"]
            }
        });
        let result: Result<UpdateOp, _> = serde_json::from_value(json);
        assert!(result.is_err());
    }

    #[test]
    fn update_op_rejects_mismatched_target_variant() {
        let json = serde_json::json!({
            "target_id": "00000000-0000-0000-0000-000000000001",
            "patch": { "target": "entity", "name": "n" },
            "preimage": { "target": "note", "content": "prior" }
        });
        let result: Result<UpdateOp, _> = serde_json::from_value(json);
        assert!(result.is_err());
    }

    #[test]
    fn update_op_explicit_null_clear_captures_prior_value() {
        let json = serde_json::json!({
            "target_id": "00000000-0000-0000-0000-000000000001",
            "patch": { "target": "entity", "description": null },
            "preimage": { "target": "entity", "description": "was-set" }
        });
        let op: UpdateOp = serde_json::from_value(json).unwrap();
        match op.patch {
            UpdatePatch::Entity(p) => assert_eq!(p.description, Some(None)),
            other => panic!("expected entity patch, got {other:?}"),
        }
        match op.preimage {
            UpdatePreimage::Entity(pre) => {
                assert_eq!(pre.description, Some(Some("was-set".to_string())))
            }
            other => panic!("expected entity preimage, got {other:?}"),
        }
    }

    #[test]
    fn update_op_accepts_congruent_preimage() {
        let json = serde_json::json!({
            "target_id": "00000000-0000-0000-0000-000000000001",
            "patch": { "target": "edge", "weight": 0.5 },
            "preimage": { "target": "edge", "weight": 0.9 }
        });
        let result: Result<UpdateOp, _> = serde_json::from_value(json);
        assert!(result.is_ok());
    }

    #[test]
    fn update_op_rejects_out_of_range_preimage_weight() {
        let json = serde_json::json!({
            "target_id": "00000000-0000-0000-0000-000000000001",
            "patch": { "target": "edge", "weight": 0.5 },
            "preimage": { "target": "edge", "weight": 1.5 }
        });
        let result: Result<UpdateOp, _> = serde_json::from_value(json);
        assert!(result.is_err());
    }

    #[test]
    fn update_op_rejects_out_of_range_preimage_salience() {
        let json = serde_json::json!({
            "target_id": "00000000-0000-0000-0000-000000000001",
            "patch": { "target": "note", "salience": 0.2 },
            "preimage": { "target": "note", "salience": 1.9 }
        });
        let result: Result<UpdateOp, _> = serde_json::from_value(json);
        assert!(result.is_err());
    }

    #[test]
    fn update_op_new_rejects_incongruent_construction() {
        let result = UpdateOp::new(
            Id128::from_u128(1),
            UpdatePatch::Entity(EntityPatch {
                name: Some("new-name".to_string()),
                description: None,
                properties: None,
                tags: None,
            }),
            UpdatePreimage::Entity(EntityPreimage {
                name: None,
                description: None,
                properties: None,
                tags: None,
            }),
        );
        assert!(result.is_err());
    }
}

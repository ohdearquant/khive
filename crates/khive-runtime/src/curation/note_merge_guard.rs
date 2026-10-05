//! Guard for a note merge whose caller planned it from an earlier read.
//!
//! A caller that groups notes, then merges one pair at a time, needs the merge
//! to refuse when the world it planned against has moved. Every check here runs
//! on the merge's own writer connection, inside the merge's own transaction,
//! after both notes were read there and before the first write, so a refusal
//! changes no note, edge, index entry or event. The checks form a closed set
//! evaluated with runtime-owned statements; a caller supplies values, never SQL
//! or code.

use super::*;

/// Longest serialized `annotation` a guard may carry.
const MAX_ANNOTATION_BYTES: usize = 4 * 1024;

/// Most merge records an `EntityLineageReaches` walk follows.
const MAX_LINEAGE_STEPS: usize = 64;

/// A fact the caller relied on when it planned the merge, re-checked inside the
/// merge transaction. The first one that does not hold refuses the merge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeAssertion {
    /// The entity exists in the caller's namespace, is not deleted and was not
    /// merged away.
    EntityLive { entity: Uuid },
    /// `canonical` is live in the caller's namespace (see
    /// [`MergeAssertion::EntityLive`]), and either `entity` equals `canonical` or
    /// the walk that starts at `entity` and follows, in the caller's namespace,
    /// the record each merged-away entity keeps of the entity it was merged into
    /// arrives at `canonical` within 64 steps. A missing record, a deleted entity
    /// with no such record, or a cycle ends the walk without holding. A
    /// `canonical` that was itself merged away or deleted never holds, even when
    /// the walk passes through it.
    EntityLineageReaches { entity: Uuid, canonical: Uuid },
    /// A live `relation` edge from `note` to `target` exists in the caller's
    /// namespace, with its target in this store.
    NoteEdgeTo {
        note: Uuid,
        relation: EdgeRelation,
        target: Uuid,
    },
    /// `allowed` is a live entity of kind `target_kind` in the caller's namespace,
    /// and every live `relation` edge stored under the caller's namespace whose
    /// source is `note` and whose target is a live entity of kind `target_kind`
    /// in the caller's namespace targets `allowed`. Only the caller's
    /// namespace's edge rows and entities are read: an edge row stored under
    /// another namespace, and an edge to any other target (a note, an event, an
    /// edge, a deleted entity, an entity of another kind, anything in another
    /// namespace or another store, or no record) does not count, so the outcome
    /// depends on no record outside the caller's namespace.
    NoteEdgeTargetsWithin {
        note: Uuid,
        relation: EdgeRelation,
        target_kind: String,
        allowed: Uuid,
    },
}

impl MergeAssertion {
    fn name(&self) -> &'static str {
        match self {
            Self::EntityLive { .. } => "EntityLive",
            Self::EntityLineageReaches { .. } => "EntityLineageReaches",
            Self::NoteEdgeTo { .. } => "NoteEdgeTo",
            Self::NoteEdgeTargetsWithin { .. } => "NoteEdgeTargetsWithin",
        }
    }

    fn holds(
        &self,
        conn: &rusqlite::Connection,
        namespace: &str,
        budget: &mut MergeTxBudget,
    ) -> Result<bool, MergeSqlError> {
        match self {
            Self::EntityLive { entity } => entity_is_live(conn, namespace, *entity, budget),
            Self::EntityLineageReaches { entity, canonical } => {
                lineage_reaches(conn, namespace, *entity, *canonical, budget)
            }
            Self::NoteEdgeTo {
                note,
                relation,
                target,
            } => {
                budget.charge(1, 128, "checking a merge assertion edge")?;
                Ok(conn.query_row(
                    crate::sql!("merge_guard_note_edge_select"),
                    rusqlite::params![
                        namespace,
                        relation.as_str(),
                        note.to_string(),
                        target.to_string(),
                    ],
                    |row| row.get(0),
                )?)
            }
            Self::NoteEdgeTargetsWithin {
                note,
                relation,
                target_kind,
                allowed,
            } => {
                budget.charge(1, 128, "checking a merge assertion allowed target")?;
                let allowed_kind: Option<String> = conn
                    .query_row(
                        crate::sql!("merge_guard_entity_live_kind_select"),
                        rusqlite::params![allowed.to_string(), namespace],
                        |row| row.get(0),
                    )
                    .optional()?;
                if allowed_kind.as_deref() != Some(target_kind.as_str()) {
                    return Ok(false);
                }
                let mut stmt = conn.prepare(crate::sql!("merge_guard_note_edge_targets_select"))?;
                let mut rows = stmt.query(rusqlite::params![
                    note.to_string(),
                    relation.as_str(),
                    namespace,
                    target_kind,
                ])?;
                while let Some(row) = rows.next()? {
                    budget.charge(1, 128, "checking merge assertion edge targets")?;
                    // Counted only when the statement found a live entity of the
                    // kind in the caller's namespace; every other target, whatever
                    // it is, reads as not counted.
                    let counted: bool = row.get(1)?;
                    if counted {
                        let target: String = row.get(0)?;
                        match Uuid::parse_str(&target) {
                            Ok(target) if target == *allowed => {}
                            _ => return Ok(false),
                        }
                    }
                }
                Ok(true)
            }
        }
    }
}

fn entity_is_live(
    conn: &rusqlite::Connection,
    namespace: &str,
    entity: Uuid,
    budget: &mut MergeTxBudget,
) -> Result<bool, MergeSqlError> {
    budget.charge(1, 128, "checking a merge assertion entity")?;
    Ok(conn.query_row(
        crate::sql!("merge_guard_entity_live_select"),
        rusqlite::params![entity.to_string(), namespace],
        |row| row.get(0),
    )?)
}

fn lineage_reaches(
    conn: &rusqlite::Connection,
    namespace: &str,
    entity: Uuid,
    canonical: Uuid,
    budget: &mut MergeTxBudget,
) -> Result<bool, MergeSqlError> {
    if !entity_is_live(conn, namespace, canonical, budget)? {
        return Ok(false);
    }
    let mut seen = HashSet::new();
    let mut current = entity;
    for step in 0..=MAX_LINEAGE_STEPS {
        if current == canonical {
            return Ok(true);
        }
        if step == MAX_LINEAGE_STEPS || !seen.insert(current) {
            return Ok(false);
        }
        budget.charge(1, 128, "following merge assertion lineage")?;
        let merged_into: Option<Option<String>> = conn
            .query_row(
                crate::sql!("merge_guard_entity_merged_into_select"),
                rusqlite::params![current.to_string(), namespace],
                |row| row.get(0),
            )
            .optional()?;
        match merged_into
            .flatten()
            .and_then(|value| Uuid::parse_str(&value).ok())
        {
            Some(next) => current = next,
            None => return Ok(false),
        }
    }
    Ok(false)
}

/// What the caller read, and what it requires to still be true, for one merge.
#[derive(Clone, Debug)]
pub struct NoteMergeGuard {
    /// The stored version of the survivor the caller read.
    pub into_version: i64,
    /// The stored version of the merged-away note the caller read.
    pub from_version: i64,
    /// Facts to re-check inside the merge transaction, evaluated in order.
    pub assertions: Vec<MergeAssertion>,
    /// Top-level property keys and values that replace the survivor's after the
    /// field-level merge rules ran and before the provenance entry is appended.
    pub survivor_properties: serde_json::Map<String, Value>,
    /// One JSON value, at most 4 KiB serialized, recorded unchanged under
    /// `annotation` in the `_merge_history` entry this merge appends.
    pub annotation: Option<Value>,
}

/// The outcome of a guarded merge. `kept_version` is the survivor's stored
/// version after the merge, so the caller can merge the next duplicate into the
/// same survivor without reading it again. A dry run commits nothing and
/// reports the version the survivor currently has.
#[derive(Clone, Debug)]
pub struct GuardedNoteMerge {
    pub summary: MergeSummary,
    pub kept_version: i64,
}

fn refusal(message: String) -> MergeSqlError {
    MergeSqlError::Refusal(RuntimeError::Khive(KhiveError::conflict(message)))
}

impl NoteMergeGuard {
    /// The checks that read no stored state. They run before the transaction
    /// starts, so a malformed guard never holds the writer.
    pub(super) fn check_values(&self) -> RuntimeResult<()> {
        if self.survivor_properties.contains_key("_merge_history") {
            return Err(RuntimeError::InvalidInput(
                "a guarded merge cannot override `_merge_history`: the merge owns its provenance"
                    .into(),
            ));
        }
        let overrides = Value::Object(self.survivor_properties.clone());
        crate::secret_gate::reject_reserved_secret_gate_property(Some(&overrides))?;
        // The whole map, so a credential in a key is refused with the values.
        crate::secret_gate::check_json_at(&overrides, "note", "properties")?;
        if let Some(annotation) = &self.annotation {
            let bytes = serde_json::to_vec(annotation)
                .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?
                .len();
            if bytes > MAX_ANNOTATION_BYTES {
                return Err(RuntimeError::InvalidInput(format!(
                    "merge annotation is {bytes} bytes serialized; the limit is \
                     {MAX_ANNOTATION_BYTES}"
                )));
            }
            crate::secret_gate::check_json_at(annotation, "note", "merge_annotation")?;
        }
        Ok(())
    }

    /// Versions, then assertions in order, against the transaction's own view.
    pub(super) fn enforce(
        &self,
        conn: &rusqlite::Connection,
        namespace: &str,
        into: &Note,
        from: &Note,
        budget: &mut MergeTxBudget,
    ) -> Result<(), MergeSqlError> {
        for (note, expected) in [(into, self.into_version), (from, self.from_version)] {
            if note.version != expected {
                return Err(MergeSqlError::Refusal(stale_note_snapshot_error(note.id)));
            }
        }
        for (index, assertion) in self.assertions.iter().enumerate() {
            if !assertion.holds(conn, namespace, budget)? {
                return Err(refusal(format!(
                    "guarded note merge refused: assertion {index} ({}) does not hold",
                    assertion.name()
                )));
            }
        }
        Ok(())
    }

    /// Apply the property override and the history annotation to the properties
    /// the field-level rules produced, after the keys the merge keeps from the
    /// survivor were restored and before the provenance entry is appended.
    pub(super) fn apply_to_survivor(
        &self,
        kind: &str,
        preserve_owner_established: bool,
        properties: &mut Option<Value>,
        history_entry: &mut Value,
    ) -> Result<(), MergeSqlError> {
        if let Some(key) = self.survivor_properties.keys().find(|key| {
            kind_owned_properties(kind).contains(&key.as_str())
                || (preserve_owner_established
                    && OWNER_ESTABLISHED_PROPERTIES.contains(&key.as_str()))
        }) {
            return Err(MergeSqlError::Refusal(RuntimeError::InvalidInput(format!(
                "`{key}` is owned by `{kind}` notes and cannot be overridden by a guarded merge"
            ))));
        }
        if !self.survivor_properties.is_empty() {
            // No properties at all is an empty object the override can fill: the
            // unguarded merge builds one for its own history entry as well.
            let Value::Object(object) =
                properties.get_or_insert_with(|| Value::Object(serde_json::Map::new()))
            else {
                return Err(refusal(
                    "guarded note merge refused: the merged properties are not an object".into(),
                ));
            };
            object.extend(
                self.survivor_properties
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
        }
        // The unguarded merge drops its own provenance entry when the history
        // the field-level rules left is not an array (under `PreferFrom` it can
        // come from the absorbed note). A guarded merge refuses instead.
        if properties
            .as_ref()
            .and_then(|props| props.get("_merge_history"))
            .is_some_and(|history| !history.is_array())
        {
            return Err(refusal(
                "guarded note merge refused: `_merge_history` is not an array".into(),
            ));
        }
        if let (Some(annotation), Some(entry)) = (&self.annotation, history_entry.as_object_mut()) {
            entry.insert("annotation".into(), annotation.clone());
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "note_merge_guard_tests.rs"]
mod tests;

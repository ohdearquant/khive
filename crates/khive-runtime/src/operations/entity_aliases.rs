//! Caller aliases on entities (ADR-221 section 2).
//!
//! An alias is another exact name of an entity. A caller names aliases on
//! `create`, changes them with `update(add_aliases, remove_aliases)`, and a
//! keyed reload replaces them. Each alias row carries the origin that wrote
//! it, and a caller request reaches caller-origin rows only.

use std::collections::BTreeSet;

use super::*;
use khive_db::stores::entity::{
    entity_alias_insert_statement, entity_caller_alias_delete_statement,
    entity_name_alias_delete_statement,
};
use khive_storage::entity::{EntityAlias, EntityAliasOrigin};

/// The refusal text for an empty alias, one over 512 UTF-8 bytes, or one
/// holding U+0000.
pub(crate) const ENTITY_ALIAS_REFUSAL: &str =
    "alias must be non-empty, at most 512 UTF-8 bytes and must not contain U+0000";

/// Validate one caller alias and scan its exact bytes with the secret gate.
///
/// Callers run this before any read, so a refused alias never reaches one.
pub fn validate_entity_alias(alias: &str) -> RuntimeResult<()> {
    if alias.is_empty() || alias.len() > 512 || alias.contains('\0') {
        return Err(RuntimeError::InvalidInput(ENTITY_ALIAS_REFUSAL.into()));
    }
    crate::secret_gate::check_at(alias, "entity", "aliases")
}

fn validated_set(aliases: &[String]) -> RuntimeResult<BTreeSet<String>> {
    for alias in aliases {
        validate_entity_alias(alias)?;
    }
    Ok(aliases.iter().cloned().collect())
}

/// A change to an entity's caller-origin aliases, validated when it is made.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EntityAliasChange {
    add: BTreeSet<String>,
    remove: BTreeSet<String>,
    replace: bool,
    /// `keep_old_name: false`: a rename keeps no `rename` alias.
    drop_old_name: bool,
}

impl EntityAliasChange {
    /// `update(add_aliases, remove_aliases)`. One alias may not be both
    /// added and removed by the same call.
    pub fn update(add: &[String], remove: &[String]) -> RuntimeResult<Self> {
        let add = validated_set(add)?;
        let remove = validated_set(remove)?;
        if let Some(both) = add.intersection(&remove).next() {
            return Err(RuntimeError::InvalidInput(format!(
                "alias {both:?} is both added and removed"
            )));
        }
        Ok(Self {
            add,
            remove,
            replace: false,
            drop_old_name: false,
        })
    }

    /// The entity's caller aliases become exactly `aliases`, as a create and
    /// a keyed reload write them. Rename- and merge-origin aliases stay.
    pub fn replace(aliases: &[String]) -> RuntimeResult<Self> {
        Ok(Self {
            add: validated_set(aliases)?,
            remove: BTreeSet::new(),
            replace: true,
            drop_old_name: false,
        })
    }

    /// `keep_old_name`: whether a write that renames the entity keeps the
    /// old name as a `rename` alias (ADR-221 section 3). Kept by default.
    pub fn keeping_old_name(mut self, keep: bool) -> Self {
        self.drop_old_name = !keep;
        self
    }

    /// Whether the change asks for nothing.
    pub fn is_empty(&self) -> bool {
        self.add.is_empty() && self.remove.is_empty() && !self.replace
    }

    /// The requested aliases equal to `name`, which are not stored.
    pub fn not_stored(&self, name: &str) -> Vec<String> {
        self.add
            .iter()
            .filter(|alias| *alias == name)
            .cloned()
            .collect()
    }

    /// The caller aliases an entity named `name` holds once a replacing
    /// change is applied to it.
    pub(crate) fn wanted(&self, name: &str) -> BTreeSet<&str> {
        self.add
            .iter()
            .map(String::as_str)
            .filter(|alias| *alias != name)
            .collect()
    }
}

/// What an alias change asked for and did not do.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct EntityAliasReport {
    /// Requested aliases equal to the entity's name, which are not stored.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub not_stored: Vec<String>,
    /// Requested removals the entity holds no caller alias for. Aliases a
    /// rename or a merge wrote are listed here too: a caller cannot remove
    /// them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub not_removed: Vec<String>,
}

impl EntityAliasReport {
    /// Whether every requested alias was stored or removed.
    pub fn is_empty(&self) -> bool {
        self.not_stored.is_empty() && self.not_removed.is_empty()
    }
}

/// The caller-origin aliases `entity` holds.
pub(crate) fn caller_alias_set(entity: &Entity) -> BTreeSet<&str> {
    entity
        .aliases
        .iter()
        .filter(|held| held.origin == EntityAliasOrigin::Caller)
        .map(|held| held.alias.as_str())
        .collect()
}

/// Apply `change` to `entity.aliases` once the entity's name is final, and
/// return whether the aliases changed, the statements that write the change
/// at time `at`, and what the change left out.
///
/// `previous_name` is the name the stored row had before this write, `None`
/// for a row this write creates. When the write renames the entity, the old
/// name becomes a `rename` alias unless the change drops it, the entity
/// already holds it, or the old name is empty.
///
/// The projection is what the statements leave in `entity_aliases`, so the
/// text and vector bodies built from `entity` index the aliases the stored
/// row has.
pub(crate) fn apply_entity_alias_change(
    entity: &mut Entity,
    change: &EntityAliasChange,
    at: i64,
    previous_name: Option<&str>,
) -> (bool, Vec<SqlStatement>, EntityAliasReport) {
    let before = entity.aliases.clone();
    let mut report = EntityAliasReport {
        not_stored: change.not_stored(&entity.name),
        not_removed: Vec::new(),
    };
    let wanted: BTreeSet<String> = change
        .wanted(&entity.name)
        .into_iter()
        .map(str::to_owned)
        .collect();
    let name = entity.name.clone();
    // An alias never equals its entity's name: an entity renamed to an alias
    // it holds loses that row, whatever wrote it. That delete is its own
    // statement; the caller's removals reach caller-origin rows only.
    let renamed_onto = entity.aliases.iter().any(|held| held.alias == name);
    let held = caller_alias_set(entity);
    let removed: Vec<String> = if change.replace {
        held.iter()
            .filter(|alias| !wanted.contains(**alias) && **alias != name)
            .map(|alias| (*alias).to_owned())
            .collect()
    } else {
        let (removed, not_removed): (Vec<String>, Vec<String>) =
            change.remove.iter().cloned().partition(|alias| {
                held.contains(alias.as_str()) || (renamed_onto && *alias == name)
            });
        report.not_removed = not_removed;
        removed.into_iter().filter(|alias| *alias != name).collect()
    };
    let namespace = entity.namespace.clone();
    let mut statements: Vec<SqlStatement> = removed
        .iter()
        .map(|alias| entity_caller_alias_delete_statement(&namespace, entity.id, alias))
        .collect();
    if renamed_onto {
        statements.push(entity_name_alias_delete_statement(
            &namespace, entity.id, &name,
        ));
    }
    entity.aliases.retain(|held| {
        held.alias != name
            && (held.origin != EntityAliasOrigin::Caller || !removed.contains(&held.alias))
    });
    for alias in wanted {
        if entity.aliases.iter().any(|held| held.alias == alias) {
            continue;
        }
        statements.push(entity_alias_insert_statement(
            &namespace,
            entity.id,
            &alias,
            EntityAliasOrigin::Caller,
            at,
        ));
        entity.aliases.push(EntityAlias {
            alias,
            origin: EntityAliasOrigin::Caller,
        });
    }
    // ADR-221 section 3: a rename keeps the old name as a `rename` alias in
    // the same transaction. A caller alias of that string named by this same
    // change was written above and is left as it is.
    if let Some(previous) =
        previous_name.filter(|previous| *previous != name && !previous.is_empty())
    {
        if !change.drop_old_name && !entity.aliases.iter().any(|held| held.alias == previous) {
            statements.push(entity_alias_insert_statement(
                &namespace,
                entity.id,
                previous,
                EntityAliasOrigin::Rename,
                at,
            ));
            entity.aliases.push(EntityAlias {
                alias: previous.to_owned(),
                origin: EntityAliasOrigin::Rename,
            });
        }
    }
    entity.aliases.sort();
    (entity.aliases != before, statements, report)
}

/// The `entity_updated` event payload. A write that renames the entity names
/// the previous name and the new one, so the record's history keeps the old
/// name even when no alias is kept (ADR-221 section 3).
pub(crate) fn entity_updated_payload(
    entity: &Entity,
    changed_fields: &[&str],
    previous_name: Option<&str>,
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "id": entity.id,
        "namespace": entity.namespace,
        "changed_fields": changed_fields,
    });
    if let Some(previous) = previous_name.filter(|previous| *previous != entity.name) {
        payload["previous_name"] = serde_json::Value::from(previous);
        payload["name"] = serde_json::Value::from(entity.name.as_str());
    }
    payload
}

/// The statements as plan statements: an alias row affects one row or none,
/// so none is guarded.
pub(crate) fn unguarded(statements: Vec<SqlStatement>) -> impl Iterator<Item = PlanStatement> {
    statements.into_iter().map(|statement| PlanStatement {
        statement,
        guard: None,
    })
}

#[cfg(test)]
#[path = "entity_aliases_tests.rs"]
mod tests;

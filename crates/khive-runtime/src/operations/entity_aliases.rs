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
        })
    }

    /// The entity's caller aliases become exactly `aliases`, as a create and
    /// a keyed reload write them. Rename- and merge-origin aliases stay.
    pub fn replace(aliases: &[String]) -> RuntimeResult<Self> {
        Ok(Self {
            add: validated_set(aliases)?,
            remove: BTreeSet::new(),
            replace: true,
        })
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
/// The projection is what the statements leave in `entity_aliases`, so the
/// text and vector bodies built from `entity` index the aliases the stored
/// row has.
pub(crate) fn apply_entity_alias_change(
    entity: &mut Entity,
    change: &EntityAliasChange,
    at: i64,
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
    let held = caller_alias_set(entity);
    let removed: Vec<String> = if change.replace {
        held.iter()
            .filter(|alias| !wanted.contains(**alias))
            .map(|alias| (*alias).to_owned())
            .collect()
    } else {
        let (removed, not_removed): (Vec<String>, Vec<String>) = change
            .remove
            .iter()
            .cloned()
            .partition(|alias| held.contains(alias.as_str()));
        report.not_removed = not_removed;
        // An alias never equals its entity's name: a patch that renames the
        // entity to one of its caller aliases removes that alias.
        let mut removed = removed;
        if held.contains(entity.name.as_str()) && !removed.contains(&entity.name) {
            removed.push(entity.name.clone());
        }
        removed
    };
    let namespace = entity.namespace.clone();
    let mut statements: Vec<SqlStatement> = removed
        .iter()
        .map(|alias| entity_caller_alias_delete_statement(&namespace, entity.id, alias))
        .collect();
    entity
        .aliases
        .retain(|held| held.origin != EntityAliasOrigin::Caller || !removed.contains(&held.alias));
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
    entity.aliases.sort();
    (entity.aliases != before, statements, report)
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

//! Keys and aliases on archive entities (ADR-221 section 4).
//!
//! The portable archive and the NDJSON snapshot carry each entity's held
//! keys, as plain strings under the entity's own kind, and its aliases with
//! the origin that wrote each. Importing an archive over an entity that is
//! already stored adds what the archive carries and removes nothing.

use std::collections::{hash_map::Entry, BTreeSet, HashMap};

use khive_db::stores::entity::{
    entity_alias_insert_statement, entity_key_claim_statement, entity_name_alias_delete_statement,
    entity_upsert_statement, ENTITY_KEY_HOLDER_SQL, NAMESPACE_KEY_ROWS_SQL,
};
use khive_storage::entity::EntityAlias;
use khive_storage::types::SqlRow;
use khive_storage::{Entity, SqlStatement, SqlValue};
use khive_types::KhiveError;
use uuid::Uuid;

use crate::atomic_plan::{AffectedRowGuard, PlanStatement};
use crate::atomic_runner::{
    run_prepared_atomic_unit, PreparedAtomicError, PreparedAtomicOp, PreparedAtomicOutcome,
};
use crate::{KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult};

/// Validate the keys and aliases one archive entity carries, before any
/// read: each key and each alias as a caller write validates it, none listed
/// twice, and no alias equal to the entity's name.
pub fn validate_entity_names(
    id: Uuid,
    name: &str,
    keys: &[String],
    aliases: &[EntityAlias],
) -> RuntimeResult<()> {
    let mut seen_keys = BTreeSet::new();
    for key in keys {
        crate::operations::validate_entity_key(key)?;
        if !seen_keys.insert(key.as_str()) {
            return Err(RuntimeError::InvalidInput(format!(
                "archive entity {id} lists key {key:?} more than once"
            )));
        }
    }
    let mut seen_aliases = BTreeSet::new();
    for alias in aliases {
        crate::operations::validate_entity_alias(&alias.alias)?;
        if alias.alias == name {
            return Err(RuntimeError::InvalidInput(format!(
                "archive entity {id} lists its own name as an alias"
            )));
        }
        if !seen_aliases.insert(alias.alias.as_str()) {
            return Err(RuntimeError::InvalidInput(format!(
                "archive entity {id} lists alias {:?} more than once",
                alias.alias
            )));
        }
    }
    Ok(())
}

/// Refuse an archive in which two entities carry the same key under the same
/// kind. Each item is one entity's id, kind and keys.
pub fn validate_archive_key_uniqueness<'a, I>(entities: I) -> RuntimeResult<()>
where
    I: IntoIterator<Item = (Uuid, &'a str, &'a [String])>,
{
    let mut holders = HashMap::<(&'a str, &'a str), Uuid>::new();
    for (id, kind, keys) in entities {
        for key in keys {
            match holders.entry((kind, key.as_str())) {
                Entry::Occupied(first) if *first.get() != id => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "archive entities {} and {id} both carry {kind} key {key:?}",
                        first.get()
                    )));
                }
                Entry::Occupied(_) => {}
                Entry::Vacant(slot) => {
                    slot.insert(id);
                }
            }
        }
    }
    Ok(())
}

/// `import_key_conflict`: archive entity `entity_id` carries `key` under
/// `kind`, and another live entity holds it. The holder is not named.
pub fn import_key_conflict(entity_id: Uuid, kind: &str, key: &str) -> RuntimeError {
    KhiveError::conflict(format!(
        "import_key_conflict: archive entity {entity_id} carries {kind} key {key:?}, \
         which another live entity holds"
    ))
    .with_details(khive_types::Details::new_owned([
        ("reason", "import_key_conflict".to_string()),
        ("entity_id", entity_id.to_string()),
        ("kind", kind.to_owned()),
        ("key", key.to_owned()),
    ]))
    .into()
}

fn column_error(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Internal(format!("read entity keys: {error}"))
}

/// The read of the entity holding `key` under `kind`, if any.
pub(super) fn key_holder_statement(namespace: &str, kind: &str, key: &str) -> SqlStatement {
    SqlStatement::new(
        ENTITY_KEY_HOLDER_SQL,
        vec![
            SqlValue::Text(namespace.to_owned()),
            SqlValue::Text(kind.to_owned()),
            SqlValue::Text(key.to_owned()),
        ],
    )
    .labelled("import-key-holder")
}

/// The holder a [`key_holder_statement`] read names, if any.
pub(super) fn key_holder(rows: &[SqlRow]) -> RuntimeResult<Option<Uuid>> {
    rows.first()
        .map(|row| row.uuid("entity_id"))
        .transpose()
        .map_err(column_error)
}

/// One key row of an entity.
pub(super) struct KeyRow {
    pub(super) kind: String,
    pub(super) key: String,
    /// Whether the key is held, rather than released by a delete.
    pub(super) held: bool,
}

/// Every key row in one namespace, held or released.
pub(super) struct NamespaceKeys {
    /// The live holder of each held `(kind, key)`.
    pub(super) holders: HashMap<(String, String), Uuid>,
    /// Each entity's key rows, ordered by kind then key.
    pub(super) rows: HashMap<Uuid, Vec<KeyRow>>,
}

impl NamespaceKeys {
    /// The keys the rows of entity `id` name under `kind`, held or released.
    pub(super) fn named_by(&self, id: Uuid, kind: &str) -> BTreeSet<&str> {
        self.rows
            .get(&id)
            .into_iter()
            .flatten()
            .filter(|row| row.kind == kind)
            .map(|row| row.key.as_str())
            .collect()
    }
}

/// The writes that add an archive entity's keys and aliases to its row.
pub(super) struct ArchiveNameWrites {
    /// Key claims first, then alias writes.
    pub(super) statements: Vec<PlanStatement>,
    /// The key each leading claim statement takes, in statement order.
    pub(super) claimed: Vec<String>,
}

/// The writes that add an archive entity's keys and aliases to `entity`,
/// written at its `updated_at`, with `entity.aliases` set to what they leave
/// stored (ADR-221 section 4).
///
/// `named` lists the keys the entity's own rows already name under its kind:
/// those stay as they are, and every other key is claimed, guarded to one
/// row, so a key another live entity took since it was read rolls the write
/// back. An alias the entity holds keeps its row and origin; any other is
/// stored with the origin the archive names. Nothing the entity holds is
/// removed, except an alias equal to its name, which an alias never is.
pub(super) fn archive_name_writes(
    entity: &mut Entity,
    keys: &[String],
    aliases: &[EntityAlias],
    named: &BTreeSet<&str>,
) -> ArchiveNameWrites {
    let namespace = entity.namespace.clone();
    let name = entity.name.clone();
    let at = entity.updated_at;
    let mut writes = ArchiveNameWrites {
        statements: Vec::new(),
        claimed: Vec::new(),
    };
    for key in keys {
        if named.contains(key.as_str()) {
            continue;
        }
        writes.statements.push(PlanStatement {
            statement: entity_key_claim_statement(&namespace, &entity.kind, key, entity.id, at),
            guard: Some(AffectedRowGuard::exactly(1)),
        });
        writes.claimed.push(key.clone());
    }
    if entity.aliases.iter().any(|stored| stored.alias == name) {
        writes.statements.push(PlanStatement {
            statement: entity_name_alias_delete_statement(&namespace, entity.id, &name),
            guard: None,
        });
        entity.aliases.retain(|stored| stored.alias != name);
    }
    for alias in aliases {
        if alias.alias == name
            || entity
                .aliases
                .iter()
                .any(|stored| stored.alias == alias.alias)
        {
            continue;
        }
        writes.statements.push(PlanStatement {
            statement: entity_alias_insert_statement(
                &namespace,
                entity.id,
                &alias.alias,
                alias.origin,
                at,
            ),
            guard: None,
        });
        entity.aliases.push(alias.clone());
    }
    entity.aliases.sort();
    writes
}

impl KhiveRuntime {
    /// Every key row in the token's namespace.
    pub(super) async fn namespace_keys(
        &self,
        token: &NamespaceToken,
    ) -> RuntimeResult<NamespaceKeys> {
        let statement = SqlStatement::new(
            NAMESPACE_KEY_ROWS_SQL,
            vec![SqlValue::Text(token.namespace().as_str().to_owned())],
        )
        .labelled("archive-namespace-keys");
        let rows = {
            let mut reader = self.sql().reader().await?;
            reader.query_all(statement).await?
        };
        let mut keys = NamespaceKeys {
            holders: HashMap::new(),
            rows: HashMap::new(),
        };
        for row in &rows {
            let id = row.uuid("entity_id").map_err(column_error)?;
            let kind = row.text("kind").map_err(column_error)?.to_owned();
            let key = row.text("key").map_err(column_error)?.to_owned();
            let held = row.opt_i64("released_at").map_err(column_error)?.is_none();
            if held {
                keys.holders.insert((kind.clone(), key.clone()), id);
            }
            keys.rows
                .entry(id)
                .or_default()
                .push(KeyRow { kind, key, held });
        }
        Ok(keys)
    }

    /// Write one legacy-imported entity row together with its archive keys
    /// and aliases in one transaction. A key another live entity claimed
    /// since it was read refuses with `import_key_conflict` and writes
    /// nothing.
    pub(super) async fn upsert_archive_entity(
        &self,
        entity: &Entity,
        writes: ArchiveNameWrites,
    ) -> RuntimeResult<()> {
        crate::secret_gate::reject_reserved_secret_gate_property(entity.properties.as_ref())?;
        let id = entity.id;
        let kind = entity.kind.clone();
        let upsert = entity_upsert_statement(entity);
        let op: PreparedAtomicOp<(), RuntimeError> = Box::new(move |writer| {
            Box::pin(async move {
                writer.execute(upsert).await?;
                for (index, planned) in writes.statements.into_iter().enumerate() {
                    let affected = writer.execute(planned.statement).await?;
                    if planned
                        .guard
                        .is_some_and(|guard| !guard.holds_for(affected))
                    {
                        let failure = match writes.claimed.get(index) {
                            Some(key) => import_key_conflict(id, &kind, key),
                            None => RuntimeError::Internal(
                                "archive name write affected an unexpected row count".into(),
                            ),
                        };
                        return Err(PreparedAtomicError::Refused {
                            failure,
                            message: "archive key claim refused".into(),
                        });
                    }
                }
                Ok(((), Vec::new()))
            })
        });
        let access = self.sql();
        match run_prepared_atomic_unit(access.as_ref(), op).await? {
            PreparedAtomicOutcome::Committed { .. } => Ok(()),
            PreparedAtomicOutcome::RolledBack(error) => Err(error),
        }
    }

    /// Write the keys and aliases a snapshot gives each entity, into a store
    /// whose rows for those entities hold none yet, in one transaction.
    ///
    /// Snapshot sync rebuilds a store from the snapshot, so the entities end
    /// with exactly the snapshot's keys and aliases (ADR-221 section 4). Each
    /// item is an entity id, its kind, its keys and its aliases; the entity
    /// rows are written first.
    pub async fn write_snapshot_entity_names(
        &self,
        token: &NamespaceToken,
        entities: Vec<(Uuid, String, Vec<String>, Vec<EntityAlias>)>,
    ) -> RuntimeResult<()> {
        let namespace = token.namespace().as_str().to_owned();
        let at = chrono::Utc::now().timestamp_micros();
        let mut statements = Vec::new();
        for (id, kind, keys, aliases) in entities {
            for key in keys {
                let claim = entity_key_claim_statement(&namespace, &kind, &key, id, at);
                statements.push((claim, Some((id, kind.clone(), key))));
            }
            for alias in aliases {
                let insert =
                    entity_alias_insert_statement(&namespace, id, &alias.alias, alias.origin, at);
                statements.push((insert, None));
            }
        }
        if statements.is_empty() {
            return Ok(());
        }
        let op: PreparedAtomicOp<(), RuntimeError> = Box::new(move |writer| {
            Box::pin(async move {
                for (statement, claim) in statements {
                    let affected = writer.execute(statement).await?;
                    if affected != 1 {
                        let failure = match claim {
                            Some((id, kind, key)) => import_key_conflict(id, &kind, &key),
                            None => RuntimeError::InvalidInput(
                                "snapshot alias is already stored for its entity".into(),
                            ),
                        };
                        return Err(PreparedAtomicError::Refused {
                            failure,
                            message: "snapshot name write refused".into(),
                        });
                    }
                }
                Ok(((), Vec::new()))
            })
        });
        let access = self.sql();
        match run_prepared_atomic_unit(access.as_ref(), op).await? {
            PreparedAtomicOutcome::Committed { .. } => Ok(()),
            PreparedAtomicOutcome::RolledBack(error) => Err(error),
        }
    }
}

#[cfg(test)]
#[path = "names_tests.rs"]
mod tests;

//! A whole entity as a change set carries it (ADR-221 section 4).

use khive_types::{Entity, EntityAlias};
use serde::{Deserialize, Deserializer, Serialize};

use crate::strict::StrictEntity;

/// An entity with the caller keys it holds and its aliases.
///
/// Every change-set carrier that copies a whole entity uses this record: a
/// delete preimage, a restore record, both sides of a merge preimage, and the
/// current entities an inversion reads. `keys` are the caller keys the entity
/// holds under its own kind; `aliases` are its aliases with the origin that
/// wrote each one. A change set is a trusted carrier for origins, with the same
/// standing as an archive: they are written as given.
///
/// Both lists are omitted from the wire when empty, so an entity that holds
/// neither serializes exactly as the bare entity record. Comparisons read each
/// list as a set; see [`EntityRecord::same_names`].
#[derive(Clone, Debug, Serialize)]
pub struct EntityRecord {
    /// The entity row.
    #[serde(flatten)]
    pub entity: Entity,
    /// Caller keys the entity holds under its kind.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
    /// Aliases the entity holds, with their origins.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<EntityAlias>,
}

impl EntityRecord {
    /// A record of `entity` holding no keys and no aliases.
    pub fn new(entity: Entity) -> Self {
        Self {
            entity,
            keys: Vec::new(),
            aliases: Vec::new(),
        }
    }

    /// Whether both records hold the same keys and the same aliases with the
    /// same origins, each read as a set.
    pub fn same_names(&self, other: &Self) -> bool {
        same_set(&self.keys, &other.keys) && same_set(&self.aliases, &other.aliases)
    }
}

impl From<Entity> for EntityRecord {
    fn from(entity: Entity) -> Self {
        Self::new(entity)
    }
}

impl<'de> Deserialize<'de> for EntityRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        StrictEntity::deserialize(deserializer).map(Into::into)
    }
}

/// Whether two lists hold the same elements, order aside. A list holding an
/// element twice equals no list holding it once.
pub(crate) fn same_set<T: Ord + Clone>(first: &[T], second: &[T]) -> bool {
    if first.len() != second.len() {
        return false;
    }
    let mut first = first.to_vec();
    let mut second = second.to_vec();
    first.sort();
    second.sort();
    first == second
}

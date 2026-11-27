//! Entity aliases: other exact names of an entity, each with the origin that
//! wrote it (ADR-221 section 2).

use alloc::string::String;

/// Who wrote an entity alias (ADR-221 section 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum EntityAliasOrigin {
    /// A caller named it on `create` or `update(add_aliases=...)`.
    Caller,
    /// A rename kept the entity's previous name.
    Rename,
    /// A merge carried the removed entity's name or aliases.
    Merge,
}

impl EntityAliasOrigin {
    /// The stored spelling of this origin.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Caller => "caller",
            Self::Rename => "rename",
            Self::Merge => "merge",
        }
    }

    /// Parse a stored origin; `None` for any other spelling.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "caller" => Some(Self::Caller),
            "rename" => Some(Self::Rename),
            "merge" => Some(Self::Merge),
            _ => None,
        }
    }
}

/// Another exact name of an entity, with the origin that wrote it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct EntityAlias {
    /// The alias, exactly as stored.
    pub alias: String,
    /// Who wrote it.
    pub origin: EntityAliasOrigin,
}

//! Substrate discriminant — the 3 data types in khive (ADR-004).
//!
//! Full substrate structs live in the sibling modules (`note`, `entity`,
//! `event`). This module provides the discriminant for typed dispatch and
//! persistence.

use core::fmt;
use core::str::FromStr;

/// The 3 substrate types in khive OSS (ADR-004).
///
/// - **Note**: temporal-referential records (observations, insights, decisions)
/// - **Entity**: graph nodes with properties and typed links
/// - **Event**: universal system log — every verb execution produces one
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum SubstrateKind {
    Note = 0,
    Entity = 1,
    Event = 2,
}

pub const SUBSTRATE_COUNT: usize = 3;

impl SubstrateKind {
    pub const ALL: [SubstrateKind; SUBSTRATE_COUNT] = [
        SubstrateKind::Note,
        SubstrateKind::Entity,
        SubstrateKind::Event,
    ];

    #[inline]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Entity => "entity",
            Self::Event => "event",
        }
    }

    #[inline]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Note),
            1 => Some(Self::Entity),
            2 => Some(Self::Event),
            _ => None,
        }
    }
}

impl fmt::Display for SubstrateKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for SubstrateKind {
    type Err = SubstrateError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "note" | "Note" => Ok(Self::Note),
            "entity" | "Entity" => Ok(Self::Entity),
            "event" | "Event" => Ok(Self::Event),
            _ => Err(SubstrateError::UnknownKind),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubstrateError {
    UnknownKind,
}

impl fmt::Display for SubstrateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownKind => f.write_str("unknown substrate kind"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for SubstrateError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_variants() {
        assert_eq!(SubstrateKind::ALL.len(), SUBSTRATE_COUNT);
        for (i, &kind) in SubstrateKind::ALL.iter().enumerate() {
            assert_eq!(kind as u8, i as u8);
            assert_eq!(SubstrateKind::from_u8(i as u8), Some(kind));
        }
    }

    #[test]
    fn parse_roundtrip() {
        for kind in SubstrateKind::ALL {
            let parsed: SubstrateKind = kind.name().parse().unwrap();
            assert_eq!(parsed, kind);
        }
    }

    #[test]
    fn out_of_range() {
        assert_eq!(SubstrateKind::from_u8(3), None);
        assert_eq!(SubstrateKind::from_u8(255), None);
    }
}

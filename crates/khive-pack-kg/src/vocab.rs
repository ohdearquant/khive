//! KG-pack vocabulary — closed enum for the 5 note kinds.
//!
//! Entity kind validation now uses `khive_types::EntityKind` directly.
//! The runtime accepts any String — validation is the pack's responsibility.

use core::fmt;
use std::string::String;

use khive_types::UnknownVariant;

/// Closed taxonomy for note classification (ADR-019).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum NoteKind {
    #[default]
    Observation,
    Insight,
    Question,
    Decision,
    Reference,
}

impl NoteKind {
    pub const ALL: [Self; 5] = [
        Self::Observation,
        Self::Insight,
        Self::Question,
        Self::Decision,
        Self::Reference,
    ];

    pub const NAMES: &'static [&'static str] = &[
        "observation",
        "insight",
        "question",
        "decision",
        "reference",
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Observation => "observation",
            Self::Insight => "insight",
            Self::Question => "question",
            Self::Decision => "decision",
            Self::Reference => "reference",
        }
    }
}

impl fmt::Display for NoteKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl From<NoteKind> for String {
    fn from(k: NoteKind) -> Self {
        String::from(k.name())
    }
}

impl std::str::FromStr for NoteKind {
    type Err = UnknownVariant;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "observation" | "obs" => Ok(Self::Observation),
            "insight" | "finding" => Ok(Self::Insight),
            "question" | "q" => Ok(Self::Question),
            "decision" | "choice" => Ok(Self::Decision),
            "reference" | "ref" | "citation" => Ok(Self::Reference),
            other => Err(UnknownVariant::new("note_kind", other, Self::NAMES)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn note_kind_roundtrip() {
        for kind in NoteKind::ALL {
            let parsed = NoteKind::from_str(kind.name()).unwrap();
            assert_eq!(parsed, kind);
        }
    }

    #[test]
    fn note_kind_aliases() {
        assert_eq!(NoteKind::from_str("obs").unwrap(), NoteKind::Observation);
        assert_eq!(NoteKind::from_str("ref").unwrap(), NoteKind::Reference);
    }
}

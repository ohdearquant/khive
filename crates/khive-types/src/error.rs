//! Unified error types for khive-types.
//!
//! All parse/validation errors from closed taxonomies (EntityKind, NoteKind,
//! EdgeRelation, SubstrateKind) and ID parsing converge here.

extern crate alloc;
use alloc::string::String;
use core::fmt;

/// A variant string was not recognized in a closed taxonomy.
///
/// Carries the rejected input, the domain name, and the list of valid values
/// so callers get actionable error messages without re-matching.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownVariant {
    pub domain: &'static str,
    pub value: String,
    pub valid: &'static [&'static str],
}

impl UnknownVariant {
    pub fn new(
        domain: &'static str,
        value: impl Into<String>,
        valid: &'static [&'static str],
    ) -> Self {
        Self {
            domain,
            value: value.into(),
            valid,
        }
    }
}

impl fmt::Display for UnknownVariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown {}: {:?}. Valid: {}",
            self.domain,
            self.value,
            ValidList(self.valid),
        )
    }
}

#[cfg(feature = "std")]
impl std::error::Error for UnknownVariant {}

struct ValidList(&'static [&'static str]);

impl fmt::Display for ValidList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, v) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(" | ")?;
            }
            f.write_str(v)?;
        }
        Ok(())
    }
}

/// Unified error for all type-level validation in khive-types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeError {
    Id(crate::id::ParseIdError),
    Variant(UnknownVariant),
}

impl fmt::Display for TypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Id(e) => write!(f, "id error: {e}"),
            Self::Variant(e) => fmt::Display::fmt(e, f),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for TypeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Id(e) => Some(e),
            Self::Variant(e) => Some(e),
        }
    }
}

impl From<crate::id::ParseIdError> for TypeError {
    fn from(e: crate::id::ParseIdError) -> Self {
        Self::Id(e)
    }
}

impl From<UnknownVariant> for TypeError {
    fn from(e: UnknownVariant) -> Self {
        Self::Variant(e)
    }
}

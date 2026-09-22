//! Attribution to one parsed operation, independent of caller authority.

/// Whether a dispatched operation consumed a resolved request reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum RefResolution {
    /// All argument values were supplied literally.
    Literal,
    /// At least one argument value came from request reference resolution.
    Resolved,
}

impl RefResolution {
    /// Stable event-storage and wire spelling.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Literal => "literal",
            Self::Resolved => "resolved",
        }
    }
}

impl core::str::FromStr for RefResolution {
    type Err = crate::UnknownVariant;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "literal" => Ok(Self::Literal),
            "resolved" => Ok(Self::Resolved),
            _ => Err(crate::UnknownVariant::new(
                "ref_resolution",
                value,
                &["literal", "resolved"],
            )),
        }
    }
}

/// Proven parser position and reference provenance for one request operation.
///
/// Absence of this value means unknown (legacy or non-request work), never a
/// synthetic position zero. Nested synchronous dispatch retains its originating
/// parser position rather than inventing another request operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OperationAttribution {
    /// Zero-based parser position, independent of completion or insertion order.
    pub op_index: u32,
    /// How the operation's argument values were obtained.
    pub ref_resolution: RefResolution,
}

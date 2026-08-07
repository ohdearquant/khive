//! Stable refusal classifications emitted by operator-facing command surfaces.
//!
//! The token spellings are a machine contract. This vocabulary is closed (only
//! the variants below are accepted) and append-only: variants may be added, but
//! an existing token must never be renamed or reused for a different meaning.

use core::fmt;

use crate::UnknownVariant;

/// Stable reason attached to a refused `kkernel exec` invocation or operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "kebab-case"))]
#[non_exhaustive]
pub enum RefusalReason {
    /// The resolved actor was anonymous where attribution was required.
    AnonymousActor,
    /// The resolved actor did not match `--expect-actor`.
    ExpectActorMismatch,
    /// A write was refused by the content secret gate.
    GateRefusal,
    /// `--strict` observed at least one failed or aborted operation.
    StrictOpFailure,
    /// The supplied operation expression could not be parsed.
    ParseError,
    /// The requested verb was unknown or was not loaded.
    VerbRefused,
}

impl RefusalReason {
    /// Every currently defined reason, in documentation order.
    pub const ALL: [Self; 6] = [
        Self::AnonymousActor,
        Self::ExpectActorMismatch,
        Self::GateRefusal,
        Self::StrictOpFailure,
        Self::ParseError,
        Self::VerbRefused,
    ];

    /// Exact machine token written to stderr and JSON envelopes.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AnonymousActor => "anonymous-actor",
            Self::ExpectActorMismatch => "expect-actor-mismatch",
            Self::GateRefusal => "gate-refusal",
            Self::StrictOpFailure => "strict-op-failure",
            Self::ParseError => "parse-error",
            Self::VerbRefused => "verb-refused",
        }
    }

    /// Parse one exact machine token without accepting aliases or case folding.
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "anonymous-actor" => Some(Self::AnonymousActor),
            "expect-actor-mismatch" => Some(Self::ExpectActorMismatch),
            "gate-refusal" => Some(Self::GateRefusal),
            "strict-op-failure" => Some(Self::StrictOpFailure),
            "parse-error" => Some(Self::ParseError),
            "verb-refused" => Some(Self::VerbRefused),
            _ => None,
        }
    }
}

impl fmt::Display for RefusalReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl core::str::FromStr for RefusalReason {
    type Err = UnknownVariant;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_token(value).ok_or_else(|| {
            UnknownVariant::new(
                "refusal_reason",
                value,
                &[
                    "anonymous-actor",
                    "expect-actor-mismatch",
                    "gate-refusal",
                    "strict-op-failure",
                    "parse-error",
                    "verb-refused",
                ],
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_vocabulary_is_exact_and_append_only() {
        let tokens = RefusalReason::ALL.map(RefusalReason::as_str);
        assert_eq!(
            tokens,
            [
                "anonymous-actor",
                "expect-actor-mismatch",
                "gate-refusal",
                "strict-op-failure",
                "parse-error",
                "verb-refused",
            ]
        );
        for (reason, token) in RefusalReason::ALL.into_iter().zip(tokens) {
            assert_eq!(RefusalReason::from_token(token), Some(reason));
            assert_eq!(token.parse::<RefusalReason>().unwrap(), reason);
        }
        assert_eq!(RefusalReason::from_token("Gate-Refusal"), None);
        assert_eq!(RefusalReason::from_token("gate_refusal"), None);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_uses_the_machine_token() {
        let encoded = serde_json::to_string(&RefusalReason::GateRefusal).unwrap();
        assert_eq!(encoded, "\"gate-refusal\"");
        assert_eq!(
            serde_json::from_str::<RefusalReason>(&encoded).unwrap(),
            RefusalReason::GateRefusal
        );
    }
}

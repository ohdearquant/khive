//! Shared UUID-or-prefix resolution for pack handlers.

use uuid::Uuid;

use crate::error::{RuntimeError, RuntimeResult};
use crate::runtime::{KhiveRuntime, NamespaceToken};

impl KhiveRuntime {
    /// Parse a full UUID or resolve a compact hexadecimal prefix in the caller's
    /// primary namespace, using the same lookup policy as [`Self::resolve_prefix`].
    ///
    /// A parseable full UUID is returned without a lookup or an existence,
    /// liveness or authorization check. Otherwise input must contain at least
    /// eight ASCII hexadecimal characters with no separators or whitespace.
    /// Missing prefixes and invalid shapes return distinct `InvalidInput` errors;
    /// lookup storage and ambiguity errors propagate unchanged.
    pub async fn resolve_uuid_or_prefix(
        &self,
        token: &NamespaceToken,
        s: &str,
    ) -> RuntimeResult<Uuid> {
        self.resolve_uuid_or_prefix_inner(token, s, None).await
    }

    /// Resolve as [`Self::resolve_uuid_or_prefix`], with the comm/schedule
    /// validation messages prefixed by `verb`. Lookup errors, including
    /// ambiguous prefixes and storage failures, propagate unchanged.
    pub async fn resolve_uuid_or_prefix_for_verb(
        &self,
        token: &NamespaceToken,
        s: &str,
        verb: &str,
    ) -> RuntimeResult<Uuid> {
        self.resolve_uuid_or_prefix_inner(token, s, Some(verb))
            .await
    }

    async fn resolve_uuid_or_prefix_inner(
        &self,
        token: &NamespaceToken,
        s: &str,
        verb: Option<&str>,
    ) -> RuntimeResult<Uuid> {
        if let Ok(uuid) = s.parse::<Uuid>() {
            return Ok(uuid);
        }
        if s.len() >= 8 && s.chars().all(|c| c.is_ascii_hexdigit()) {
            return match self.resolve_prefix(token, s).await? {
                Some(uuid) => Ok(uuid),
                None => Err(RuntimeError::InvalidInput(match verb {
                    Some(verb) => format!("{verb}: no record matches prefix: {s:?}"),
                    None => format!("no record matches prefix: {s:?}"),
                })),
            };
        }
        Err(RuntimeError::InvalidInput(match verb {
            Some(verb) => {
                format!("{verb}: invalid id {s:?}; expected full UUID or 8-char hex prefix")
            }
            None => format!("invalid UUID (expected full UUID or 8+ hex prefix): {s:?}"),
        }))
    }
}

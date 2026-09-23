//! Namespace narrowing shared by all web verbs.

use khive_runtime::{Namespace, NamespaceToken, RuntimeError};

/// A `namespace` argument narrows capability, never elevates it: it must
/// equal the token's own namespace (mirrors `khive-pack-knowledge`'s
/// `knowledge.compose` handling of the same shape of argument).
pub(crate) fn resolve_effective_token(
    token: &NamespaceToken,
    namespace: Option<&str>,
) -> Result<NamespaceToken, RuntimeError> {
    match namespace {
        None => Ok(token.clone()),
        Some(ns_str) => {
            let ns = Namespace::parse(ns_str).map_err(|error| {
                RuntimeError::InvalidInput(format!("invalid namespace {ns_str:?}: {error}"))
            })?;
            if &ns != token.namespace() {
                return Err(RuntimeError::InvalidInput(
                    "web namespace does not match the authorized token namespace".to_string(),
                ));
            }
            Ok(token.with_namespace(ns))
        }
    }
}

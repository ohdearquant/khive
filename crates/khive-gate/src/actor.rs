use serde::{Deserialize, Serialize};

use crate::GateValidationError;

/// Kind prefixes reserved by runtime event attribution and its identity fixtures.
///
/// Add newly stamped kinds here and extend the per-kind event scope regressions.
/// This is not a closed taxonomy: [`ActorRef::try_new`] accepts any non-empty kind.
pub const RUNTIME_STAMPED_ACTOR_KINDS: &[&str] = &["actor", "anonymous", "agent"];

/// Split a `kind:id` label when `kind` is one of [`RUNTIME_STAMPED_ACTOR_KINDS`].
///
/// Returns `None` when the label has no `:` or its prefix is not a stamped kind, so an id that
/// merely contains a colon (such as `svc:build`) is left whole.
pub fn split_stamped_label(label: &str) -> Option<(&str, &str)> {
    label
        .split_once(':')
        .filter(|(kind, _)| RUNTIME_STAMPED_ACTOR_KINDS.contains(kind))
}

/// Caller identity with non-empty `kind` and `id`, validated on construction and deserialization.
///
/// See `crates/khive-gate/docs/api/policy-types.md`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct ActorRef {
    pub kind: String,
    pub id: String,
}

/// Raw deserialization target for [`ActorRef`] — validated via `TryFrom`.
#[derive(Deserialize)]
struct RawActorRef {
    kind: String,
    id: String,
}

impl TryFrom<RawActorRef> for ActorRef {
    type Error = GateValidationError;

    fn try_from(raw: RawActorRef) -> Result<Self, Self::Error> {
        Self::try_new(raw.kind, raw.id)
    }
}

impl<'de> Deserialize<'de> for ActorRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawActorRef::deserialize(deserializer)?;
        ActorRef::try_from(raw).map_err(serde::de::Error::custom)
    }
}

impl ActorRef {
    /// Create a validated `ActorRef`. Returns `Err` if `kind` or `id` is empty.
    pub fn try_new(
        kind: impl Into<String>,
        id: impl Into<String>,
    ) -> Result<Self, GateValidationError> {
        let kind = kind.into();
        let id = id.into();
        if kind.is_empty() {
            return Err(GateValidationError::EmptyActorKind);
        }
        if id.is_empty() {
            return Err(GateValidationError::EmptyActorId);
        }
        Ok(Self { kind, id })
    }

    /// Create a validated `ActorRef`. Panics if `kind` or `id` is empty.
    pub fn new(kind: impl Into<String>, id: impl Into<String>) -> Self {
        Self::try_new(kind, id).expect("ActorRef::new: kind and id must not be empty")
    }

    /// The implicit caller for unauthenticated local usage.
    pub fn anonymous() -> Self {
        Self {
            kind: "anonymous".into(),
            id: "local".into(),
        }
    }

    /// Whether this actor is the implicit anonymous caller.
    pub fn is_anonymous(&self) -> bool {
        self.kind == "anonymous"
    }

    /// Return the explicit binding ID, or `None` for the anonymous caller.
    ///
    /// Anonymous identity must never participate in binding resolution. See
    /// `crates/khive-gate/docs/api/policy-types.md`.
    pub fn binding_id(&self) -> Option<&str> {
        if self.is_anonymous() {
            None
        } else {
            Some(self.id.as_str())
        }
    }

    /// The actor as one label, `kind:id`, except that the plain `actor` kind collapses to its id
    /// so a configured `lambda:khive` reads back as itself.
    pub fn label(&self) -> String {
        if self.kind == "actor" {
            self.id.clone()
        } else {
            format!("{}:{}", self.kind, self.id)
        }
    }
}

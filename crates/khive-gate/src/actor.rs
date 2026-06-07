use serde::{Deserialize, Serialize};

use crate::GateValidationError;

// ---------- Actor ----------

/// Caller identity. `kind` distinguishes user vs agent vs lambda etc.
///
/// Invariant: both `kind` and `id` must be non-empty. Enforced at construction
/// and deserialization via [`serde(try_from)`].
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
}

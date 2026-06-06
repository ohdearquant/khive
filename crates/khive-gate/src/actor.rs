use serde::{Deserialize, Serialize};

// ---------- Actor ----------

/// Caller identity. `kind` distinguishes user vs agent vs lambda etc.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ActorRef {
    pub kind: String,
    pub id: String,
}

impl ActorRef {
    /// Creates an `ActorRef` with the given actor kind and identifier.
    pub fn new(kind: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            id: id.into(),
        }
    }

    /// The implicit caller for unauthenticated local usage.
    pub fn anonymous() -> Self {
        Self {
            kind: "anonymous".into(),
            id: "local".into(),
        }
    }
}

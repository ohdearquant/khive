//! Fold context for parameterizing fold operations

use std::ops::Deref;
use std::sync::Arc;

use chrono::{DateTime, Utc};
#[cfg(feature = "serde")]
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

/// Arc-backed shared JSON value; cheap to clone in hot paths.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SharedJson(Arc<serde_json::Value>);

impl SharedJson {
    /// Create a shared JSON wrapper from an owned JSON value.
    #[must_use]
    pub fn new(value: serde_json::Value) -> Self {
        Self(Arc::new(value))
    }

    /// Borrow the inner JSON value.
    #[must_use]
    pub fn as_value(&self) -> &serde_json::Value {
        self.0.as_ref()
    }

    /// Get mutable access to the JSON value, cloning only when needed.
    pub fn make_mut(&mut self) -> &mut serde_json::Value {
        Arc::make_mut(&mut self.0)
    }

    /// Convert back into an owned JSON value.
    #[must_use]
    pub fn into_inner(self) -> serde_json::Value {
        match Arc::try_unwrap(self.0) {
            Ok(value) => value,
            Err(value) => value.as_ref().clone(),
        }
    }
}

impl Deref for SharedJson {
    type Target = serde_json::Value;

    fn deref(&self) -> &Self::Target {
        self.as_value()
    }
}

impl AsRef<serde_json::Value> for SharedJson {
    fn as_ref(&self) -> &serde_json::Value {
        self.as_value()
    }
}

impl From<serde_json::Value> for SharedJson {
    fn from(value: serde_json::Value) -> Self {
        Self::new(value)
    }
}

impl From<SharedJson> for serde_json::Value {
    fn from(value: SharedJson) -> Self {
        value.into_inner()
    }
}

impl PartialEq<serde_json::Value> for SharedJson {
    fn eq(&self, other: &serde_json::Value) -> bool {
        self.as_value() == other
    }
}

#[cfg(feature = "serde")]
impl Serialize for SharedJson {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.as_value().serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de> Deserialize<'de> for SharedJson {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        serde_json::Value::deserialize(deserializer).map(Self::new)
    }
}

/// Context for fold operations; `as_of` defaults to Unix epoch.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct FoldContext {
    /// Point in time to evaluate (for temporal queries)
    pub as_of: DateTime<Utc>,

    /// Correlation ID for tracing
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub correlation_id: Option<Uuid>,

    /// Additional context as shared JSON.
    #[cfg_attr(feature = "serde", serde(default))]
    pub extra: SharedJson,
}

impl FoldContext {
    /// Create a new context with the Unix epoch as `as_of`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a context for a specific point in time.
    pub fn at(as_of: DateTime<Utc>) -> Self {
        Self {
            as_of,
            ..Default::default()
        }
    }

    /// Set the correlation ID.
    pub fn with_correlation_id(mut self, id: Uuid) -> Self {
        self.correlation_id = Some(id);
        self
    }

    /// Set extra context.
    pub fn with_extra(mut self, extra: impl Into<SharedJson>) -> Self {
        self.extra = extra.into();
        self
    }

    /// Borrow the extra context as a plain `serde_json::Value`.
    #[must_use]
    pub fn extra(&self) -> &serde_json::Value {
        self.extra.as_value()
    }

    /// Mutably access the extra context, cloning the shared payload only if needed.
    pub fn extra_mut(&mut self) -> &mut serde_json::Value {
        self.extra.make_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_at_time() {
        let past = Utc::now() - chrono::Duration::hours(1);
        let ctx = FoldContext::at(past);
        assert_eq!(ctx.as_of, past);
    }

    #[test]
    fn test_context_new_is_epoch() {
        let ctx = FoldContext::new();
        assert_eq!(ctx.as_of, DateTime::<Utc>::default());
    }

    #[cfg(feature = "serde")]
    #[test]
    fn test_shared_json_round_trip() {
        let ctx = FoldContext::new().with_extra(serde_json::json!({"count": 3, "flag": true}));
        let encoded = serde_json::to_string(&ctx).unwrap();
        let decoded: FoldContext = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.extra, serde_json::json!({"count": 3, "flag": true}));
    }

    #[test]
    fn test_shared_json_make_mut() {
        let mut ctx = FoldContext::new().with_extra(serde_json::json!({"count": 1}));
        let _clone = ctx.clone();
        *ctx.extra_mut() = serde_json::json!({"count": 2});
        assert_eq!(ctx.extra, serde_json::json!({"count": 2}));
    }

    #[test]
    fn test_shared_json_clone_is_cheap_arc_refcount() {
        let value = serde_json::json!({"large": "payload", "nested": {"a": 1, "b": 2}});
        let shared = SharedJson::new(value.clone());
        let clone = shared.clone();
        assert_eq!(
            shared.as_value() as *const _,
            clone.as_value() as *const _,
            "clone should share the same Arc allocation"
        );
        assert_eq!(*shared, *clone);
        drop(clone);
        let extracted = shared.into_inner();
        assert_eq!(extracted, value);
    }

    #[test]
    fn test_shared_json_extra_mut_creates_independent_copy() {
        let original = FoldContext::new().with_extra(serde_json::json!({"x": 1}));
        let mut mutated = original.clone();
        assert_eq!(original.extra(), mutated.extra());
        *mutated.extra_mut() = serde_json::json!({"x": 99});
        assert_eq!(original.extra(), &serde_json::json!({"x": 1}));
        assert_eq!(mutated.extra(), &serde_json::json!({"x": 99}));
    }

    #[test]
    fn test_shared_json_from_value_transparent() {
        let value = serde_json::json!([1, 2, 3]);
        let shared: SharedJson = value.clone().into();
        assert_eq!(shared.as_value(), &value);
    }

    #[test]
    fn test_shared_json_default_is_null() {
        let default = SharedJson::default();
        assert_eq!(default.as_value(), &serde_json::Value::Null);
    }
}

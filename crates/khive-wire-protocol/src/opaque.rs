//! Owned JSON tokens for ADR-137 Amendment 1 Decision 9.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{value::RawValue, Value};

/// An immutable opaque JSON value carried by a response or event.
///
/// Received values retain their original JSON token. Locally constructed
/// `Value`s keep serde_json's normal serialization. Parsing is explicit and
/// fallible; no parsed view or caller buffer is retained alongside the token.
#[derive(Clone, Debug)]
pub struct OpaqueValue(Representation);

#[derive(Clone, Debug)]
enum Representation {
    Local(Value),
    Received(Box<RawValue>),
}

impl OpaqueValue {
    /// Return an independent parsed value, without changing this token.
    ///
    /// Received numbers can exceed serde_json's representable range, so this
    /// operation can fail even though decode and re-encode remain valid.
    pub fn to_value(&self) -> serde_json::Result<Value> {
        match &self.0 {
            Representation::Local(value) => Ok(value.clone()),
            Representation::Received(raw) => serde_json::from_str(raw.get()),
        }
    }

    /// The original JSON token for a received value; `None` for local values.
    pub fn received_json(&self) -> Option<&str> {
        match &self.0 {
            Representation::Local(_) => None,
            Representation::Received(raw) => Some(raw.get()),
        }
    }

    pub(crate) fn is_object(&self) -> bool {
        match &self.0 {
            Representation::Local(value) => value.is_object(),
            // RawValue is validated JSON with leading/trailing whitespace
            // outside its token removed. No numeric projection is needed.
            Representation::Received(raw) => raw.get().as_bytes().first() == Some(&b'{'),
        }
    }
}

impl From<Value> for OpaqueValue {
    fn from(value: Value) -> Self {
        Self(Representation::Local(value))
    }
}

impl Serialize for OpaqueValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.0 {
            Representation::Local(value) => value.serialize(serializer),
            Representation::Received(raw) => raw.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for OpaqueValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Box::<RawValue>::deserialize(deserializer).map(|raw| Self(Representation::Received(raw)))
    }
}

impl PartialEq for OpaqueValue {
    fn eq(&self, other: &Self) -> bool {
        if let (Representation::Local(left), Representation::Local(right)) = (&self.0, &other.0) {
            return left == right;
        }
        match (self.to_value(), other.to_value()) {
            (Ok(left), Ok(right)) => left == right,
            (Err(_), Err(_)) => match (&self.0, &other.0) {
                (Representation::Received(left), Representation::Received(right)) => {
                    left.get() == right.get()
                }
                _ => false,
            },
            _ => false,
        }
    }
}

#[cfg(test)]
#[path = "opaque_tests.rs"]
mod tests;

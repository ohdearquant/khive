//! Raw JSON classification before runtime policy (ADR-105 A.5).
use crate::encoding::{CanonicalUuid, ProtocolVersion};
use crate::wire::{required_option, UtcTimestamp};
use serde::{
    de::{MapAccess, SeqAccess, Visitor},
    Deserialize, Deserializer, Serialize,
};
use serde_json::{Map, Value};
use std::{collections::HashSet, fmt};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaintextKind {
    Announce,
    Report,
    Ask,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Plaintext {
    pub v: ProtocolVersion,
    pub subject: Option<String>,
    pub body: String,
    pub sent_at: UtcTimestamp,
    pub thread_id: Option<CanonicalUuid>,
    pub in_reply_to: Option<CanonicalUuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<PlaintextKind>,
}

#[derive(Deserialize)]
struct ParsedPlaintext {
    v: ProtocolVersion,
    #[serde(deserialize_with = "required_option")]
    subject: Option<String>,
    body: String,
    sent_at: UtcTimestamp,
    #[serde(deserialize_with = "required_option")]
    thread_id: Option<CanonicalUuid>,
    #[serde(deserialize_with = "required_option")]
    in_reply_to: Option<CanonicalUuid>,
    #[serde(default)]
    kind: Option<PlaintextKind>,
}
impl ParsedPlaintext {
    fn into_plaintext(self) -> Plaintext {
        Plaintext {
            v: self.v,
            subject: self.subject,
            body: self.body,
            sent_at: self.sent_at,
            thread_id: self.thread_id,
            in_reply_to: self.in_reply_to,
            kind: self.kind,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvalidPlaintextReason {
    NotObject,
    DuplicateMember,
    ReservedIdentityMember,
    InvalidKind,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlaintextClassification {
    Valid(Plaintext),
    Invalid(InvalidPlaintextReason),
}

// Every object is visited before map conversion, including ignored extension values.
// Duplicate information survives recursively instead of being silently overwritten.
struct RawJson {
    value: Value,
    duplicate: bool,
}
impl<'de> Deserialize<'de> for RawJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct RawVisitor;
        impl<'de> Visitor<'de> for RawVisitor {
            type Value = RawJson;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("JSON value")
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(RawJson {
                    value: Value::Bool(v),
                    duplicate: false,
                })
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(RawJson {
                    value: v.into(),
                    duplicate: false,
                })
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(RawJson {
                    value: v.into(),
                    duplicate: false,
                })
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
                Ok(RawJson {
                    value: serde_json::Number::from_f64(v)
                        .map(Value::Number)
                        .ok_or_else(|| E::custom("nonfinite number"))?,
                    duplicate: false,
                })
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                self.visit_string(v.into())
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(RawJson {
                    value: Value::String(v),
                    duplicate: false,
                })
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(RawJson {
                    value: Value::Null,
                    duplicate: false,
                })
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                self.visit_unit()
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                let mut duplicate = false;
                while let Some(v) = seq.next_element::<RawJson>()? {
                    duplicate |= v.duplicate;
                    values.push(v.value);
                }
                Ok(RawJson {
                    value: Value::Array(values),
                    duplicate,
                })
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut values = Map::new();
                let mut seen = HashSet::new();
                let mut duplicate = false;
                while let Some((key, v)) = map.next_entry::<String, RawJson>()? {
                    duplicate |= !seen.insert(key.clone()) || v.duplicate;
                    values.insert(key, v.value);
                }
                Ok(RawJson {
                    value: Value::Object(values),
                    duplicate,
                })
            }
        }
        deserializer.deserialize_any(RawVisitor)
    }
}
pub fn classify_plaintext(bytes: &[u8]) -> PlaintextClassification {
    use InvalidPlaintextReason as Reason;
    let Ok(raw) = serde_json::from_slice::<RawJson>(bytes) else {
        return PlaintextClassification::Invalid(Reason::NotObject);
    };
    let Some(object) = raw.value.as_object() else {
        return PlaintextClassification::Invalid(Reason::NotObject);
    };
    if raw.duplicate {
        return PlaintextClassification::Invalid(Reason::DuplicateMember);
    }
    const RESERVED: &[&str] = &[
        "from",
        "sender",
        "to",
        "recipient",
        "tenant",
        "namespace",
        "actor",
        "project",
        "device",
        "delegation",
    ];
    if RESERVED.iter().any(|key| object.contains_key(*key)) {
        return PlaintextClassification::Invalid(Reason::ReservedIdentityMember);
    }
    if let Some(kind) = object.get("kind") {
        if !matches!(kind.as_str(), Some("announce" | "report" | "ask")) {
            return PlaintextClassification::Invalid(Reason::InvalidKind);
        }
    }
    match serde_json::from_value::<ParsedPlaintext>(raw.value) {
        Ok(value) => PlaintextClassification::Valid(value.into_plaintext()),
        Err(_) => PlaintextClassification::Invalid(Reason::NotObject),
    }
}

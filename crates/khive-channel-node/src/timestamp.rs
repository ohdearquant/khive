//! The strict JSON timestamp profile shared by plaintext and server fields.
use crate::ProtocolError;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};

fn parse_strict(value: &str, utc_only: bool) -> Result<DateTime<Utc>, ProtocolError> {
    let bytes = value.as_bytes();
    if !(20..=35).contains(&bytes.len())
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || ![0..4, 5..7, 8..10, 11..13, 14..16, 17..19]
            .into_iter()
            .all(|range| bytes[range].iter().all(u8::is_ascii_digit))
        || bytes[17] > b'5'
    {
        return Err(ProtocolError::InvalidEncoding);
    }
    let mut suffix = 19;
    if bytes[suffix] == b'.' {
        suffix += 1;
        let start = suffix;
        while bytes.get(suffix).is_some_and(u8::is_ascii_digit) {
            suffix += 1;
        }
        if !(1..=9).contains(&(suffix - start)) {
            return Err(ProtocolError::InvalidEncoding);
        }
    }
    let offset = &bytes[suffix..];
    if offset != b"Z"
        && (offset.len() != 6
            || !matches!(offset[0], b'+' | b'-')
            || offset[3] != b':'
            || ![offset[1], offset[2], offset[4], offset[5]]
                .iter()
                .all(u8::is_ascii_digit)
            || (offset[1] - b'0') * 10 + offset[2] - b'0' > 23
            || (offset[4] - b'0') * 10 + offset[5] - b'0' > 59
            || offset == b"-00:00"
            || (utc_only && offset != b"+00:00"))
    {
        return Err(ProtocolError::InvalidEncoding);
    }
    DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.with_timezone(&Utc))
        .map_err(|_| ProtocolError::InvalidEncoding)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UtcTimestamp(DateTime<Utc>);
impl UtcTimestamp {
    pub fn parse(value: &str) -> Result<Self, ProtocolError> {
        parse_strict(value, true).map(Self)
    }
    pub fn from_utc(value: DateTime<Utc>) -> Self {
        Self(value)
    }
    pub fn as_utc(&self) -> DateTime<Utc> {
        self.0
    }
}
impl Serialize for UtcTimestamp {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.to_rfc3339_opts(SecondsFormat::AutoSi, true))
    }
}
impl<'de> Deserialize<'de> for UtcTimestamp {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(d)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerTimestamp(DateTime<Utc>);
impl ServerTimestamp {
    pub fn parse(value: &str) -> Result<Self, ProtocolError> {
        parse_strict(value, false).map(Self)
    }
    pub fn from_utc(value: DateTime<Utc>) -> Self {
        Self(value)
    }
    pub fn as_utc(&self) -> DateTime<Utc> {
        self.0
    }
}
impl Serialize for ServerTimestamp {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.to_rfc3339_opts(SecondsFormat::AutoSi, true))
    }
}
impl<'de> Deserialize<'de> for ServerTimestamp {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(d)?).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::ServerTimestamp;

    #[test]
    fn server_timestamp_accepts_offsets_and_normalizes_to_utc() {
        for (input, expected) in [
            ("2026-09-23T20:00:00Z", "2026-09-23T20:00:00Z"),
            ("2026-09-23T20:00:00+00:00", "2026-09-23T20:00:00Z"),
            ("2026-09-23T16:00:00-04:00", "2026-09-23T20:00:00Z"),
            ("2026-09-23T21:30:00+05:30", "2026-09-23T16:00:00Z"),
            ("2026-01-01T00:30:00+01:00", "2025-12-31T23:30:00Z"),
            ("2026-09-23T20:00:59.1Z", "2026-09-23T20:00:59.100Z"),
            (
                "2026-09-23T20:00:59.123456789Z",
                "2026-09-23T20:00:59.123456789Z",
            ),
        ] {
            let parsed = ServerTimestamp::parse(input).expect(input);
            let from_json: ServerTimestamp =
                serde_json::from_value(serde_json::json!(input)).expect(input);
            assert_eq!(parsed, from_json);
            assert_eq!(parsed, ServerTimestamp::parse(expected).unwrap(), "{input}");
            assert_eq!(serde_json::to_value(&parsed).unwrap(), expected, "{input}");
            assert_eq!(ServerTimestamp::from_utc(parsed.as_utc()), parsed);
        }
    }

    #[test]
    fn server_timestamp_refuses_every_non_profile_form() {
        for input in [
            "2026-09-23 20:00:00Z",
            "2026-09-23t20:00:00Z",
            "2026-09-23T20:00:00z",
            "2026-09-23T20:00:00-00:00",
            "2026-09-23T23:59:60Z",
            "2026-09-23T20:00:00.1234567890Z",
            "2026-09-23T20:00:00.Z",
            "2026-09-23T20:00:00+24:00",
            "2026-09-23T20:00:00+05:60",
            "2026-02-30T20:00:00Z",
            "2026-09-23T24:00:00Z",
            "2026-09-23T20:00:00Z ",
        ] {
            assert!(ServerTimestamp::parse(input).is_err(), "{input}");
            assert!(
                serde_json::from_value::<ServerTimestamp>(serde_json::json!(input)).is_err(),
                "{input}"
            );
        }
    }
}

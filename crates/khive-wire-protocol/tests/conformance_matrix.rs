//! Stage-aware ADR-137 vectors exercised by Rust and an independent JSON implementation.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, Stdio};

use khive_wire_protocol::{
    encode_frame, ClientHandshakeGate, ClientHandshakeOutcome, Frame, FrameCodec, HandshakeGate,
    HandshakeOutcome, OperationId, ProtocolVersion, SupportedVersions, TerminalScope,
    WireErrorCode, CURRENT_VERSION, WIRE_ERROR_CODES,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;
use serde_json::Value;

type MatrixResult<T> = Result<T, String>;
const PYTHON_INTERPRETER: &str = "python3";
const EXPECTED_VECTOR_COUNT: usize = 213;

// Unlike Option, this distinguishes an omitted member from an explicit null.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Field<T> {
    Missing,
    Present(T),
}
impl<T> Default for Field<T> {
    fn default() -> Self {
        Self::Missing
    }
}
impl<T> Field<T> {
    fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }
    fn get(&self) -> Option<&T> {
        match self {
            Self::Missing => None,
            Self::Present(value) => Some(value),
        }
    }
    fn required(&self, name: &str) -> MatrixResult<&T> {
        self.get().ok_or_else(|| format!("missing {name}"))
    }
}
impl<'de, T: Deserialize<'de>> Deserialize<'de> for Field<T> {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        T::deserialize(de).map(Self::Present)
    }
}
impl<T: Serialize> Serialize for Field<T> {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Missing => ser.serialize_unit(),
            Self::Present(value) => value.serialize(ser),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Stage {
    Decode,
    Admit,
    Encode,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Accepted,
    Rejected,
    Admitted,
    Emitted,
    Refused,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Scope {
    Connection,
    Request,
}
impl From<TerminalScope> for Scope {
    fn from(value: TerminalScope) -> Self {
        match value {
            TerminalScope::Connection => Self::Connection,
            TerminalScope::Request => Self::Request,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Fields {
    outcome: Outcome,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    consumed_length: Field<u64>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    wire_error_code: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    terminal_scope: Field<Scope>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    opaque_field: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    opaque_input_hex: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    opaque_output_hex: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    effective_code: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    raw_error_code: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    fallback_scope: Field<Scope>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    reencode_outcome: Field<Outcome>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IndependentRow {
    id: String,
    stage: Stage,
    outcome: Outcome,
    #[serde(default)]
    consumed_length: Field<u64>,
    #[serde(default)]
    wire_error_code: Field<String>,
    #[serde(default)]
    terminal_scope: Field<Scope>,
    #[serde(default)]
    opaque_field: Field<String>,
    #[serde(default)]
    opaque_input_hex: Field<String>,
    #[serde(default)]
    opaque_output_hex: Field<String>,
    #[serde(default)]
    effective_code: Field<String>,
    #[serde(default)]
    raw_error_code: Field<String>,
    #[serde(default)]
    fallback_scope: Field<Scope>,
    #[serde(default)]
    reencode_outcome: Field<Outcome>,
}
impl IndependentRow {
    fn into_fields(self) -> Fields {
        Fields {
            outcome: self.outcome,
            consumed_length: self.consumed_length,
            wire_error_code: self.wire_error_code,
            terminal_scope: self.terminal_scope,
            opaque_field: self.opaque_field,
            opaque_input_hex: self.opaque_input_hex,
            opaque_output_hex: self.opaque_output_hex,
            effective_code: self.effective_code,
            raw_error_code: self.raw_error_code,
            fallback_scope: self.fallback_scope,
            reencode_outcome: self.reencode_outcome,
        }
    }
}

impl Fields {
    fn new(outcome: Outcome) -> Self {
        Self {
            outcome,
            consumed_length: Field::Missing,
            wire_error_code: Field::Missing,
            terminal_scope: Field::Missing,
            opaque_field: Field::Missing,
            opaque_input_hex: Field::Missing,
            opaque_output_hex: Field::Missing,
            effective_code: Field::Missing,
            raw_error_code: Field::Missing,
            fallback_scope: Field::Missing,
            reencode_outcome: Field::Missing,
        }
    }
    fn rejected(code: WireErrorCode) -> Self {
        let mut fields = Self::new(Outcome::Rejected);
        fields.wire_error_code = Field::Present(code.as_str().to_owned());
        fields.terminal_scope = Field::Present(code.terminal_scope().into());
        fields
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Vector {
    id: String,
    stage: Stage,
    decision: u8,
    role: Value,
    #[serde(default)]
    description: Field<String>,
    #[serde(default)]
    input_hex: Field<String>,
    #[serde(default)]
    max_payload_bytes: Field<u64>,
    #[serde(default)]
    roundtrip_opaque: Field<bool>,
    #[serde(default)]
    check_unknown_fallback: Field<bool>,
    #[serde(default)]
    starting_state: Field<StartingState>,
    #[serde(default)]
    preceding: Field<Vec<FrameFixture>>,
    #[serde(default)]
    frame: Field<FrameFixture>,
    expected: Fields,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartingState {
    phase: Phase,
    #[serde(default)]
    offered_version: Field<u32>,
    #[serde(default)]
    version: Field<u32>,
    #[serde(default)]
    wire_error_code: Field<String>,
}
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Fresh,
    Outstanding,
    Complete,
    Closed,
}

// These are local fixture fields, not a decoder for received wire bytes.
// In particular, empty IDs, version zero and non-object Value payloads survive
// this conversion so that the public encoder owns their wire validation.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum FrameFixture {
    Handshake {
        version: u32,
    },
    HandshakeAck {
        version: u32,
    },
    Request {
        id: String,
        ops: String,
        #[serde(default)]
        deadline_ms: Field<u64>,
        #[serde(default)]
        namespace: Field<String>,
        #[serde(default)]
        actor_id: Field<String>,
        #[serde(default)]
        visible_namespaces: Field<Vec<String>>,
    },
    Response {
        id: String,
        result: Value,
    },
    Error {
        #[serde(default)]
        id: Field<String>,
        code: String,
        message: String,
    },
    Cancel {
        id: String,
    },
    Subscribe {
        id: String,
        topic: String,
        #[serde(default)]
        resume_cursor: Field<u64>,
    },
    SubscribeAck {
        id: String,
        topic: String,
        start_cursor: u64,
    },
    Unsubscribe {
        id: String,
        topic: String,
    },
    UnsubscribeAck {
        id: String,
        topic: String,
    },
    Event {
        topic: String,
        cursor: u64,
        occurred_at: String,
        payload: Value,
    },
}
impl FrameFixture {
    fn construct(&self) -> MatrixResult<Frame> {
        Ok(match self {
            Self::Handshake { version } => Frame::Handshake {
                version: ProtocolVersion::new(*version),
            },
            Self::HandshakeAck { version } => Frame::HandshakeAck {
                version: ProtocolVersion::new(*version),
            },
            Self::Request {
                id,
                ops,
                deadline_ms,
                namespace,
                actor_id,
                visible_namespaces,
            } => Frame::Request {
                id: OperationId::from(id.clone()),
                ops: ops.clone(),
                deadline_ms: deadline_ms.get().copied(),
                namespace: namespace.get().cloned(),
                actor_id: actor_id.get().cloned(),
                visible_namespaces: visible_namespaces.get().cloned(),
            },
            Self::Response { id, result } => Frame::Response {
                id: id.clone().into(),
                result: result.clone(),
            },
            Self::Error { id, code, message } => Frame::Error {
                id: id.get().cloned().map(OperationId::from),
                code: parse_code(code)?,
                message: message.clone(),
                unrecognized_code: (!WIRE_ERROR_CODES.contains(&code.as_str()))
                    .then(|| code.clone()),
            },
            Self::Cancel { id } => Frame::Cancel {
                id: id.clone().into(),
            },
            Self::Subscribe {
                id,
                topic,
                resume_cursor,
            } => Frame::Subscribe {
                id: id.clone().into(),
                topic: topic.clone(),
                resume_cursor: resume_cursor.get().copied(),
            },
            Self::SubscribeAck {
                id,
                topic,
                start_cursor,
            } => Frame::SubscribeAck {
                id: id.clone().into(),
                topic: topic.clone(),
                start_cursor: *start_cursor,
            },
            Self::Unsubscribe { id, topic } => Frame::Unsubscribe {
                id: id.clone().into(),
                topic: topic.clone(),
            },
            Self::UnsubscribeAck { id, topic } => Frame::UnsubscribeAck {
                id: id.clone().into(),
                topic: topic.clone(),
            },
            Self::Event {
                topic,
                cursor,
                occurred_at,
                payload,
            } => Frame::Event {
                topic: topic.clone(),
                cursor: *cursor,
                occurred_at: occurred_at.clone(),
                payload: payload.clone(),
            },
        })
    }
}
fn parse_code(raw: &str) -> MatrixResult<WireErrorCode> {
    serde_json::from_value(Value::String(raw.to_owned()))
        .map_err(|e| format!("error-code fixture {raw:?}: {e}"))
}
fn flag(field: &Field<bool>, name: &str) -> MatrixResult<bool> {
    match field {
        Field::Missing => Ok(false),
        Field::Present(true) => Ok(true),
        Field::Present(false) => Err(format!("{name} must be omitted or literal true")),
    }
}
fn absent<T>(field: &Field<T>, name: &str) -> MatrixResult<()> {
    if field.is_missing() {
        Ok(())
    } else {
        Err(format!("inapplicable field {name} must be omitted"))
    }
}
fn hex_bytes(hex: &str) -> MatrixResult<Vec<u8>> {
    if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) || !hex.len().is_multiple_of(2) {
        return Err("hex must be even-length ASCII".into());
    }
    hex.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).map_err(|e| e.to_string())?;
            u8::from_str_radix(pair, 16).map_err(|e| format!("invalid hex byte {pair:?}: {e}"))
        })
        .collect()
}
fn hex_string(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut out, "{byte:02x}").expect("writing to String");
    }
    out
}

fn validate_fields(vector: &Vector, fields: &Fields) -> MatrixResult<()> {
    match (vector.stage, fields.outcome) {
        (Stage::Decode, Outcome::Accepted) => {
            fields.consumed_length.required("consumed_length")?;
        }
        (Stage::Decode | Stage::Admit, Outcome::Rejected) => {
            let code = fields.wire_error_code.required("wire_error_code")?;
            if !WIRE_ERROR_CODES.contains(&code.as_str()) {
                return Err(format!("unknown generated wire code {code:?}"));
            }
            fields.terminal_scope.required("terminal_scope")?;
        }
        (Stage::Admit, Outcome::Admitted)
        | (Stage::Encode, Outcome::Emitted | Outcome::Refused) => {}
        _ => {
            return Err(format!(
                "outcome {:?} does not belong to {:?}",
                fields.outcome, vector.stage
            ))
        }
    }
    let accepted_decode = vector.stage == Stage::Decode && fields.outcome == Outcome::Accepted;
    let rejected_wire = vector.stage != Stage::Encode && fields.outcome == Outcome::Rejected;
    if !accepted_decode {
        absent(&fields.consumed_length, "consumed_length")?;
    }
    if !rejected_wire {
        absent(&fields.wire_error_code, "wire_error_code")?;
        absent(&fields.terminal_scope, "terminal_scope")?;
    }
    if flag(&vector.roundtrip_opaque, "roundtrip_opaque")? && accepted_decode {
        let field = fields.opaque_field.required("opaque_field")?;
        if field != "response.result" && field != "event.payload" {
            return Err(format!("unknown opaque field {field:?}"));
        }
        hex_bytes(fields.opaque_input_hex.required("opaque_input_hex")?)?;
        hex_bytes(fields.opaque_output_hex.required("opaque_output_hex")?)?;
    } else {
        absent(&fields.opaque_field, "opaque_field")?;
        absent(&fields.opaque_input_hex, "opaque_input_hex")?;
        absent(&fields.opaque_output_hex, "opaque_output_hex")?;
    }
    if flag(&vector.check_unknown_fallback, "check_unknown_fallback")? && accepted_decode {
        fields.effective_code.required("effective_code")?;
        fields.raw_error_code.required("raw_error_code")?;
        fields.fallback_scope.required("fallback_scope")?;
        let outcome = fields.reencode_outcome.required("reencode_outcome")?;
        if !matches!(outcome, Outcome::Emitted | Outcome::Refused) {
            return Err("invalid reencode_outcome".into());
        }
    } else {
        absent(&fields.effective_code, "effective_code")?;
        absent(&fields.raw_error_code, "raw_error_code")?;
        absent(&fields.fallback_scope, "fallback_scope")?;
        absent(&fields.reencode_outcome, "reencode_outcome")?;
    }
    Ok(())
}
fn canonical_fields(fields: &Fields) -> MatrixResult<Fields> {
    let mut fields = fields.clone();
    for field in [&mut fields.opaque_input_hex, &mut fields.opaque_output_hex] {
        if let Field::Present(hex) = field {
            *hex = hex_string(&hex_bytes(hex)?);
        }
    }
    Ok(fields)
}
fn validate_vector(vector: &Vector) -> MatrixResult<()> {
    if vector.id.is_empty() {
        return Err("vector id must not be empty".into());
    }
    if !(1..=10).contains(&vector.decision) {
        return Err("decision must be integer1..10".into());
    }
    let _ = vector.description.get();
    match vector.stage {
        Stage::Decode => {
            if vector.role != "server" && vector.role != "client" {
                return Err("decode role must be server/client".into());
            }
            hex_bytes(vector.input_hex.required("input_hex")?)?;
            if let Some(limit) = vector.max_payload_bytes.get() {
                usize::try_from(*limit)
                    .map_err(|_| "max_payload_bytes exceeds usize".to_string())?;
            }
            absent(&vector.frame, "frame")?;
            absent(&vector.starting_state, "starting_state")?;
            absent(&vector.preceding, "preceding")?;
        }
        Stage::Admit => {
            if vector.role != "server" && vector.role != "client" {
                return Err("admit role must be server/client".into());
            }
            vector.frame.required("frame")?;
            vector.starting_state.required("starting_state")?;
            absent(&vector.input_hex, "input_hex")?;
            absent(&vector.max_payload_bytes, "max_payload_bytes")?;
            absent(&vector.roundtrip_opaque, "roundtrip_opaque")?;
            absent(&vector.check_unknown_fallback, "check_unknown_fallback")?;
        }
        Stage::Encode => {
            if !vector.role.is_null() {
                return Err("encode role must be null".into());
            }
            vector.frame.required("frame")?;
            absent(&vector.input_hex, "input_hex")?;
            absent(&vector.max_payload_bytes, "max_payload_bytes")?;
            absent(&vector.roundtrip_opaque, "roundtrip_opaque")?;
            absent(&vector.check_unknown_fallback, "check_unknown_fallback")?;
            absent(&vector.starting_state, "starting_state")?;
            absent(&vector.preceding, "preceding")?;
        }
    }
    if (flag(&vector.roundtrip_opaque, "roundtrip_opaque")?
        || flag(&vector.check_unknown_fallback, "check_unknown_fallback")?)
        && !(vector.stage == Stage::Decode && vector.expected.outcome == Outcome::Accepted)
    {
        return Err("designated decode observations require an accepted decode expectation".into());
    }
    validate_fields(vector, &vector.expected)
}

#[derive(Deserialize)]
struct RawResponse<'a> {
    #[serde(borrow)]
    result: &'a RawValue,
}
#[derive(Deserialize)]
struct RawEvent<'a> {
    #[serde(borrow)]
    payload: &'a RawValue,
}
fn opaque_span(wire: &[u8], field: &str) -> MatrixResult<String> {
    let payload = wire
        .get(khive_wire_protocol::codec::LENGTH_PREFIX_BYTES..)
        .ok_or_else(|| "opaque observation has no length prefix".to_string())?;
    let raw = match field {
        "response.result" => serde_json::from_slice::<RawResponse<'_>>(payload)
            .map(|value| value.result)
            .map_err(|e| e.to_string())?,
        "event.payload" => serde_json::from_slice::<RawEvent<'_>>(payload)
            .map(|value| value.payload)
            .map_err(|e| e.to_string())?,
        _ => return Err(format!("unknown opaque measurement {field:?}")),
    };
    Ok(hex_string(raw.get().as_bytes()))
}
fn observe_decode(vector: &Vector) -> MatrixResult<Fields> {
    let wire = hex_bytes(vector.input_hex.required("input_hex")?)?;
    let limit = vector
        .max_payload_bytes
        .get()
        .copied()
        .map(usize::try_from)
        .transpose()
        .map_err(|_| "max_payload_bytes exceeds usize".to_string())?
        .unwrap_or(khive_wire_protocol::DEFAULT_MAX_FRAME_BYTES);
    match FrameCodec::new(limit).decode_with_consumed(&wire) {
        Err(error) => Ok(Fields::rejected(error.wire_code())),
        Ok((frame, consumed)) => {
            let mut fields = Fields::new(Outcome::Accepted);
            fields.consumed_length =
                Field::Present(u64::try_from(consumed).map_err(|e| e.to_string())?);
            if flag(&vector.roundtrip_opaque, "roundtrip_opaque")? {
                let field = match &frame {
                    Frame::Response { .. } => "response.result",
                    Frame::Event { .. } => "event.payload",
                    _ => return Err("opaque roundtrip decoded neither response nor event".into()),
                };
                let output = encode_frame(&frame)
                    .map_err(|e| format!("public opaque re-encode failed: {e}"))?;
                fields.opaque_field = Field::Present(field.into());
                fields.opaque_input_hex = Field::Present(opaque_span(&wire[..consumed], field)?);
                fields.opaque_output_hex = Field::Present(opaque_span(&output, field)?);
            }
            if flag(&vector.check_unknown_fallback, "check_unknown_fallback")? {
                match &frame {
                    Frame::Error {
                        code,
                        unrecognized_code: Some(raw),
                        ..
                    } => {
                        fields.effective_code = Field::Present(code.as_str().into());
                        fields.raw_error_code = Field::Present(raw.clone());
                        fields.fallback_scope = Field::Present(code.terminal_scope().into());
                        fields.reencode_outcome = Field::Present(if encode_frame(&frame).is_ok() {
                            Outcome::Emitted
                        } else {
                            Outcome::Refused
                        });
                    }
                    _ => {
                        return Err(
                            "requested fallback observation was not a decoded unknown-code error"
                                .into(),
                        )
                    }
                }
            }
            Ok(fields)
        }
    }
}

enum Gate {
    Server(HandshakeGate),
    Client(ClientHandshakeGate),
}
impl Gate {
    fn admit(&mut self, frame: &Frame) -> MatrixResult<Fields> {
        match self {
            Self::Server(gate) => match gate.admit(frame) {
                Ok(HandshakeOutcome::Accepted { .. } | HandshakeOutcome::Admitted) => {
                    Ok(Fields::new(Outcome::Admitted))
                }
                Ok(HandshakeOutcome::Rejected { error }) => error_fields(&error),
                Err(error) => error_fields(&error.error),
            },
            Self::Client(gate) => match gate.admit(frame) {
                Ok(ClientHandshakeOutcome::Completed { version }) => {
                    if gate.accepted_version() != Some(version) {
                        return Err("client completion did not retain accepted version".into());
                    }
                    Ok(Fields::new(Outcome::Admitted))
                }
                Ok(ClientHandshakeOutcome::Admitted) => Ok(Fields::new(Outcome::Admitted)),
                Ok(ClientHandshakeOutcome::ServerTerminated) => {
                    if !gate.is_closed() {
                        return Err(
                            "admitted terminal server error did not close client gate".into()
                        );
                    }
                    Ok(Fields::new(Outcome::Admitted))
                }
                Err(error) => error_fields(&error.error),
            },
        }
    }
}
fn error_fields(frame: &Frame) -> MatrixResult<Fields> {
    match frame {
        Frame::Error { code, .. } => Ok(Fields::rejected(*code)),
        other => Err(format!(
            "gate rejection returned {}, not an error",
            other.kind()
        )),
    }
}
fn starting_gate(vector: &Vector) -> MatrixResult<Gate> {
    let state = vector.starting_state.required("starting_state")?;
    match state.phase {
        Phase::Fresh => {
            absent(&state.offered_version, "offered_version")?;
            absent(&state.version, "version")?;
            absent(&state.wire_error_code, "wire_error_code")?;
        }
        Phase::Outstanding => {
            state.offered_version.required("offered_version")?;
            absent(&state.version, "version")?;
            absent(&state.wire_error_code, "wire_error_code")?;
        }
        Phase::Complete => {
            state.version.required("version")?;
            absent(&state.offered_version, "offered_version")?;
            absent(&state.wire_error_code, "wire_error_code")?;
        }
        Phase::Closed => {
            state.wire_error_code.required("wire_error_code")?;
            absent(&state.offered_version, "offered_version")?;
            absent(&state.version, "version")?;
        }
    }
    if vector.role == "server" {
        let mut gate = HandshakeGate::new(SupportedVersions::current());
        match state.phase {
            Phase::Fresh => {}
            Phase::Complete => {
                let version = ProtocolVersion::new(*state.version.required("version")?);
                match gate.admit(&Frame::Handshake { version }) {
                    Ok(HandshakeOutcome::Accepted {
                        version: actual, ..
                    }) if actual == version => {}
                    other => {
                        return Err(format!(
                            "cannot establish server completed state: {other:?}"
                        ))
                    }
                }
            }
            Phase::Closed => {
                let code = state.wire_error_code.required("wire_error_code")?;
                let frame = match code.as_str() {
                    "unsupported_version" => Frame::Handshake {
                        version: ProtocolVersion::new(0),
                    },
                    "malformed_frame" => Frame::Cancel { id: "setup".into() },
                    _ => return Err(format!("server API cannot establish closed cause {code:?}")),
                };
                let observed = match gate.admit(&frame) {
                    Ok(HandshakeOutcome::Rejected { error }) => error_fields(&error)?,
                    Err(error) => error_fields(&error.error)?,
                    other => return Err(format!("server terminal setup admitted: {other:?}")),
                };
                if observed.outcome != Outcome::Rejected
                    || observed.wire_error_code.get() != Some(code)
                {
                    return Err("server closed-state cause differs from fixture".into());
                }
            }
            Phase::Outstanding => return Err("server gate has no outstanding state".into()),
        }
        Ok(Gate::Server(gate))
    } else if vector.role == "client" {
        let mut gate = ClientHandshakeGate::new();
        match state.phase {
            Phase::Fresh => {}
            Phase::Outstanding => gate
                .record_handshake_sent(ProtocolVersion::new(
                    *state.offered_version.required("offered_version")?,
                ))
                .map_err(|e| format!("cannot establish client outstanding state: {e}"))?,
            Phase::Complete => {
                let version = ProtocolVersion::new(*state.version.required("version")?);
                gate.record_handshake_sent(version)
                    .map_err(|e| e.to_string())?;
                match gate.admit(&Frame::HandshakeAck { version }) {
                    Ok(ClientHandshakeOutcome::Completed { version: actual })
                        if actual == version => {}
                    other => {
                        return Err(format!(
                            "cannot establish client completed state: {other:?}"
                        ))
                    }
                }
            }
            Phase::Closed => {
                let raw = state.wire_error_code.required("wire_error_code")?;
                if !WIRE_ERROR_CODES.contains(&raw.as_str()) {
                    return Err("client closed cause must be a known code".into());
                }
                let code = parse_code(raw)?;
                if code.terminal_scope() != TerminalScope::Connection {
                    return Err("client closed cause is not connection-terminal".into());
                }
                gate.record_handshake_sent(CURRENT_VERSION)
                    .map_err(|e| e.to_string())?;
                let frame = Frame::Error {
                    id: None,
                    code,
                    message: "setup".into(),
                    unrecognized_code: None,
                };
                if gate.admit(&frame).map_err(|e| e.to_string())?
                    != ClientHandshakeOutcome::ServerTerminated
                    || !gate.is_closed()
                {
                    return Err("cannot establish terminal client state".into());
                }
            }
        }
        Ok(Gate::Client(gate))
    } else {
        Err("admit role must be server/client".into())
    }
}
fn observe(vector: &Vector) -> MatrixResult<Fields> {
    match vector.stage {
        Stage::Decode => observe_decode(vector),
        Stage::Encode => {
            let frame = vector.frame.required("frame")?.construct()?;
            Ok(Fields::new(if encode_frame(&frame).is_ok() {
                Outcome::Emitted
            } else {
                Outcome::Refused
            }))
        }
        Stage::Admit => {
            let mut gate = starting_gate(vector)?;
            if let Some(preceding) = vector.preceding.get() {
                for fixture in preceding {
                    gate.admit(&fixture.construct()?)?;
                }
            }
            gate.admit(&vector.frame.required("frame")?.construct()?)
        }
    }
}

fn independent_rows(
    interpreter: &str,
    oracle: &Path,
    vector_path: &Path,
    vectors: &[Vector],
) -> MatrixResult<BTreeMap<String, Fields>> {
    let output = Command::new(interpreter)
        .arg("-I")
        .arg(oracle)
        .arg(vector_path)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            format!("failed to spawn independent interpreter {interpreter:?}: {error}")
        })?;
    if !output.status.success() {
        return Err(format!(
            "independent interpreter {interpreter:?} exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let stdout = std::str::from_utf8(&output.stdout)
        .map_err(|e| format!("independent stdout is not UTF-8: {e}"))?;
    let known: BTreeMap<&str, &Vector> = vectors.iter().map(|v| (v.id.as_str(), v)).collect();
    let mut rows = BTreeMap::new();
    for (index, line) in stdout.lines().enumerate() {
        if line.trim().is_empty() {
            return Err(format!("empty independent result row at line{}", index + 1));
        }
        let row: IndependentRow = serde_json::from_str(line)
            .map_err(|e| format!("invalid independent result row at line{}: {e}", index + 1))?;
        let vector = known
            .get(row.id.as_str())
            .ok_or_else(|| format!("unknown independent result id {:?}", row.id))?;
        if row.stage != vector.stage {
            return Err(format!("independent stage differs for {}", row.id));
        }
        let id = row.id.clone();
        let fields = row.into_fields();
        validate_fields(vector, &fields).map_err(|e| format!("independent row {id}: {e}"))?;
        if rows
            .insert(id.clone(), canonical_fields(&fields)?)
            .is_some()
        {
            return Err(format!("duplicate independent result id {id}"));
        }
    }
    let missing: Vec<&str> = vectors
        .iter()
        .filter(|v| !rows.contains_key(&v.id))
        .map(|v| v.id.as_str())
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "missing independent result ids: {}",
            missing.join(", ")
        ));
    }
    if rows.is_empty() || rows.len() != vectors.len() {
        return Err(format!(
            "independent result count{} differs from vector count{}",
            rows.len(),
            vectors.len()
        ));
    }
    Ok(rows)
}

fn known_gap_note(id: &str) -> Option<&'static str> {
    if id.starts_with("d04.decode.duplicate.") {
        Some("Decision4 requires duplicate rejection before field validation")
    } else if id.starts_with("d04.") && id.ends_with(".null") {
        Some("Decision4 requires omission, not explicit-null optional fields")
    } else if id == "d05.decode.unknown.without_id" {
        Some("Decision5 rejects id-less unknown error codes")
    } else if id.starts_with("d09.decode.") {
        Some("Decision9 requires byte-exact opaque subvalue preservation")
    } else if id.starts_with("d10.") {
        Some("Decision10 requires object event payloads at decode and encode")
    } else if id.ends_with("request.deadline_ms.zero") {
        Some("ADR137 requires positive deadline_ms")
    } else {
        None
    }
}

#[test]
fn conformance_matrix_agrees_with_independent_json() {
    if let Err(error) = run_matrix() {
        panic!("{error}");
    }
}
fn run_matrix() -> MatrixResult<()> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let vector_path = manifest_dir.join("tests/conformance/vectors.json");
    let oracle_path = manifest_dir.join("tests/conformance/oracle.py");
    let input =
        std::fs::read(&vector_path).map_err(|e| format!("read {}: {e}", vector_path.display()))?;
    let vectors: Vec<Vector> =
        serde_json::from_slice(&input).map_err(|e| format!("invalid vector file: {e}"))?;
    if vectors.len() != EXPECTED_VECTOR_COUNT {
        return Err(format!(
            "vector count{} differs from frozen matrix count{EXPECTED_VECTOR_COUNT}",
            vectors.len()
        ));
    }
    let mut ids = BTreeSet::new();
    let mut decisions = BTreeSet::new();
    let mut counts = [0usize; 3];
    for vector in &vectors {
        validate_vector(vector).map_err(|e| format!("vector {}: {e}", vector.id))?;
        if !ids.insert(vector.id.as_str()) {
            return Err(format!("duplicate vector id {}", vector.id));
        }
        decisions.insert(vector.decision);
        counts[match vector.stage {
            Stage::Decode => 0,
            Stage::Admit => 1,
            Stage::Encode => 2,
        }] += 1;
    }
    if counts != [164, 20, 29] || decisions != (1u8..=10).collect() {
        return Err(format!(
            "matrix coverage changed: stages{counts:?}, decisions{decisions:?}"
        ));
    }
    // Neither an expected mismatch nor a known crate gap suppresses the second run.
    let rust: Vec<MatrixResult<Fields>> = vectors.iter().map(observe).collect();
    let independent = independent_rows(PYTHON_INTERPRETER, &oracle_path, &vector_path, &vectors)?;
    let mut failures = Vec::new();
    for (vector, rust) in vectors.iter().zip(rust) {
        let other = independent
            .get(&vector.id)
            .ok_or_else(|| format!("missing independent result id {}", vector.id))?;
        let mut problems = Vec::new();
        let rust_log = match rust {
            Ok(fields) => {
                if let Err(error) = validate_fields(vector, &fields) {
                    problems.push(format!("Rust result shape: {error}"));
                }
                let fields = canonical_fields(&fields)?;
                let expected = canonical_fields(&vector.expected)?;
                // Agreement is checked separately from either implementation's expectation.
                if &fields != other {
                    problems.push(format!("agreement: Rust {fields:?}; independent {other:?}"));
                }
                if fields != expected {
                    problems.push(format!("Rust expected {expected:?}, observed {fields:?}"));
                }
                if other != &expected {
                    problems.push(format!(
                        "independent expected {expected:?}, observed {other:?}"
                    ));
                }
                serde_json::to_value(&fields).map_err(|e| e.to_string())?
            }
            Err(error) => {
                problems.push(format!("Rust adapter/API observation failed: {error}"));
                let expected = canonical_fields(&vector.expected)?;
                if other != &expected {
                    problems.push(format!(
                        "independent expected {expected:?}, observed {other:?}"
                    ));
                }
                serde_json::json!({"observation_error": error})
            }
        };
        let gap = if problems.is_empty() {
            None
        } else {
            known_gap_note(&vector.id)
        };
        println!(
            "{}",
            serde_json::json!({
                "id": vector.id, "stage": vector.stage, "rust": rust_log,
                "independent": other, "known_crate_gap": gap,
            })
        );
        if !problems.is_empty() {
            failures.push(format!(
                "{} ({:?}): {}",
                vector.id,
                vector.stage,
                problems.join("; ")
            ));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "conformance disagreement in{} vectors:\n{}",
            failures.len(),
            failures.join("\n")
        ))
    }
}

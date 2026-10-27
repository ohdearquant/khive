#!/usr/bin/env python3
"""Independent ADR-137 oracle: ADR plus authorized documentation comments only.

Precedence: Amendment 1 overrides displaced documentation. No crate source,
implementation, runner, tests, or golden fixtures were read or copied.
This program reads a vector file and prints actual observations, never deriving
classification from a vector's expected outcome. Python standard library only.
"""
from __future__ import annotations

import json
import math
from pathlib import Path
import re
import struct
import sys
from dataclasses import dataclass
from typing import Any

DEFAULT_MAX = 8 * 1024 * 1024
U32_MAX = (1 << 32) - 1
CONNECTION_CODES = frozenset((
    "unsupported_version", "identity_rejected", "malformed_frame",
    "frame_too_large", "subscriber_overflow", "subscription_revoked",
))
REQUEST_CODES = frozenset((
    "context_rejected", "peer_class_denied", "subscription_denied",
    "already_subscribed", "cursor_expired", "in_flight_limit_exceeded",
    "deadline_exceeded", "cancelled", "shutting_down", "internal",
))
CODES = CONNECTION_CODES | REQUEST_CODES
CLIENT_KINDS = frozenset(("handshake", "request", "cancel", "subscribe", "unsubscribe"))
SERVER_KINDS = frozenset((
    "handshake_ack", "response", "error", "subscribe_ack", "unsubscribe_ack", "event",
))
# Field names/types are from the authorized lib documentation table, not Rust.
FIELDS = {
    "handshake": ({"version": "u32"}, {}),
    "handshake_ack": ({"version": "u32"}, {}),
    "request": ({"id": "id", "ops": "string"}, {
        "deadline_ms": "positive_u64", "namespace": "string", "actor_id": "string",
        "visible_namespaces": "strings",
    }),
    "response": ({"id": "id", "result": "opaque"}, {}),
    "error": ({"code": "string", "message": "string"}, {"id": "id"}),
    "cancel": ({"id": "id"}, {}),
    "subscribe": ({"id": "id", "topic": "string"}, {"resume_cursor": "u64"}),
    "subscribe_ack": ({"id": "id", "topic": "string", "start_cursor": "u64"}, {}),
    "unsubscribe": ({"id": "id", "topic": "string"}, {}),
    "unsubscribe_ack": ({"id": "id", "topic": "string"}, {}),
    "event": ({
        "topic": "string", "cursor": "u64", "occurred_at": "string",
        "payload": "opaque_object",
    }, {}),
}


class SchemaError(Exception):
    """A malformed vector file is not a wire rejection."""


class JsonSyntaxError(Exception):
    pass


class WireFailure(Exception):
    def __init__(self, code: str = "malformed_frame") -> None:
        super().__init__(code)
        self.code = code


class LocalRefusal(Exception):
    pass


@dataclass(frozen=True)
class Number:
    raw: str
    integer: bool


@dataclass(frozen=True)
class Pairs:
    members: list[tuple[str, Any]]


@dataclass
class Frame:
    fields: dict[str, Any]
    opaque: dict[str, bytes]
    raw_error_code: str | None = None


def _constant(_: str) -> Any:
    raise JsonSyntaxError("non-JSON numeric constant")


WIRE_JSON = json.JSONDecoder(
    object_pairs_hook=Pairs,
    parse_int=lambda raw: Number(raw, True),
    parse_float=lambda raw: Number(raw, False),
    parse_constant=_constant,
)


def _skip(text: str, index: int) -> int:
    while index < len(text) and text[index] in " \t\r\n":
        index += 1
    return index


def _wire_object(payload: bytes) -> tuple[list[tuple[str, Any]], dict[str, bytes]]:
    """Parse root members once, retaining complete opaque value token spans."""
    text = payload.decode("utf-8", errors="strict")
    index = _skip(text, 0)
    if index >= len(text) or text[index] != "{":
        raise JsonSyntaxError("frame payload is not an object")
    index = _skip(text, index + 1)
    members: list[tuple[str, Any]] = []
    opaque: dict[str, bytes] = {}
    if index < len(text) and text[index] == "}":
        index += 1
    else:
        while True:
            key, index = WIRE_JSON.raw_decode(text, index)
            if not isinstance(key, str):
                raise JsonSyntaxError("object key is not a string")
            index = _skip(text, index)
            if index >= len(text) or text[index] != ":":
                raise JsonSyntaxError("missing colon")
            start = _skip(text, index + 1)
            value, end = WIRE_JSON.raw_decode(text, start)
            members.append((key, value))
            if key in ("result", "payload"):
                # Disjoint value slices: total UTF-8 encoding work is linear.
                opaque[key] = text[start:end].encode("utf-8")
            index = _skip(text, end)
            if index < len(text) and text[index] == "}":
                index += 1
                break
            if index >= len(text) or text[index] != ",":
                raise JsonSyntaxError("missing comma or object end")
            index = _skip(text, index + 1)
    if _skip(text, index) != len(text):
        raise JsonSyntaxError("trailing JSON data")
    return members, opaque


def _unique_envelope(members: list[tuple[str, Any]]) -> dict[str, Any]:
    seen: set[str] = set()
    for key, _ in members:
        if key in seen:
            raise WireFailure("malformed_frame")
        seen.add(key)
    return dict(members)


def _uint(value: Any, bits: int, positive: bool = False) -> int:
    if isinstance(value, Number) and value.integer:
        # Bound before int conversion; opaque huge integers are never converted.
        if len(value.raw.lstrip("-")) > len(str((1 << bits) - 1)):
            raise WireFailure()
        number = int(value.raw)
    elif type(value) is int:
        number = value
    else:
        # bool is an int subclass, but is not a JSON integer.
        raise WireFailure()
    if number < (1 if positive else 0) or number > (1 << bits) - 1:
        raise WireFailure()
    return number


def _validate(fields: dict[str, Any], opaque: dict[str, bytes], received: bool) -> Frame:
    fields = dict(fields)
    kind = fields.get("kind")
    if not isinstance(kind, str) or kind not in FIELDS:
        raise WireFailure()
    required, optional = FIELDS[kind]
    if not set(required) <= fields.keys():
        raise WireFailure()
    if fields.keys() - (required.keys() | optional.keys() | {"kind"}):
        raise WireFailure()
    for key, typ in (required | optional).items():
        if key not in fields:
            continue
        value = fields[key]
        if key in optional and value is None:
            raise WireFailure()
        if typ in ("string", "id"):
            if not isinstance(value, str) or (typ == "id" and not value):
                raise WireFailure()
        elif typ == "strings":
            if not isinstance(value, list) or not all(isinstance(x, str) for x in value):
                raise WireFailure()
        elif typ in ("u32", "u64", "positive_u64"):
            fields[key] = _uint(value, 32 if typ == "u32" else 64, typ == "positive_u64")
        elif typ == "opaque_object" and not isinstance(value, (Pairs, dict)):
            raise WireFailure()
    if not received and kind in ("handshake", "handshake_ack") and fields["version"] == 0:
        raise WireFailure()
    raw_code = None
    if kind == "error":
        code = fields["code"]
        if code not in CODES:
            if not received or "id" not in fields:
                raise WireFailure()
            raw_code = code
            fields["code"] = "internal"
        elif (code in CONNECTION_CODES) == ("id" in fields):
            raise WireFailure()
    return Frame(fields, opaque, raw_code)


def decode(buf: bytes, maximum: int) -> tuple[Frame, int]:
    if len(buf) < 4:
        raise WireFailure()
    length = struct.unpack(">I", buf[:4])[0]
    # Declared ceiling precedes body buffering; oversized prefix alone suffices.
    if length > maximum:
        raise WireFailure("frame_too_large")
    if len(buf) < length + 4:
        raise WireFailure()
    try:
        members, opaque = _wire_object(buf[4:4 + length])
    except (UnicodeDecodeError, json.JSONDecodeError, JsonSyntaxError) as error:
        raise WireFailure() from error
    fields = _unique_envelope(members)  # Before any field validation.
    return _validate(fields, opaque, received=True), length + 4


def _json_bytes(value: Any) -> bytes:
    if isinstance(value, Number):
        return value.raw.encode("ascii")
    if isinstance(value, Pairs):
        return b"{" + b",".join(
            _json_bytes(key) + b":" + _json_bytes(child)
            for key, child in value.members
        ) + b"}"
    if isinstance(value, dict):
        if not all(isinstance(key, str) for key in value):
            raise LocalRefusal()
        return b"{" + b",".join(
            _json_bytes(key) + b":" + _json_bytes(child)
            for key, child in value.items()
        ) + b"}"
    if isinstance(value, list):
        return b"[" + b",".join(_json_bytes(child) for child in value) + b"]"
    if isinstance(value, float) and not math.isfinite(value):
        raise LocalRefusal()
    try:
        return json.dumps(
            value, ensure_ascii=True, separators=(",", ":"), allow_nan=False,
        ).encode("ascii")
    except (TypeError, ValueError, UnicodeEncodeError) as error:
        raise LocalRefusal() from error


def encode(frame: Frame, maximum: int = DEFAULT_MAX) -> bytes:
    if frame.raw_error_code is not None:
        raise LocalRefusal()
    try:
        checked = _validate(frame.fields, frame.opaque, received=False)
    except WireFailure as error:
        raise LocalRefusal() from error
    kind = checked.fields["kind"]
    opaque_key = "result" if kind == "response" else "payload" if kind == "event" else None
    chunks = []
    for key, value in checked.fields.items():
        data = frame.opaque[key] if key == opaque_key and key in frame.opaque else _json_bytes(value)
        chunks.append(_json_bytes(key) + b":" + data)
    payload = b"{" + b",".join(chunks) + b"}"
    if len(payload) > maximum or len(payload) > U32_MAX:
        raise LocalRefusal()
    return struct.pack(">I", len(payload)) + payload


def _state(role: str, state: dict[str, Any]) -> dict[str, Any]:
    if not isinstance(state, dict):
        raise SchemaError("starting_state must be an object")
    phase = state.get("phase")
    allowed = {"phase"}
    if phase == "outstanding" and role == "client":
        allowed.add("offered_version")
        if type(state.get("offered_version")) is not int or state["offered_version"] != 1:
            raise SchemaError("outstanding client needs current offered_version1")
    elif phase == "complete":
        allowed.add("version")
        if type(state.get("version")) is not int or state["version"] != 1:
            raise SchemaError("complete state needs supported version1")
    elif phase == "closed":
        allowed.add("wire_error_code")
        if state.get("wire_error_code") not in CONNECTION_CODES:
            raise SchemaError("closed state needs connection-terminal cause")
    elif phase != "fresh":
        raise SchemaError("invalid starting phase for role")
    if set(state) != allowed:
        raise SchemaError("incorrect starting_state fields")
    return dict(state)


class Gate:
    """ADR state model, not an adapter or copy of a Rust gate."""
    def __init__(self, role: str, state: dict[str, Any]) -> None:
        self.role = role
        self.state = _state(role, state)

    def fail(self, code: str = "malformed_frame") -> None:
        self.state = {"phase": "closed", "wire_error_code": code}
        raise WireFailure(code)

    def admit(self, frame: Frame) -> None:
        kind = frame.fields["kind"]
        phase = self.state["phase"]
        if phase == "closed":
            raise WireFailure(self.state["wire_error_code"])
        if self.role == "server":
            if phase == "fresh":
                if kind != "handshake":
                    self.fail()
                if frame.fields["version"] != 1:
                    self.fail("unsupported_version")
                self.state = {"phase": "complete", "version": 1}
                return
            if phase != "complete" or kind not in CLIENT_KINDS or kind == "handshake":
                self.fail()
            return
        if phase == "fresh":
            self.fail()
        if phase == "outstanding":
            if kind == "handshake_ack":
                if frame.fields["version"] != self.state["offered_version"]:
                    self.fail()
                self.state = {"phase": "complete", "version": frame.fields["version"]}
                return
            if kind == "error" and frame.fields["code"] in CONNECTION_CODES:
                self.state = {"phase": "closed", "wire_error_code": frame.fields["code"]}
                return  # Admitted terminal server answer, not a local rejection.
            self.fail()
        if kind not in SERVER_KINDS or kind == "handshake_ack":
            self.fail()
        if kind == "error" and frame.fields["code"] in CONNECTION_CODES:
            self.state = {"phase": "closed", "wire_error_code": frame.fields["code"]}


def _metadata_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise SchemaError("duplicate vector metadata member")
        result[key] = value
    return result


def _metadata_constant(_: str) -> Any:
    raise SchemaError("non-JSON vector numeric constant")


def _hex(value: Any) -> bytes:
    if not isinstance(value, str) or len(value) % 2 or re.fullmatch(r"[0-9a-fA-F]*", value) is None:
        raise SchemaError("hex must be even-length ASCII hex")
    return bytes.fromhex(value)


def validate_vector(vector: Any) -> None:
    if not isinstance(vector, dict):
        raise SchemaError("vector must be an object")
    common = {"id", "stage", "decision", "role", "expected"}
    stage = vector.get("stage")
    if stage == "decode":
        required = common | {"input_hex"}
        allowed = required | {"description", "max_payload_bytes", "roundtrip_opaque", "check_unknown_fallback"}
        _hex(vector.get("input_hex"))
        maximum = vector.get("max_payload_bytes", DEFAULT_MAX)
        if type(maximum) is not int or maximum < 0:
            raise SchemaError("max_payload_bytes must be a nonnegative integer")
        for flag in ("roundtrip_opaque", "check_unknown_fallback"):
            if flag in vector and vector[flag] is not True:
                raise SchemaError("optional observation flags must be true or omitted")
    elif stage == "admit":
        required = common | {"starting_state", "frame"}
        allowed = required | {"description", "preceding"}
        if not isinstance(vector.get("frame"), dict):
            raise SchemaError("admit frame must be an object")
        if not isinstance(vector.get("preceding", []), list) or not all(isinstance(x, dict) for x in vector.get("preceding", [])):
            raise SchemaError("preceding must contain frame objects")
    elif stage == "encode":
        required = common | {"frame"}
        allowed = required | {"description"}
        if not isinstance(vector.get("frame"), dict):
            raise SchemaError("encode frame must be an object")
    else:
        raise SchemaError("unknown stage")
    if not required <= vector.keys() or vector.keys() - allowed:
        raise SchemaError("missing or unknown vector fields")
    if not isinstance(vector["id"], str) or not vector["id"]:
        raise SchemaError("id must be a nonempty string")
    if type(vector["decision"]) is not int or not 1 <= vector["decision"] <= 10:
        raise SchemaError("decision must be integer1..10")
    if (stage != "encode" and vector["role"] not in ("server", "client")) or (stage == "encode" and vector["role"] is not None):
        raise SchemaError("incorrect endpoint role")
    if "description" in vector and not isinstance(vector["description"], str):
        raise SchemaError("description must be a string")
    if stage == "admit":
        _state(vector["role"], vector["starting_state"])
    expected = vector["expected"]
    if not isinstance(expected, dict):
        raise SchemaError("expected must be an object")
    outcome = expected.get("outcome")
    outcomes = {"decode": {"accepted", "rejected"}, "admit": {"admitted", "rejected"}, "encode": {"emitted", "refused"}}
    if outcome not in outcomes[stage]:
        raise SchemaError("wrong expected stage vocabulary")
    keys = {"outcome"}
    if stage == "decode" and outcome == "accepted":
        keys.add("consumed_length")
        if type(expected.get("consumed_length")) is not int or expected["consumed_length"] < 4:
            raise SchemaError("accepted decode needs consumed_length")
        if vector.get("roundtrip_opaque"):
            if vector["decision"] != 9:
                raise SchemaError("opaque observation belongs to Decision9")
            keys |= {"opaque_field", "opaque_input_hex", "opaque_output_hex"}
            if expected.get("opaque_field") not in ("response.result", "event.payload"):
                raise SchemaError("unknown opaque observation field")
            _hex(expected.get("opaque_input_hex"))
            _hex(expected.get("opaque_output_hex"))
        if vector.get("check_unknown_fallback"):
            keys |= {"effective_code", "raw_error_code", "fallback_scope", "reencode_outcome"}
            if not all(isinstance(expected.get(k), str) for k in keys - {"outcome", "consumed_length"}):
                raise SchemaError("fallback observations must be strings")
    elif outcome == "rejected":
        keys |= {"wire_error_code", "terminal_scope"}
        if expected.get("wire_error_code") not in CODES or expected.get("terminal_scope") != "connection":
            raise SchemaError("invalid expected rejection")
    if set(expected) != keys:
        raise SchemaError("inapplicable, missing or unknown expected fields")
    if (vector.get("roundtrip_opaque") or vector.get("check_unknown_fallback")) and not (stage == "decode" and outcome == "accepted"):
        raise SchemaError("observations require accepted decode")


def _admit_frame(value: dict[str, Any]) -> Frame:
    try:
        return _validate(value, {}, received=True)
    except WireFailure as error:
        raise SchemaError("admit input is not an already-decoded frame") from error


def observe(vector: dict[str, Any]) -> dict[str, Any]:
    row = {"id": vector["id"], "stage": vector["stage"]}
    stage = vector["stage"]
    if stage == "decode":
        try:
            frame, consumed = decode(_hex(vector["input_hex"]), vector.get("max_payload_bytes", DEFAULT_MAX))
        except WireFailure as error:
            return row | {"outcome": "rejected", "wire_error_code": error.code, "terminal_scope": "connection"}
        row |= {"outcome": "accepted", "consumed_length": consumed}
        if vector.get("roundtrip_opaque"):
            kind = frame.fields["kind"]
            field = "result" if kind == "response" else "payload" if kind == "event" else None
            if field is None or field not in frame.opaque:
                raise SchemaError("opaque case lacks an opaque received field")
            encoded = encode(frame)
            _, output_spans = _wire_object(encoded[4:])
            row |= {
                "opaque_field": kind + "." + field,
                "opaque_input_hex": frame.opaque[field].hex(),
                "opaque_output_hex": output_spans[field].hex(),
            }
        if vector.get("check_unknown_fallback"):
            try:
                encode(frame)
                reencode_outcome = "emitted"
            except LocalRefusal:
                reencode_outcome = "refused"
            row |= {
                "effective_code": frame.fields.get("code"),
                "raw_error_code": frame.raw_error_code,
                "fallback_scope": "request" if "id" in frame.fields else "connection",
                "reencode_outcome": reencode_outcome,
            }
        return row
    if stage == "admit":
        gate = Gate(vector["role"], vector["starting_state"])
        for previous in vector.get("preceding", []):
            try:
                gate.admit(_admit_frame(previous))
            except WireFailure:
                pass  # Deliberate terminal history must remain terminal.
        try:
            gate.admit(_admit_frame(vector["frame"]))
            return row | {"outcome": "admitted"}
        except WireFailure as error:
            return row | {"outcome": "rejected", "wire_error_code": error.code, "terminal_scope": "connection"}
    try:
        encode(_validate(vector["frame"], {}, received=False))
        return row | {"outcome": "emitted"}
    except (WireFailure, LocalRefusal):
        return row | {"outcome": "refused"}


def main() -> int:
    try:
        if len(sys.argv) != 2:
            raise SchemaError("usage: oracle.py <vectors.json>")
        vectors = json.loads(
            Path(sys.argv[1]).read_text(encoding="utf-8"),
            object_pairs_hook=_metadata_object, parse_constant=_metadata_constant,
        )
        if not isinstance(vectors, list) or not vectors:
            raise SchemaError("vector file must be a nonempty array")
        seen: set[str] = set()
        for vector in vectors:
            validate_vector(vector)
            if vector["id"] in seen:
                raise SchemaError("duplicate vector id " + vector["id"])
            seen.add(vector["id"])
        for vector in vectors:
            row = observe(vector)
            print(json.dumps(row, separators=(",", ":"), ensure_ascii=True, allow_nan=False))
        return 0
    except (SchemaError, OSError, UnicodeError, json.JSONDecodeError) as error:
        print("oracle: " + str(error), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())

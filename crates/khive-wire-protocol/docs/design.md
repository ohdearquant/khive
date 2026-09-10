# khive-wire-protocol Design

## Purpose

`khive-wire-protocol` is the normative, transport-independent implementation of ADR-137's khive
frame protocol. It defines the closed frame grammar, length-prefixed codec, handshake state
machine, protocol versions, and wire-error taxonomy. It performs no socket or asynchronous I/O.

## Key types and modules

- `Frame` is the internally tagged JSON union for the eleven protocol frame kinds.
  `OperationId` rejects empty ids, and `Cursor` is the ordered event cursor type.
- `FrameCodec` and the free encode/decode functions operate on complete in-memory frames.
  `CodecError` preserves the exact framing or grammar failure and maps it to a wire error code.
- `HandshakeGate` is the forward-only server-side admission state machine. `HandshakeOutcome`
  distinguishes an accepted handshake, a version rejection, and an ordinary admitted frame.
- `ProtocolVersion`, `SupportedVersions`, and `CURRENT_VERSION` define compatibility boundaries.
- `WireErrorCode` and `TerminalScope` define whether a wire failure closes the connection or only
  terminates the operation id it echoes.

## Framing and grammar

Each frame is a four-byte big-endian `u32` JSON-payload length followed by exactly that many payload
bytes. The length excludes its own prefix. The default payload limit is 8 MiB; a `FrameCodec` may
use a different explicit bound for both encode and decode.

The JSON payload is an object with a string `kind` discriminant. Version 1 recognizes
`handshake`, `handshake_ack`, `request`, `response`, `error`, `cancel`, `subscribe`,
`subscribe_ack`, `unsubscribe`, `unsubscribe_ack`, and `event`. Kinds and the fields belonging to
each kind are closed. The opaque `response.result` and `event.payload` values remain ordinary JSON
data and are not part of that field grammar.

## Invariants

- The codec does not buffer partial reads. A transport supplies one complete frame, or uses
  `decode_with_consumed` to split a larger received buffer. After a decode error, stream position
  is not recoverable; the connection must not attempt frame resynchronization.
- Unknown kinds, missing discriminants, invalid required fields, empty operation ids, oversized
  frames, and unknown fields are rejected. Optional explicit `null` decodes as absence, duplicate
  object members follow serde JSON's last-value-wins behavior, and opaque values preserve semantic
  rather than byte-for-byte equality.
- A handshake must be the first application frame. Once completed, the server-side gate admits
  only client-to-server kinds; duplicate handshakes and server-only inbound frames close the gate.
  `HandshakeGate` is deliberately not `Clone`, so its per-connection state cannot move backward.
- Connection-terminal errors carry no operation id. Request-terminal errors carry the id they
  terminate. The rule is enforced on encode and, for recognized (closed-set) codes, on every
  `Frame` decode path. An unknown code's scope is unknowable in this protocol version, so
  unknown codes bypass scope validation on decode.
- An unrecognized wire error code decodes as `internal` while retaining the raw code for
  diagnostics. That fallback frame cannot be re-encoded because doing so would discard the newer
  code; a transparent relay must retain raw bytes instead.
- Version 0 does not exist. Supported ranges are inclusive, non-inverted, and cannot name a version
  newer than the grammar implemented by this crate. A breaking kind, field, or error-code change
  requires a protocol-version bump.
- Transport identity, peer authorization, request-DSL semantics, and per-topic event payload
  catalogs belong to higher layers and are intentionally not defined here.

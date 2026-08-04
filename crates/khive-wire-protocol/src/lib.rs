//! # khive wire protocol
//!
//! This crate is the normative specification of khive's frame protocol
//! (ADR-137: "Tailnet Wire Transport for the khive Frame Protocol"). It
//! defines framing, the handshake sequence, every frame kind and its
//! fields, the closed wire error taxonomy, and protocol versioning. A
//! non-Rust client can implement this protocol from this documentation
//! alone — that is the ADR's stated bar for this crate, and the sections
//! below are written to meet it.
//!
//! This crate does no I/O and depends on no async runtime. It is types, a
//! byte-buffer codec, and a handshake state machine; a transport crate
//! (Unix-domain socket, tailnet TCP, or any other carrier of the same
//! framing) drives it.
//!
//! ## Framing
//!
//! One wire frame is:
//!
//! ```text
//! +----------------------------+----------------------------------+
//! | length: u32, big-endian    | payload: `length` bytes of JSON  |
//! | (4 bytes)                  |                                  |
//! +----------------------------+----------------------------------+
//! ```
//!
//! `length` is the byte length of the JSON payload only — it does not
//! include itself. This is the same framing the existing Unix-domain-socket
//! transport uses (`crates/khive-runtime/src/daemon.rs`, `read_frame` /
//! `write_frame`): a 4-byte big-endian `u32` length prefix followed by JSON
//! bytes. This crate defines what that JSON is; it does not change the
//! outer framing.
//!
//! The default maximum frame size is **8 MiB**
//! ([`codec::DEFAULT_MAX_FRAME_BYTES`]), matching the existing transport's
//! configured limit. A deployment may configure a different maximum via
//! [`codec::FrameCodec::new`]. A frame whose declared length exceeds the
//! configured maximum is rejected at decode with
//! [`codec::CodecError::FrameTooLarge`], which a server maps to the wire
//! error [`error::WireErrorCode::FrameTooLarge`] and closes the connection
//! — the frame is never partially buffered.
//!
//! [`codec::decode_frame`] and [`codec::encode_frame`] (or the
//! [`codec::FrameCodec`] wrapper) operate on one complete frame's bytes at a
//! time. They do not support partial/streaming reads: a transport reading
//! from a socket is responsible for buffering until it has the 4-byte
//! prefix and then the declared number of payload bytes before calling
//! into this crate.
//!
//! ## The JSON payload: frame kind and fields
//!
//! Every payload is a JSON object with a `"kind"` string field naming one
//! of the eleven closed frame kinds, plus that kind's own fields flattened
//! into the same object (an internally tagged encoding). For example, a
//! `cancel` frame referencing operation id `"op-42"`:
//!
//! ```json
//! {"kind":"cancel","id":"op-42"}
//! ```
//!
//! The eleven frame kinds ([`frame::FRAME_KINDS`]), grouped by role:
//!
//! | Kind               | Direction       | Fields                                                        | Role |
//! | ------------------ | --------------- | -------------------------------------------------------------- | ---- |
//! | `handshake`        | client → server | `version: u32`                                                 | First frame on every connection; names the client's protocol version. |
//! | `handshake_ack`    | server → client | `version: u32`                                                 | Accepts a `handshake`; names the version the connection now speaks. |
//! | `request`          | client → server | `id: string`, `ops: string`, `deadline_ms?: u64`, `namespace?: string`, `actor_id?: string`, `visible_namespaces?: string[]` | One DSL batch/chain (ADR-016) to execute. The three optional identity-override fields exist for transports that accept caller-supplied context; a mapped transport rejects a request carrying any of them with `context_rejected`. |
//! | `response`         | server → client | `id: string`, `result: json`                                   | Successful terminal frame for a `request`; `result` is the verb-dispatch result, opaque to this crate. |
//! | `error`            | server → client | `id?: string`, `code: string`, `message: string`               | Wire-level failure. `id` is present for a request-scoped error, absent for a connection-terminal error. |
//! | `cancel`           | client → server | `id: string`                                                    | Asks the server to terminate the named `request`. No-op on an unknown/subscribe/unsubscribe/already-terminal id. |
//! | `subscribe`        | client → server | `id: string`, `topic: string`, `resume_cursor?: u64`            | Opens delivery for one topic. |
//! | `subscribe_ack`    | server → client | `id: string`, `topic: string`, `start_cursor: u64`              | Confirms a `subscribe`; names the cursor delivery begins after. |
//! | `unsubscribe`      | client → server | `id: string`, `topic: string`                                   | Ends delivery for one topic. Idempotent no-op if not subscribed. |
//! | `unsubscribe_ack`  | server → client | `id: string`, `topic: string`                                   | Confirms an `unsubscribe`. |
//! | `event`            | server → client | `topic: string`, `cursor: u64`, `occurred_at: string`, `payload: json` | One state-change delivery. Carries no operation id — correlated by topic and ordered by `cursor` instead. `occurred_at` is RFC 3339. `payload`'s field-by-field shape is owned by the per-topic catalog (ADR-137, "Implementation-phase deliverables"), not by this crate. |
//!
//! The set is closed within one protocol version. A decoder that sees a
//! `"kind"` value outside this table rejects the frame
//! ([`codec::CodecError::UnknownFrameKind`]) rather than skipping it.
//!
//! ## Strict field rejection (closed grammar)
//!
//! The grammar is closed in both dimensions — kinds AND fields. Every
//! payload is parsed by its kind's payload struct
//! ([`frame::HandshakePayload`], [`frame::RequestPayload`], ...), each of
//! which carries `#[serde(deny_unknown_fields)]`: a payload carrying any
//! field its kind does not declare is rejected with
//! [`codec::CodecError::InvalidFields`], never silently ignored. Unknown
//! fields are rejected within a protocol version; forward compatibility is
//! carried by the version handshake ([`version::ProtocolVersion`]), not by
//! field tolerance — new fields arrive by bumping the protocol version and
//! teaching the new version's grammar about them. This is the fail-closed
//! posture ADR-137 requires: a decoder never guesses that an unrecognized
//! field is ignorable.
//!
//! The strictness covers the fields each frame KIND declares. The two
//! opaque JSON values — `response.result` and `event.payload` — are data,
//! not grammar: keys inside them are preserved, and their field-by-field
//! shape is owned by the verb result surface (ADR-016) and the per-topic
//! event catalog respectively, not by this crate.
//!
//! Three deliberate boundaries of the strictness, stated so nobody
//! re-derives them:
//!
//! - **Explicit `null` on an optional field is equivalent to absence.**
//!   Optional payload fields are `Option<T>`; a member present with value
//!   `null` decodes as absent and re-encodes with the member omitted. No
//!   frame distinguishes present-null from absent.
//! - **Duplicate members are last-wins, not rejected.** Payloads pass
//!   through `serde_json`'s object model before field checking, so a
//!   duplicated member name silently keeps the last occurrence —
//!   `deny_unknown_fields` cannot see the earlier one. Rejecting
//!   duplicates would require validating the raw document; the grammar
//!   takes the documented last-wins stance instead.
//! - **`topic` syntax is not validated here.** The codec accepts any JSON
//!   string (including empty) for `subscribe`/`unsubscribe`/`event`
//!   topics; the `<domain>.<event>` shape is enforced by the server
//!   against its topic catalog, where the catalog lives.
//!
//! ## Opaque payload fidelity
//!
//! `response.result` and `event.payload` are preserved as JSON VALUES —
//! semantic equality, not byte-for-byte. Decoding and re-encoding yields a
//! payload semantically equal to the original under the JSON data model,
//! but the wire bytes may differ in two documented ways, both consequences
//! of this workspace's `serde_json` feature set (default features only —
//! no `preserve_order`, no `arbitrary_precision`):
//!
//! - **Object key order is not preserved.** Decoded objects use
//!   `serde_json`'s `BTreeMap`-backed map and re-encode with keys in
//!   sorted order. Consumers must treat key order as insignificant.
//! - **Integers outside the u64/i64 range lose precision.** Integers
//!   within u64/i64 range — including values above 2^53 — parse and
//!   re-encode exactly; anything outside that range parses as `f64` and
//!   may lose precision.
//!
//! The codec's `opaque_payloads_are_preserved_semantically_not_byte_for_byte`
//! test pins exactly this behavior.
//!
//! ## Server-produced fields
//!
//! `event.occurred_at` (RFC 3339) and `event.topic` are SERVER-PRODUCED
//! fields: the server validates them when it produces an event, and the
//! codec accepts them as plain strings without parsing. This crate
//! deliberately takes no timestamp-parsing dependency for decode-side
//! validation of server-produced data.
//!
//! ## Handshake sequence
//!
//! 1. The client opens the transport connection (Unix-domain socket or
//!    tailnet TCP) and sends `handshake` as its first frame, naming the
//!    highest protocol version it supports.
//! 2. The server checks that version against its own supported range
//!    ([`version::SupportedVersions`]):
//!    - If supported, it replies `handshake_ack` naming the accepted
//!      version. The connection now accepts `request`, `subscribe`,
//!      `unsubscribe`, and `cancel` frames.
//!    - If not supported, it replies `error` with code
//!      `unsupported_version` (no `id` — connection-terminal) and closes
//!      the connection. The client must surface this rejection; it must
//!      not fall back to a different protocol or a local code path.
//! 3. No `request`, `subscribe`, `unsubscribe`, or `cancel` frame is valid
//!    before step 2 completes successfully.
//!
//! [`handshake::HandshakeGate`] implements the server side of this sequence
//! as a type: it is fed every inbound frame and returns an admit/accept/
//! reject decision, so "no request frame before handshake completes" is a
//! property of the gate's API rather than a rule every call site has to
//! remember to check.
//!
//! ## Wire error taxonomy
//!
//! [`error::WireErrorCode`] is the closed set of wire-level error codes for
//! protocol version 1, matching ADR-137's "Wire error taxonomy" table
//! exactly (serialized as the `snake_case` names below). A wire error is
//! distinct from a DSL-level per-operation error (ADR-016's `{ok: false,
//! error}` result carried inside a *successful* `response` frame) — a wire
//! error is returned instead of, or before, DSL dispatch.
//!
//! | Code                        | Condition                                                                                      | Terminal |
//! | ---------------------------- | ----------------------------------------------------------------------------------------------- | -------- |
//! | `unsupported_version`        | Handshake named no mutually supported protocol version.                                        | Connection |
//! | `identity_rejected`          | Whois lookup failed, no/invalid node mapping, or native-frame ingress by a class without access. | Connection |
//! | `malformed_frame`            | A frame cannot be decoded or violates the frame grammar.                                        | Connection |
//! | `frame_too_large`            | A frame exceeds the configured maximum size.                                                    | Connection |
//! | `subscriber_overflow`        | The connection's bounded outbound event queue reached its limit.                                | Connection |
//! | `subscription_revoked`       | A live mapping/class change short of deletion removed authorization for an active subscription. | Connection |
//! | `context_rejected`           | A frame-level attempt to supply `namespace`/`actor_id`/`visible_namespaces` on a mapped transport. | Request |
//! | `peer_class_denied`          | A parsed operation names a verb outside the mapped class allowlist.                             | Request |
//! | `subscription_denied`        | A `subscribe` names a topic outside the mapped class's allowed-topic set.                       | Request |
//! | `already_subscribed`         | A `subscribe` names a topic with an already-active subscription on this connection.             | Request |
//! | `cursor_expired`             | A `subscribe` resume cursor is older than the topic's retention window.                         | Request |
//! | `in_flight_limit_exceeded`   | A `request` arrives while the per-connection in-flight limit is reached.                        | Request |
//! | `deadline_exceeded`          | The server abandoned the request at its deadline.                                               | Request |
//! | `cancelled`                  | The request was terminated by a `cancel` frame before normal completion.                        | Request |
//! | `shutting_down`              | The server is draining and refuses new work.                                                    | Request |
//! | `internal`                   | An unclassified server-side transport failure — also the fallback for an unrecognized code.     | Request |
//!
//! A **connection-terminal** error is followed by connection close and
//! carries no operation id. A **request-terminal** error terminates only
//! the operation id it echoes; the connection stays usable. The codec
//! enforces that pairing at decode time
//! ([`codec::CodecError::InconsistentErrorScope`]): an `error` frame whose
//! id presence contradicts its code's terminal scope is rejected, never
//! represented. The check covers the codes in the closed set
//! ([`error::WIRE_ERROR_CODES`]); an unrecognized code falls back to
//! `internal` and is processed per the fallback rule above (its true scope
//! is unknown to this version, so the pairing is not enforced for it).
//! The set is
//! closed within a protocol version: adding a code requires a version bump,
//! and a client that decodes a code it does not recognize treats it as
//! `internal` (request-terminal) rather than inventing semantics for it —
//! [`error::WireErrorCode`] implements this with `#[serde(other)]`.
//!
//! ## Versioning and compatibility
//!
//! Protocol version numbers ([`version::ProtocolVersion`]) are monotonic
//! `u32`s. A server supports the current version and at least the
//! immediately prior version ([`version::SupportedVersions::current`]). A
//! breaking wire change — a new frame kind, a new wire error code, or a
//! non-backward-compatible change to an existing frame's fields — is never
//! introduced within a version number; it requires incrementing the
//! version and updating this documentation as the normative source.
//!
//! ## What this crate does not define
//!
//! - Transport (socket binding, TLS/WireGuard, `tailscale whois`, peer
//!   mapping) — ADR-137's "Transport identity and actor mapping".
//! - The DSL carried in a `request` frame's `ops` string — ADR-016.
//! - The per-topic `event` payload catalog — ADR-137's
//!   "Implementation-phase deliverables".
//! - Peer-class allowlists and dispatch enforcement — ADR-137's "Peer
//!   classes and dispatch enforcement".

pub mod codec;
pub mod error;
pub mod frame;
pub mod handshake;
pub mod version;

pub use codec::{
    decode_frame, encode_frame, encode_frame_with_max, CodecError, FrameCodec,
    DEFAULT_MAX_FRAME_BYTES,
};
pub use error::{TerminalScope, WireErrorCode, WIRE_ERROR_CODES};
pub use frame::{Cursor, Frame, OperationId, CLIENT_TO_SERVER_KINDS, FRAME_KINDS};
pub use handshake::{HandshakeGate, HandshakeOutcome, HandshakeSequenceError};
pub use version::{ProtocolVersion, SupportedVersions, CURRENT_VERSION};

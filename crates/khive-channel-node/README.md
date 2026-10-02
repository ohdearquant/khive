# khive-channel-node

Node wire protocol v1 cryptography, strict JSON encodings, and HTTPS calls from ADR-105 Appendix A.
Callers supply confirmed contact keys, choose policy, and commit received messages.

`NodeClient` provides contact lookup, submission, polling, receipt posting, and status reads. Its
read-only `OutboundSource` returns the current persisted envelope for the configured namespace and
`khive` slug. Submission never seals a new envelope. The stable request serializer preserves the
published byte order, and each request signs the body sent with a fresh nonce and timestamp.

`PinSource` distinguishes owner-confirmed keys from unconfirmed epochs, absent contacts, and pin
read failures. Directory keys never confer confirmation. Receipt results become verified only after
all persisted binding fields and the recipient's pinned signature agree. A node authentication
refusal returns `ChannelError::Auth`; its consumer must pause and retain pending transport rows.

Polling returns opened classifications or closed unopened reasons alongside the original delivery
object bytes. It writes no message or replay state and posts no receipt. The recipient receipt
binding is parsed from that same preserved object. The binary receipt signature input and the
original delivery JSON are distinct: the signature covers the binding and disposition, while the
original JSON is retained for a later quarantine commit. `ack` is a separate post-commit call.

`KeyFacility` exposes signing and authenticated HPKE operations without exporting private key
material. `InMemoryKeyFacility` generates volatile keys from OS randomness. Request authentication
signs the exact body bytes and request target. Receipt input construction is shared with
`khive-channel` and the runtime; signature verification uses the runtime's Ed25519 implementation.

The files under `tests/fixtures` contain published test values only, including deterministic test
keys. The W0 fixture copies the protocol's A.11 byte vectors and conformance cases for use by both
clients and hosted services. The source tests compare this copy with the authoritative ADR and
reproduce the RFC 9180 Auth-mode suite vector. Deterministic key/ephemeral constructors exist only
in test builds.

Timestamp JSON uses uppercase `T` and `Z`, rejects spaces, lowercase `t`/`z`, `-00:00`, leap seconds and fractions longer than nine digits; `sent_at` accepts `Z` or `+00:00`, while server timestamps accept numeric offsets and normalize to UTC `Z`.
Signing public keys must have a canonical Ed25519 encoding and must not be points of small order, both at enrolment and pinning.

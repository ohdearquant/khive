# khive-channel-node

Node wire protocol v1 cryptographic inputs and strict JSON encodings from ADR-105 Appendix A.
The crate performs no network or persistence operations. Callers supply confirmed contact keys,
choose policy, and commit received messages.

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

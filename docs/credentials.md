# Credential custody

[ADR-192 S1](adr/ADR-192-credential-and-producer-seams.md) provides named credential
references and an environment provider. Configuration contains names and provider
references; never put credential values in the configuration file.

```toml
[[credentials]]
name = "partner-token"
kind = "header"
provider = "env"
env_var = "PARTNER_TOKEN"
header = "X-Api-Key"

[[credentials]]
name = "receipt-current"
kind = "signing_key"
provider = "env"
env_var = "KHIVE_RECEIPT_KEY"

[visibility_receipts]
[[visibility_receipts.keys]]
id = "current-1"
credential = "receipt-current"
encrypt = true
```

Each credential requires a unique, nonempty name, a provider, and one of `header`,
`basic`, `cookie_jar`, or `signing_key`. The `env` provider requires an `env_var`
name. Only a `header` credential accepts and requires `header`, which must be an
HTTP header field name. All credential and receipt tables reject unknown fields
at config load. A configured receipt ring requires exactly one `encrypt = true`
key, unique IDs of 1–64 ASCII letters, digits, `.`, `_`, or `-`, and references to
`signing_key` credentials. Other ring keys default to decrypt-only.

The environment provider resolves on every call and does not cache material.
Missing, empty, or non-Unicode values fail with the credential name and a fixed
reason. Resolution does not log values, and material has a redacted `Debug`, no
`Display` or serialization, and no public bytes/string accessor. Its owned buffer
is zeroized on drop. A process environment remains under the host's custody;
zeroizing the returned buffer does not erase the host's environment.

Hosts compose a `CredentialRegistry` from parsed declarations and may register
additional implementations of `CredentialProvider` under names referenced by
configuration. No file, keychain, vault, or refresh provider ships here. A provider
declares `NoCache` or `Process` cache lifetime. Provider failures are reduced to
fixed registry errors; provider-supplied error detail is not propagated. The
registry only calls `update` for `cookie_jar` credentials. That optional method
receives caller-supplied, zeroizing replacement bytes; it does not expose bytes
from a resolved opaque credential. The environment provider does not support
updates.

Receipt signing-key material is canonical, unpadded base64url encoding of exactly
32 bytes (43 characters). Only the runtime-private receipt sealer decodes it.
Deployments must supply the same immutable ID-to-key mapping across replicas and
restarts; the sealer never creates a replacement key when one is unavailable.
Retain retired decrypt-only keys for at least the accepted receipt lifetime plus
clock skew before removing them, as specified by
[ADR-144 Amendment 2](adr/ADR-144-memory-write-visibility-fence.md).

The sealer implements the accepted v2 envelope using XChaCha20-Poly1305 and a fresh
24-byte OS nonce. Its AAD is the ASCII bytes `khive.memory.visibility` followed by
the version byte, the one-byte key-ID length, and the key-ID bytes. This concrete
encoding binds the accepted purpose, version, and key ID. It is internal to the
server; clients treat receipts as opaque.

This stage supplies custody and a private seal/open capability. It does not change
`memory.remember`, recall, v1 receipts, or receipt issuance. Web request binding and
cookie hooks are later stages. ADR-192 C2 (resolution/error redaction) and C8 (the
store secret-gate backstop) have regressions here; request-pipeline C1 and C3–C7
await that wiring. The store gate is a backstop, while keeping material out of
ordinary request payloads and persisted records is the primary boundary.

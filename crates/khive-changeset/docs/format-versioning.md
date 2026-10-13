# NDJSON Format Versioning

The change-set format chooses a single explicit schema version and fail-loud decoding. This keeps durable proposal artifacts auditable and prevents an older binary from partially interpreting a newer mutation shape.

The envelope is the only version carrier because all following lines belong to the same change-set contract. Encoding always writes the current version, and decoding checks it before parsing any operation. Unknown fields are rejected throughout the wire shape for the same reason: a field the current code cannot interpret must not disappear silently during review or reserialization.

NDJSON keeps the envelope and each operation independently inspectable while retaining append-friendly line boundaries. Order remains part of the contract, and blank lines are not comments or separators; treating them as malformed keeps physical line numbers and semantic operation positions aligned.

## Version 2: restore

Version 2 adds `restore`, carrying a target ID and a strict full live record.
Both encoding and decoding accept only version 2. Version-1 decoders refuse the
new envelope before parsing its operations; version-2 decoders likewise refuse
version-1 files. There is no automatic upgrade or best-effort mixed-version
interpretation. A caller must deliberately restage a legacy artifact against the
current contract rather than merely relabel its envelope.

# NDJSON Format Versioning

The change-set format chooses a single explicit schema version and fail-loud decoding. This keeps durable proposal artifacts auditable and prevents an older binary from partially interpreting a newer mutation shape.

The envelope is the only version carrier because all following lines belong to the same change-set contract. Encoding always writes the current version, and decoding checks it before parsing any operation. Unknown fields are rejected throughout the wire shape for the same reason: a field the current code cannot interpret must not disappear silently during review or reserialization.

NDJSON keeps the envelope and each operation independently inspectable while retaining append-friendly line boundaries. Order remains part of the contract, and blank lines are not comments or separators; treating them as malformed keeps physical line numbers and semantic operation positions aligned.

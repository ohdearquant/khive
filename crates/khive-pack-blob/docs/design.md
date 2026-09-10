# khive-pack-blob Design

## Purpose

`khive-pack-blob` exposes the runtime's installed content-addressed blob service through three MCP
verbs: `blob.put`, `blob.get`, and `blob.stat`. It is a thin wire adapter over `BlobStore` and
`BlobHydrator`; it does not implement a storage backend, schema, or graph vocabulary.

## Key types and modules

- `BlobPack` holds the `KhiveRuntime` used to resolve the installed store and shared hydrator.
- `pack.rs` declares the three agent-visible verbs, inventory-registers the factory, and dispatches
  calls to `handlers.rs`.
- `handlers.rs` validates base64 payloads, strict content references and optional ranges, enforces
  memory/wire bounds, and shapes verb responses.
- `ContentRef` is the canonical lowercase-hex BLAKE3 identity supplied by `khive-storage`.
- `vocab.rs` contributes no entity or note kinds; typed artifact/reference modeling is a separate
  layer.

## Verb contracts

- `blob.put(bytes)` decodes base64, stores the bytes, and returns `{content_ref, size}`. Content
  addressing makes identical puts idempotent.
- `blob.get(content_ref, range?)` verifies and hydrates the complete object through the runtime's
  shared admission controller, then optionally slices it and returns base64 bytes.
- `blob.stat(content_ref)` reports existence and size through metadata only; it neither hydrates
  bytes nor implies a lease or reservation.

## Invariants

- The verb surface accepts bytes, never a server-local file path. This prevents a remote caller
  from turning `blob.put` into host-file exfiltration.
- Put and whole-object hydration share ADR-111's 64 MiB object ceiling, giving filesystem and S3
  backends the same externally visible acceptance limit.
- A `blob.get` response must also fit the daemon frame limit after base64 expansion. Callers use a
  smaller range when a whole object cannot fit on the wire, even though range slicing currently
  happens after full verified hydration.
- `blob.get` uses digest-verified hydration; `blob.stat` deliberately does not claim digest
  verification because it never reads the content.
- `blob.put` is unavailable on a read-only runtime. Reads remain available when a store is
  installed.
- Physical deletion and orphan sweeping remain administrator-only operations and are not pack
  verbs.

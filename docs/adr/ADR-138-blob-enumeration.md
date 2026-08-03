# ADR-138: Add read-only blob enumeration to the blob-store contract

- **Status:** Proposed
- **Date:** 2026-08-03

## Context

ADR-111 defines a capability-by-hash model in which a caller cannot enumerate content references, so possession of a reference is the basis for by-reference reads. (Source: ADR-111, Amendment 4, lines 637-661 at `origin/main`.)

Measured on a development deployment, `blob.list` was not available; the `origin/main` blob handler table and dispatcher register only `blob.put`, `blob.get`, and `blob.stat`. (Source: `origin/main:crates/khive-pack-blob/src/pack.rs:12-63, 99-113`.)

The original absence of enumeration therefore followed the capability-by-hash model rather than an omission from the blob pack. (Source: ADR-111, Amendment 4, lines 637-661 at `origin/main`.)

The existing `blob.get` verb returns bytes, whereas `blob.stat` returns metadata without hydrating bytes; a browse operation needs the latter property while discovering available content references. (Source: `origin/main:crates/khive-pack-blob/src/pack.rs:28-61`.)

## Decision

ADR-111 is amended to add the read-only `blob.list` verb, backed by a new provider-neutral list-metadata index rather than by an extension to the `BlobStore` trait's by-reference methods. (Source: ADR-111 §3 `BlobStore` trait, lines 89-108, and Amendment 2 `size`, lines 592-607, at `origin/main`; neither defines enumeration or a creation timestamp, so this ADR introduces both as new surface rather than citing an existing contract for them.)

### List-metadata index

Every `BlobStore` implementation that serves `blob.list` maintains a separate, authoritative list-metadata index alongside its object storage. The index is not part of the `put`/`get`/`exists`/`delete`/`size` capability-by-hash surface and does not change how a `ContentRef` is computed or resolved. (Source: ADR-111 §3, lines 89-108 at `origin/main`.)

The index records one row per distinct `ContentRef` ever accepted by `put`, each carrying `content_ref`, `size`, and `created_at`. A backend that does not maintain the index rejects `blob.list` with a distinct "enumeration unsupported" error rather than returning a partial or best-effort listing; `blob.get`, `blob.stat`, and `blob.exists` remain governed solely by the existing capability-by-hash contract and never consult the index.

**Lifecycle.** A successful `put` that creates a new object also inserts its index row in the same logical operation, with `created_at` set to that put's acceptance time. A `put` for a `ContentRef` already present in the store (global dedup) is a no-op against the index: the existing row and its original `created_at` are unchanged. A successful `delete` removes the index row for that `ContentRef`. A subsequent `put` of the same content after a `delete` creates a new row with a new `created_at`; the object is not "resurrected" with its prior creation time. The index is read-your-writes consistent with the backend's own `put`/`delete` calls; it is not required to be consistent with a concurrent writer's in-flight, not-yet-completed call.

`blob.list(cursor?, limit?, namespace?)` returns a paginated sequence of metadata-only items drawn from the index: `content_ref`, `size`, and `created_at`, plus an opaque `next_cursor` when another page is available.

`blob.list` must not hydrate or return blob bytes, and it is an Assertive, read-only verb. (Source: `origin/main:crates/khive-pack-blob/src/pack.rs:28-61`.)

The optional `namespace` parameter filters enumerated references by the caller-visible namespace association at the dispatch gate; it does not partition the content-addressed object store or change a `ContentRef`. (Source: ADR-111, lines 654-685 at `origin/main`, the multi-tenant put-ledger described below.)

For a multi-tenant hosted deployment, `blob.list` applies the existing `(namespace, ContentRef)` gate put-ledger before emitting a row, exactly as `blob.get` and `blob.stat` already do for by-reference reads. (Source: ADR-111, lines 671-685 at `origin/main`.)

### Pagination contract

`blob.list` orders rows by `created_at` descending, with `content_ref` ascending as the tie-breaker for rows sharing a `created_at` value; this is a total order, so two identical calls against an unchanged index return rows in the same sequence. There is no other sort option in v1.

`cursor` is an opaque token encoding the `(created_at, content_ref)` of the last row returned on the prior page, together with the `namespace` value (including its absence) that produced that page. A `cursor` that does not decode to that shape, that decodes but does not correspond to a value the index could have produced, or that is replayed with a `namespace` argument different from the one it encodes, is rejected with a validation error rather than silently treated as "start of list" or silently resumed under the new filter.

`limit` is an integer, minimum 1, maximum 500, default 100; a value outside that range is a validation error, not a silent clamp.

The verb-specific success value is `{ "items": [ { "content_ref", "size", "created_at" }, ... ], "next_cursor": <string> | null }`, returned as the `result` field of the per-op envelope defined by the request DSL (`ok`/`tool`/`result` — ADR-016). `next_cursor` is `null` exactly when the returned page is the last page.

## Consequences

Callers can browse blob metadata in pages without using `blob.get` to hydrate bytes. (Source: `origin/main:crates/khive-pack-blob/src/pack.rs:28-61`.)

The new enumeration surface changes the prior assumption that a caller cannot learn content references, so the dispatch gate must enforce the same namespace visibility policy documented for multi-tenant reads. (Source: ADR-111, lines 654-685 at `origin/main`.)

The blob implementation needs a new list-metadata index, maintained at `put` and `delete` time, that supplies `content_ref`, `size`, and `created_at` without reading object bytes and without being treated as authoritative for object existence. (Source: `origin/main:crates/khive-pack-blob/src/pack.rs:28-61`; ADR-111 §3, lines 89-108 at `origin/main`.)

## Alternatives considered

1. **Keep only by-reference reads.** This was rejected because the registered surface exposes no browse verb and a caller cannot enumerate references under the prior capability-by-hash model. (Source: `origin/main:crates/khive-pack-blob/src/pack.rs:12-63`; ADR-111, Amendment 4, lines 637-661 at `origin/main`.)

2. **Use `blob.get` for browsing.** This was rejected because `blob.get` returns byte content, while `blob.stat` establishes the metadata-only read precedent. (Source: `origin/main:crates/khive-pack-blob/src/pack.rs:28-61`.)

3. **Return the entire catalogue in one response.** This was rejected because a paginated browse surface avoids requiring a caller to materialize every metadata row at once. (Source: ADR-111, Amendment 4, lines 654-661 at `origin/main`.)

4. **Partition physical blob storage by namespace.** This was rejected because ADR-111 identifies global deduplication as a design goal and assigns tenant visibility to the dispatch gate rather than the storage layout. (Source: ADR-111, lines 644-652 and 678-685 at `origin/main`.)

5. **Add `created_at` and iteration directly to the `BlobStore` trait's by-reference methods.** This was rejected because the trait's existing methods (`put`, `get`, `exists`, `delete`, `size`) are all keyed by a caller-supplied `ContentRef` and answer questions about one object; enumeration is a different capability with its own consistency and lifecycle rules, and folding it into the by-reference trait would force every implementation, including ones that never need `blob.list`, to carry index-maintenance code. A separate list-metadata index keeps the by-reference trait unchanged and makes enumeration support an explicit, checkable per-backend capability. (Source: ADR-111 §3, lines 89-108, and Amendment 2, lines 592-607, at `origin/main`.)

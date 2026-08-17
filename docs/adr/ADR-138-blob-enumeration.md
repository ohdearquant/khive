# ADR-138: Add read-only blob enumeration to the blob-store contract

- **Status:** Proposed
- **Date:** 2026-08-03

## Context

ADR-111 defines a capability-by-hash model in which a caller cannot enumerate content references, so possession of a reference is the basis for by-reference reads. (Source: [ADR-111](ADR-111-blob-store.md), Amendment 4 "capability-by-hash authorization model".)

At the base commit this proposal is written against, the blob pack registers only `blob.put`, `blob.get`, and `blob.stat`, in both its handler table and its dispatcher. (Source: [pack.rs lines 12-63 at 9442ec2](https://github.com/ohdearquant/khive/blob/9442ec2c52290120c5bf4a4c8a1dc771102658dd/crates/khive-pack-blob/src/pack.rs#L12-L63) and [lines 99-113](https://github.com/ohdearquant/khive/blob/9442ec2c52290120c5bf4a4c8a1dc771102658dd/crates/khive-pack-blob/src/pack.rs#L99-L113).)

The original absence of enumeration therefore followed the capability-by-hash model rather than an omission from the blob pack. (Source: [ADR-111](ADR-111-blob-store.md), Amendment 4.)

The existing `blob.get` verb returns bytes, whereas `blob.stat` returns metadata without hydrating bytes; a browse operation needs the latter property while discovering available content references. (Source: [pack.rs lines 12-63 at 9442ec2](https://github.com/ohdearquant/khive/blob/9442ec2c52290120c5bf4a4c8a1dc771102658dd/crates/khive-pack-blob/src/pack.rs#L12-L63).)

In a multi-tenant hosted deployment, ADR-111 Amendment 4 makes the `(namespace, ContentRef)` put-ledger mandatory gate policy and explicitly forbids adding a handler-level check in its place, while the accepted gate interface returns exactly one pre-dispatch allow/deny decision per operation — before a list operation has produced any candidate rows to judge. (Source: [ADR-111](ADR-111-blob-store.md), Amendment 4; [gate.rs lines 9-17 at 9442ec2](https://github.com/ohdearquant/khive/blob/9442ec2c52290120c5bf4a4c8a1dc771102658dd/crates/khive-gate/src/gate.rs#L9-L17).)

## Decision

ADR-111 is amended to add the read-only `blob.list` verb, backed by a new provider-neutral catalog capability rather than by an extension to the `BlobStore` trait's by-reference methods. (Source: [ADR-111](ADR-111-blob-store.md) §3 "BlobStore trait" and Amendment 3 "size accessor"; neither defines enumeration or a creation timestamp, so this ADR introduces both as new surface rather than citing an existing contract for them.)

### Deployment scope: single-tenant only in v1

`blob.list` is a single-tenant, operator-deployment surface in v1, and it takes no `namespace` parameter. In a multi-tenant hosted deployment, `blob.list` is rejected with the "enumeration unsupported" error defined below.

This restriction follows from the accepted authorization contract rather than from taste: per-row tenant visibility for an enumeration verb requires a per-row decision, the gate's put-ledger is the only permitted authority for that decision, no handler-level check may be added in its place, and the accepted gate interface yields one pre-dispatch decision per operation with no per-row query seam. (Source: [ADR-111](ADR-111-blob-store.md), Amendment 4; [gate.rs lines 9-17 at 9442ec2](https://github.com/ohdearquant/khive/blob/9442ec2c52290120c5bf4a4c8a1dc771102658dd/crates/khive-gate/src/gate.rs#L9-L17).) A caller-supplied namespace value is never treated as authority. Lifting the restriction is named future work: a separate amendment must define a gate-owned, paginated visibility-query interface with a fail-closed contract, and `blob.list` becomes available to hosted tenants only after that interface exists and this verb is specified against it.

### The `BlobCatalog` capability and its index

Enumeration is an explicit, optional backend capability, `BlobCatalog`, declared by a `BlobStore` implementation and discoverable by the runtime at startup. A backend that does not declare it, or whose catalog bootstrap (below) has not completed, rejects `blob.list` with a distinct "enumeration unsupported" error rather than returning a partial or best-effort listing. `blob.get` and `blob.stat` remain governed solely by the existing capability-by-hash contract and never consult the catalog; the same is true of the `BlobStore` trait's `exists` accessor, which is a trait method consumed inside the backend boundary, not an MCP-callable verb. (Source: [ADR-111](ADR-111-blob-store.md) §3 "BlobStore trait".)

The catalog is a current-inventory index, stated as a converged-state invariant: when converged, it contains exactly one row per `ContentRef` currently present in the object store, each carrying `content_ref`, `size`, and `created_at`. While repair is pending after an index-step failure (see "Failure and repair"), it may temporarily omit a present object or retain a stale row. It is not a historical acceptance log. The index is never authoritative for object existence; the object store is.

`created_at` is a JSON string in canonical RFC 3339 UTC form with millisecond precision (for example `2026-08-03T19:41:00.000Z`). Its source is the serving backend's clock at the moment the index row is inserted — put acceptance for ordinary rows, scan time for bootstrapped or reconciled rows (below) — so it is an index-acceptance time, not a claim about the object's original creation.

**Lifecycle.** The object store is authoritative, and the index mutation is always the second, non-fatal step of a mutating call. A `put` publishes the object first, then performs its index step, keyed on the object store's own outcome: a `put` that actually inserted the object — because the content was absent, whether never stored or previously removed by `delete` or by a physical deletion path such as [ADR-111](ADR-111-blob-store.md) §8's orphan GC — inserts the row, or replaces a stale row left for the same `ContentRef`, with that put's acceptance time as `created_at`; a `put` deduplicated against an already-present object is a no-op against the index, leaving the existing row and its `created_at` unchanged. A `delete` removes the object first, then removes the row. In both directions, a failure of the index step never fails the object operation (see "Failure and repair"), so `blob.list` is explicitly **eventually consistent** with the caller's own completed mutations: in the normal case the index step lands inside the mutating call, and after an index-step failure the listing reflects the mutation only once reconciliation heals it. A caller that must confirm one object's presence uses `blob.stat`, which never consults the index.

**Bootstrap.** A store that predates the catalog, or that enables it after objects exist, builds the index by scanning the object store; backfilled rows take the scan time as `created_at`. `blob.list` is rejected as unsupported until the bootstrap scan completes.

**Failure and repair.** The index can diverge from the object store in both directions. A crash or index-write failure after object publication leaves an unlisted object, and a deduplicated retry of the same content does not recreate the missing row (the object is present, so the retry's index step is a no-op). A `delete` whose row removal fails leaves a stale row, as do physical deletion paths other than `delete` — most concretely ADR-111 §8's orphan GC. Neither divergence fails the object operation itself: `put` and `delete` succeed or fail on the capability-by-hash contract alone. Divergence is healed two ways: the re-put replacement rule above (a put that re-inserts a physically deleted object replaces its stale row with a fresh `created_at`), and reconciliation — an operator-triggered or scheduled sweep that compares the index against the object store, inserts missing rows (with the sweep time as `created_at`), and removes rows for objects no longer present. (Source: [ADR-111](ADR-111-blob-store.md) §8 "Orphan GC".)

### The verb

`blob.list(cursor?, limit?)` returns a paginated sequence of metadata-only items drawn from the index: `content_ref`, `size`, and `created_at`, plus an opaque `next_cursor` when another page is available.

`blob.list` must not hydrate or return blob bytes, and it is an Assertive, read-only verb. (Source: [pack.rs lines 12-63 at 9442ec2](https://github.com/ohdearquant/khive/blob/9442ec2c52290120c5bf4a4c8a1dc771102658dd/crates/khive-pack-blob/src/pack.rs#L12-L63).)

### Pagination contract

`blob.list` orders rows by `created_at` descending, with `content_ref` ascending as the tie-breaker for rows sharing a `created_at` value; this is a total order over the canonical `created_at` encoding, so two identical calls against an unchanged index return rows in the same sequence. There is no other sort option in v1.

Pagination is explicitly a best-effort live traversal of the current index, not a snapshot: a row inserted during traversal whose `created_at` sorts after the pages already returned will not appear in a continuation of that traversal, a row deleted during traversal simply stops appearing, and `next_cursor: null` means the traversal reached the end of the index as of that page's read. A caller that needs a consistent point-in-time inventory runs the traversal while writes are quiesced; the verb itself does not promise one.

`cursor` is an opaque token encoding the `(created_at, content_ref)` of the last row returned on the prior page, using the canonical `created_at` encoding above. Cursor validation is a shape check only: a `cursor` that does not decode to a well-formed `(created_at, content_ref)` pair in the canonical encoding is rejected with a validation error rather than silently treated as "start of list". A well-formed issued cursor is consumed purely as a position predicate in the total order — the next page begins strictly after that tuple — so it remains valid when its boundary row has since been deleted: continuation never requires the boundary row's current membership in the index, which is the same live-traversal behavior specified above.

`limit` is an integer, minimum 1, maximum 500, default 100; a value outside that range is a validation error, not a silent clamp.

The verb-specific success value is `{ "items": [ { "content_ref", "size", "created_at" }, ... ], "next_cursor": <string> | null }`, returned as the `result` field of the per-op envelope defined by the request DSL (`ok`/`tool`/`result` — ADR-016). `next_cursor` is `null` exactly when the returned page is the last page.

## Consequences

Callers in a single-tenant deployment can browse blob metadata in pages without using `blob.get` to hydrate bytes.

The multi-tenant hosted tier keeps its closed enumeration channel: `blob.list` stays unsupported there until the gate-owned per-row visibility interface named above exists, so this ADR does not weaken Amendment 4's capability-by-hash property for hosted tenants. (Source: [ADR-111](ADR-111-blob-store.md), Amendment 4.)

The blob implementation needs the `BlobCatalog` capability and its index, maintained at `put` and `delete` time and repaired by bootstrap and reconciliation sweeps, supplying `content_ref`, `size`, and `created_at` without reading object bytes and without being treated as authoritative for object existence.

## Alternatives considered

1. **Keep only by-reference reads.** This was rejected because the registered surface exposes no browse verb and a caller cannot enumerate references under the prior capability-by-hash model. (Source: [pack.rs lines 12-63 at 9442ec2](https://github.com/ohdearquant/khive/blob/9442ec2c52290120c5bf4a4c8a1dc771102658dd/crates/khive-pack-blob/src/pack.rs#L12-L63); [ADR-111](ADR-111-blob-store.md), Amendment 4.)

2. **Use `blob.get` for browsing.** This was rejected because `blob.get` returns byte content, while `blob.stat` establishes the metadata-only read precedent. (Source: [pack.rs lines 12-63 at 9442ec2](https://github.com/ohdearquant/khive/blob/9442ec2c52290120c5bf4a4c8a1dc771102658dd/crates/khive-pack-blob/src/pack.rs#L12-L63).)

3. **Return the entire catalogue in one response.** This was rejected because a paginated browse surface avoids requiring a caller to materialize every metadata row at once.

4. **Partition physical blob storage by namespace.** This was rejected because ADR-111 identifies global deduplication as a design goal and assigns tenant visibility to the dispatch gate rather than the storage layout. (Source: [ADR-111](ADR-111-blob-store.md), Amendment 4.)

5. **Add `created_at` and iteration directly to the `BlobStore` trait's by-reference methods.** This was rejected because the trait's existing methods (`put`, `get_bounded_verified`, `exists`, `delete`, `size`) are all keyed by a caller-supplied `ContentRef` and answer questions about one object; enumeration is a different capability with its own consistency and lifecycle rules, and folding it into the by-reference trait would force every implementation, including ones that never need `blob.list`, to carry index-maintenance code. A separate catalog capability keeps the by-reference trait unchanged and makes enumeration support an explicit, checkable per-backend property. (Source: [ADR-111](ADR-111-blob-store.md) §3 "BlobStore trait" and Amendment 3 "size accessor".)

6. **Serve hosted tenants by filtering rows in the handler against the put-ledger.** This was rejected because Amendment 4 makes the ledger gate policy and forbids handler-level checks in its place, and the accepted gate interface offers no per-row query for a handler to consume compliantly; a handler-side filter would re-create exactly the parallel authorization path the amendment closed. Hosted enumeration waits for the gate-owned visibility interface named in the deployment-scope section. (Source: [ADR-111](ADR-111-blob-store.md), Amendment 4; [gate.rs lines 9-17 at 9442ec2](https://github.com/ohdearquant/khive/blob/9442ec2c52290120c5bf4a4c8a1dc771102658dd/crates/khive-gate/src/gate.rs#L9-L17).)

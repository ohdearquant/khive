# ADR-110: WebAssembly Support for khive-vamana

- Status: Proposed
- Date: 2026-07-12
- Deciders: khive maintainers

## Context

khive-vamana is the workspace's Vamana/DiskANN-family ANN index. It provides
two-tier SQ8 steering (ADR-052), incremental insert, two-hop delete repair, and
fingerprint-gated v2 persistence (ADR-079). It currently targets native
environments. A browser build enables local-first search over exported knowledge
bases, offline retrieval demos, and edge runtimes where a native binary cannot
ship.

The SQ8 acquisition tier makes this useful in browser memory budgets. It keeps a
compressed steering representation and reranks a small candidate set with full
precision vectors. Full-precision-only indexes remain supported.

### Source audit: parallel execution

The portability boundary is based on the current source, not on a crate-level
dependency summary. These are all direct Rayon iterator call sites in
`index.rs`, `graph.rs`, and `khive-quant/src/lib.rs`:

| Source site                                                         | Current parallel operation                                                                             | `parallel`-off equivalent                                                                           |
| ------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------- |
| `graph.rs:181-182`, `VamanaGraph::build`                            | `batch.par_iter().zip(batch_prior.par_iter())` computes the f32 build proposals                        | `batch.iter().zip(batch_prior.iter())` with the same proposal body and ordered collection           |
| `graph.rs:318-319`, `VamanaGraph::build_sq8`                        | `batch.par_iter().zip(batch_prior.par_iter())` computes the SQ8 build proposals                        | `batch.iter().zip(batch_prior.iter())` with the same proposal body and ordered collection           |
| `graph.rs:1051-1067`, `select_medoid`                               | `(0..num_vectors).into_par_iter().map(...).reduce(...)` selects the nearest vector to the sampled mean | A sequential range iterator and fold using the same distance comparison and lower-ordinal tie break |
| `index.rs:1987-1998`, `exact_search`                                | `(0..n).into_par_iter()` filters tombstones and calculates exact distances                             | A sequential range iterator with the same filter, map, top-k selection, and final ordering          |
| `khive-quant/src/lib.rs:468-493`, `Sq8Codec::try_encode_flat_par`   | `(0..n).into_par_iter()` encodes flat rows                                                             | A sequential range iterator calling the same `encode_unchecked` body                                |
| `khive-quant/src/lib.rs:510-524`, `Sq8Codec::try_encode_par`        | `vectors.par_iter()` encodes row vectors                                                               | `vectors.iter()` calling the same `encode_unchecked` body                                           |
| `khive-quant/src/lib.rs:749-774`, `GsSq8Codec::try_encode_flat_par` | `(0..n).into_par_iter()` encodes flat rows for global-scale SQ8                                        | A sequential range iterator calling the same `encode_unchecked` body                                |

The infallible `Sq8Codec::encode_flat_par`, `Sq8Codec::encode_par`, and
`GsSq8Codec::encode_flat_par` wrappers delegate to those three fallible batch
encoders and remain available in both modes. In `index.rs:343-346`,
`train_codec_and_encode` consumes `GsSq8Codec::encode_flat_par`; it therefore
uses the same feature-selected parallel or sequential implementation. The
test-only use in `graph.rs` is covered by that same encoder gate.

### Source audit: native storage and file paths

`VectorStorage` is the storage type in `index.rs:269-301`. Its native `Mmap`
variant and every path-backed persistence surface are:

| Source site                                                                                                               | Native behavior                                                                                                | `mmap`-off or portable equivalent                                                                                 |
| ------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| `index.rs:269-301`, `VectorStorage::{Owned, Mmap}`                                                                        | Holds owned vectors or a read-only `memmap2::Mmap`                                                             | Compile only `Owned(Vec<f32>)`; byte loads validate and copy the vector payload into owned storage                |
| `index.rs:466-482`, `VamanaIndex::save` and `load`                                                                        | Write or read a segment directory; `load` accepts legacy v1 or committed v2 and mmaps `vectors.bin`            | Gate the path APIs behind `mmap`; `to_bytes` and `from_bytes` provide the non-filesystem v2 route                 |
| `index.rs:543-651`, `VamanaIndex::save_atomic`                                                                            | Stage and commit `vectors.bin`, `graph.bin`, `lifecycle.bin`, and `metadata.bin` by filesystem rename and sync | Gate the path API behind `mmap`; the portable encoder uses the same four segment payload encoders in memory       |
| `index.rs:659-845`, `VamanaIndex::load_or_build`                                                                          | Inspect a directory, validate checksums and fingerprint, mmap or rebuild, then save atomically                 | Gate the path API behind `mmap`; hosts use `from_bytes` or build explicitly when no portable container exists     |
| `index.rs:856-966`, `load_v2_raw` and `load_v2_fast`                                                                      | Read committed raw segments and mmap vectors                                                                   | Gate these path helpers; a byte loader validates the same payloads and constructs `VectorStorage::Owned`          |
| `index.rs:2047-2078`, `write_v2_commit_full`; `index.rs:2201-2240`, `write_lifecycle`                                     | Write native commit and lifecycle files                                                                        | Split payload encoding from the gated file writer; the byte encoders are shared with the portable container       |
| `index.rs:2422-2665`, `write_metadata`, `read_metadata`, `write_graph`, `read_graph`, `write_vectors`, and `mmap_vectors` | Encode, decode, write, read, and mmap native segment files                                                     | Keep byte codecs unconditional, gate `Path`, `File`, filesystem, and mmap wrappers, and load vectors into `Owned` |
| `index.rs:2682-2714`, `read_commit_fingerprint`                                                                           | Read the v2 fingerprint from `metadata.bin` in a directory                                                     | Gate the path helper; portable reads parse the identical `metadata.bin` payload from the container                |

`parse_v2_commit` and `parse_lifecycle` already operate on byte slices and stay
unconditional. The implementation extracts equivalent byte encoders for the
current writers rather than defining a second segment schema.

The production graph code has no OS-entropy call site. Every `StdRng` in
`graph.rs` is constructed with `seed_from_u64` and a fixed build seed. The wasm
work therefore configures `rand` without an OS-entropy requirement and does not
add a `getrandom` JavaScript entropy path. Tests continue to pass explicit
seeds.

`blake3` is already used with `default-features = false`. `bytemuck`, `serde`,
and `thiserror` require no wasm-specific boundary.

## Decision

The work has two layers. Layer A makes the existing Rust crates portable behind
default-on native features. Layer B adds the JavaScript product surface.

### Layer A: feature-gated khive-vamana and khive-quant

1. **`parallel`, default-on.** `rayon` becomes an optional dependency in both
   crates. The feature selects every parallel operation listed in the source
   audit. With the feature off, the same loop bodies execute sequentially in
   input order. Public quantizer batch method names remain available and become
   sequential internally. Seeded graph builds must produce the same graph and
   search results in both modes.
2. **`mmap`, default-on in khive-vamana.** `memmap2`, imports of `Path`, `File`,
   and filesystem operations, `VectorStorage::Mmap`, and every path API or
   helper listed above are gated together. With the feature off,
   `VectorStorage::Owned` is the only storage representation. Native default
   builds retain the current path API and mmap behavior.
3. **Portable bytes API, unconditional.**
   `VamanaIndex::to_bytes(&self, external_ids: &[(u32, String)])` encodes the
   portable container defined below.
   `VamanaIndex::from_bytes(&[u8])` returns the owned index plus its
   `Vec<(u32, String)>` live-ordinal mapping. They do not replace or alter the
   native directory format.
4. **Deterministic RNG.** Graph construction retains the current fixed seeded
   RNG seam. No OS-entropy API is introduced for wasm.
5. **CI.** Native CI continues with both default features. A wasm job runs
   `cargo check --target wasm32-unknown-unknown -p khive-quant -p khive-vamana --no-default-features`.
   A native job runs the Vamana and quantizer suites with
   `--no-default-features` to exercise every sequential and owned-storage path.

Default native consumers keep the current dependency set, public path surface,
and mmap behavior.

### Portable container and native v2 persistence

The existing native v2 persistence contract remains a directory of separately
committed files:

```text
metadata.bin
vectors.bin
graph.bin
lifecycle.bin
```

`metadata.bin` is the commit record. `vectors.bin`, `graph.bin`, and
`lifecycle.bin` retain their current payloads and commit/checksum semantics.
SQ8 codes are not a persisted segment. Native loading reconstructs the
`GsSq8Codec` and encoded SQ8 corpus from `vectors.bin` in
`index.rs:884-915` and `index.rs:965-966`; portable loading does the same.

`to_bytes` and `from_bytes` define a new versioned portable container. This is
framing around the same four v2 segment payloads, plus an optional
container-owned string-ID segment. It is not v3 of the native directory
protocol.

The container layout is:

1. Eight-byte magic `KHVVAMAC`.
2. Little-endian `u32` container version, initially `1`.
3. Little-endian `u32` segment count.
4. A segment table with exactly `segment_count` entries. Each entry contains,
   in order, a little-endian `u32` name length, that many raw UTF-8 name bytes,
   a little-endian `u64` absolute payload offset, a little-endian `u64` payload
   length, and 32 raw bytes containing the blake3 checksum of that payload. The
   table ends exactly after the final entry.
5. Non-overlapping payloads named `metadata.bin`, `vectors.bin`, `graph.bin`,
   and `lifecycle.bin`, plus optional `portable_ids.bin`.

Offsets are authoritative. Readers locate payloads only through the segment
table and do not infer payload positions from table order, adjacency, or the
end of the preceding payload. Writers leave no gaps, but readers do not assume
contiguity.

Unknown container versions, missing or duplicate required core segments,
duplicate names, overlapping or out-of-bounds ranges, checksum failures, and
malformed segment payloads are errors. The existing fingerprint validation
applies after framing and checksum validation.

`portable_ids.bin` is itself versioned. Its payload contains, in order, the
eight-byte magic `KHVEXTID`, a little-endian `u32` version, initially `1`, and a
little-endian `u64` live-entry count. Entries follow sorted by ordinal. Each
entry contains a little-endian `u32` ordinal, a little-endian `u32` UTF-8 byte
length, and that many raw UTF-8 bytes for one non-empty string ID. There is
exactly one ID for every live ordinal and none for a tombstoned ordinal.
Duplicate strings, duplicate ordinals, invalid UTF-8, out-of-range ordinals,
or live-set mismatch are errors. Deserialization reconstructs both
ordinal-to-ID and ID-to-ordinal maps from this segment. When the optional
segment is absent, the wasm binding constructs decimal ordinal strings (`"0"`,
`"1"`, and so on) for the live ordinals.

The native directory writer, atomic commit order, and mmap load path are
unchanged. A pure reframing adapter maps the four required payload files and
optional `portable_ids.bin` to or from a portable container without decoding
or rewriting any payload. Container-to-directory writes the string-ID segment
only as `portable_ids.bin`, never as `external_ids.bin`. The native directory
loader in `crates/khive-vamana/src/index.rs` opens only the four named core
segment paths and does not enumerate the directory, so it ignores the unknown
extra `portable_ids.bin` file. Directory interchange therefore covers the four
core Vamana segments plus optional `portable_ids.bin`. The native runtime
ignores that optional sidecar; only the wasm layer, or a future amended
knowledge bridge, consumes it.

ADR-079 says there is no change to the `khive-vamana` public surface. ADR-110
amends that statement only for the additive `to_bytes` and `from_bytes` API and
the new native-loader-ignored `portable_ids.bin` sidecar filename. ADR-079's
knowledge bridge owns the existing `external_ids.bin` contract, whose
`KHVANIDS` payload binds UUIDs to the v2 commit content hash. ADR-110 neither
reuses nor modifies that filename or format. Generic string-ID interoperability
with the knowledge bridge is out of scope for v0 and requires a future joint
amendment. ADR-079's native directory persistence, mmap loading, and remaining
integration contract otherwise remain unchanged.

### Layer B: khive-vamana-wasm bindings crate

A new `crates/khive-vamana-wasm` crate uses `wasm-bindgen`, emits a `cdylib`, is
excluded from the default workspace build, and is packaged for npm.

The v0 JavaScript contract is:

- `build(vectors: Float32Array, dim, config, ids?)` accepts an optional array of
  string IDs. Its length must equal the vector count, every ID must be non-empty
  and unique, and omitted IDs default to decimal ordinal strings (`"0"`, `"1"`,
  and so on). The `config` argument is a closed object with three optional
  keys, `maxDegree`, `searchListSize`, and `alpha`, mapping to the
  `VamanaConfig` fields `max_degree`, `search_list_size`, and `alpha`;
  `dimensions` comes from the `dim` argument. An omitted key takes the core
  default. An unknown key, a non-positive `maxDegree`, a `searchListSize`
  below `maxDegree`, or a non-finite or sub-1.0 `alpha` is rejected before
  any index state is constructed.
- All numeric arguments crossing the JavaScript boundary (`dim`, `k`,
  `maxDegree`, `searchListSize`) must be finite, integral safe integers
  (`Number.isSafeInteger`). The binding rejects fractional, negative,
  non-finite, and out-of-safe-range values before any conversion to `usize`;
  no implicit truncation or rounding occurs. `dim` must be positive.
- `search(query: Float32Array, k)` returns `{ ids, distances }`, where `ids` is
  a JavaScript array of string IDs and `distances` is a parallel
  `Float32Array` of f32 distances. Both arrays have the same length and index
  order. Tombstoned ordinals are never returned. `k` must be a positive safe
  integer; `k` of zero is rejected. A `k` larger than the live count returns
  all live results.
- `insert(id, vector)` rejects an empty or duplicate string ID. It performs the
  core insert and updates both ID maps as one operation. If the core recycles a
  tombstoned ordinal, the new mapping is installed for that ordinal in the same
  operation. Any failure leaves the index and both maps unchanged.
- `remove(id)` resolves the string through the ID-to-ordinal map, tombstones
  that ordinal through the core delete path, and removes both map entries as
  one operation. An unknown or already removed ID is an error.
- `serialize(): Uint8Array` emits the portable container, including the
  portable string-ID segment for all live ordinals.
- `deserialize(bytes: Uint8Array)` validates the complete container, restores
  owned vector storage and lifecycle state, and reconstructs SQ8. When the
  container carries a `portable_ids.bin` segment, both ID maps are restored and
  search and remove use the original string IDs. When it does not (for example,
  a container reframed from a native v2 directory), `deserialize` creates the
  documented default maps of decimal live-ordinal strings instead.

v0 is SQ8-only. `build` always trains SQ8, matching the native
`VamanaIndex::build` path, and no acquisition-mode selector exists in the
JavaScript config or in either persistence format. A full-precision mode is
deferred to a future amendment, which must specify its configuration shape,
its core-state representation, and how both portable and native persistence
preserve the mode across `serialize` and `deserialize`. Persistence is the
host's responsibility through OPFS, IndexedDB, HTTP, or another byte store.
An OPFS round-trip example ships in the crate README.

The v0 module is single-threaded. Wasm threads require cross-origin isolation
and shared memory and are deferred.

### Layer B size gate

The normative release budget is a gzipped wasm module size of at most 500 KB,
defined as 500,000 bytes. The Layer B CI job runs `wasm-bindgen` on the release
artifact, measures `khive_vamana_wasm_bg.wasm` with `gzip -9 -c`, and fails when
the byte count exceeds 500,000. The crate README may repeat this limit, but this
ADR is the source of truth.

### Non-goals for v0

- Browser embedding models. Query vectors arrive from the host.
- Porting the khive storage layer, packs, or MCP surface.
- A selectable full-precision (non-SQ8) acquisition mode.
- Explicit wasm SIMD128 kernels before measurements justify them.
- WebGPU.
- Wasm threads.

## Consequences

- `khive-vamana` gains default-on `parallel` and `mmap` features plus an
  unconditional portable bytes API. `khive-quant` gains a default-on
  `parallel` feature.
- Sequential graph construction, search, and batch quantization become tested
  configurations.
- The portable container becomes a compatibility surface. Its version and
  per-segment checksums allow future readers to reject unsupported layouts.
- Native v2 persistence remains a crash-safe segment directory and keeps its
  zero-copy mmap load path.
- SQ8 remains derived state in both formats, so persisted bytes do not duplicate
  the quantized corpus.
- The npm artifact adds a release step and versions in lockstep with the Rust
  crates.

## Acceptance gates

| Gate                    | Assertion                                                                                                                                                                                                                      |
| ----------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| wasm check              | `cargo check --target wasm32-unknown-unknown -p khive-quant -p khive-vamana --no-default-features` passes in CI                                                                                                                |
| Sequential coverage     | Native Vamana and quantizer tests pass with `--no-default-features`, exercising every fallback in the source-audit table                                                                                                       |
| Build parity            | A fixed-seed corpus produces identical graph and search results with default features and with `--no-default-features`                                                                                                         |
| Native bytes round-trip | `to_bytes` followed by `from_bytes` preserves lifecycle state and search results exactly                                                                                                                                       |
| Native-to-wasm fixture  | A container reframed from a native v2 directory with no ID segment loads in wasm and returns default decimal-ordinal-string IDs and the fixture's distances                                                                    |
| Wasm-to-native fixture  | A wasm-produced container reframes to a native v2 directory, the unchanged native loader reproduces the fixture's search results, and a second wasm load round-trips the ID segment losslessly through `portable_ids.bin`      |
| Corruption rejection    | Bad magic or version, truncation, overlapping ranges, missing or duplicate segments, bad checksums, and malformed portable string IDs are rejected                                                                             |
| SQ8 reconstruction      | Neither native nor portable persisted output contains an SQ8 segment, and both load paths reconstruct SQ8 with search parity                                                                                                   |
| Insert by string ID     | Inserting a unique string ID makes it searchable; duplicate insertion errors without changing index or map state; recycled ordinals map only to the new ID                                                                     |
| Remove by string ID     | Removing a string ID resolves and tombstones its ordinal, removes both mappings, and prevents that ID from appearing in search                                                                                                 |
| Serialize string IDs    | `serialize` emits one versioned portable string-ID entry per live ordinal and none for tombstones                                                                                                                              |
| Deserialize string IDs  | `deserialize` restores both ID maps so remove-by-ID and search-by-ID work without rebuilding                                                                                                                                   |
| Search by string ID     | Search returns equal-length parallel arrays of string IDs and f32 distances in result order, including after deserialize                                                                                                       |
| Config validation       | `build` rejects an unknown config key, a non-positive `maxDegree`, a `searchListSize` below `maxDegree`, and a non-finite or sub-1.0 `alpha`, each without constructing any index state                                        |
| Numeric boundary        | Fractional, negative, non-finite, and non-safe-integer values for `dim`, `k`, `maxDegree`, and `searchListSize` are rejected before conversion; `k` of zero is rejected; browser tests cover each case including `dim` and `k` |
| Browser smoke           | A headless browser test covers build with explicit and default IDs, search, insert, remove, serialize, and deserialize with SQ8 enabled                                                                                        |
| Size                    | Layer B CI asserts `gzip -9` of the wasm-bindgen release `khive_vamana_wasm_bg.wasm` artifact is at most 500,000 bytes                                                                                                         |

## Rollout

1. Layer A lands first with feature gates, portable byte codecs, the reframing
   adapter, native/wasm fixtures, and CI checks.
2. Layer B lands second with bindings, npm packaging, browser tests, and the
   release size gate.
3. A local-first demo consumer follows separately and is not an acceptance
   requirement for this ADR.

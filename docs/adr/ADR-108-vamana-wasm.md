# ADR-108: WebAssembly Support for khive-vamana

- Status: Proposed
- Date: 2026-07-12
- Deciders: khive maintainers

## Context

khive-vamana is the workspace's Vamana/DiskANN-family ANN index: two-tier SQ8
quantized-steering search (ADR-052), incremental insert, two-hop delete repair,
and fingerprint-gated persistence (ADR-079). It currently builds only for native
targets. Making the index run in a browser opens a class of deployments the
substrate cannot serve today: local-first graph tooling that searches an
exported knowledge base entirely client-side, offline demos of the retrieval
stack, and edge runtimes (workers) where a native binary cannot ship.

The SQ8 acquisition tier is what makes a browser target credible rather than a
stunt. Browser tabs budget memory aggressively; a 4x-compressed steering tier
with exact re-rank on a small candidate set is the difference between "fits a
useful corpus" and "toy demo". The two-tier design also degrades gracefully:
full-precision-only indexes work the same way, just larger.

An audit of the crate's dependency surface found four blockers for
`wasm32-unknown-unknown`, all peripheral to the core algorithms:

| Blocker           | Where                                               | Nature                                                                                      |
| ----------------- | --------------------------------------------------- | ------------------------------------------------------------------------------------------- |
| `rayon`           | `graph.rs` build passes; `khive-quant` `encode_par` | Parallelism convenience; every call site has a natural sequential form                      |
| `memmap2`         | `index.rs` `VectorSlab::Mmap` variant               | Native persistence optimization; the owned-`Vec` slab representation is already first-class |
| `rand` OS entropy | `graph.rs` RNG construction                         | `SeedableRng` is already in use; only the entropy source is platform-bound                  |
| File-path I/O     | `save_atomic` / `load_or_build`                     | The browser has no filesystem; persistence must round-trip through bytes                    |

`khive-quant` is otherwise pure computation. `blake3` is already consumed with
`default-features = false` and compiles for wasm. `bytemuck`, `serde`, and
`thiserror` are target-agnostic.

## Decision

Two layers. Layer A makes the existing crates compile and behave correctly on
`wasm32-unknown-unknown` behind feature gates with zero native behavior change.
Layer B adds a new bindings crate exposing a JavaScript API. Layer A is useful
without Layer B (any downstream wasm consumer can embed the Rust API directly);
Layer B is the product surface.

### Layer A: feature-gated portability in khive-vamana and khive-quant

1. **`parallel` feature, default-on.** Gates the `rayon` dependency and every
   `par_iter` call site in `khive-vamana` (graph build passes) and `khive-quant`
   (`encode_par`, `encode_batch`). Each gated site gets a sequential fallback
   compiled when the feature is off. The fallback is the same loop body in
   deterministic sequential order; build results must be identical between the
   two modes given the same seed (the build is already seeded and
   deterministic per ADR-052 testing).
2. **`mmap` feature, default-on.** Gates `memmap2` and the `VectorSlab::Mmap`
   variant plus the file-path `save_atomic` / `load_or_build` surface. With the
   feature off, the owned-`Vec` slab is the only representation.
3. **Bytes persistence API, unconditional.** `to_bytes()` and
   `from_bytes(&[u8])` on the index, producing the same serialized layout the
   file path uses (header, fingerprint, slab, adjacency, quantized tier), so a
   native writer and a wasm reader interoperate. File-path save/load becomes a
   thin wrapper over the bytes API under the `mmap` feature. Fingerprint
   validation applies on both paths.
4. **Entropy.** RNG construction goes through one seam: seeded explicitly by
   the caller, or from OS entropy where available. On wasm, `getrandom`'s
   `js` feature provides entropy; deterministic builds remain available by
   passing a seed. No API removal.
5. **CI gate.** The workspace CI adds
   `cargo check --target wasm32-unknown-unknown -p khive-quant -p khive-vamana --no-default-features`
   so wasm cleanliness cannot regress silently. Native CI continues to run with
   default features; a test job runs the vamana suite with `--no-default-features`
   to keep the sequential fallbacks honest.

Native consumers see no change: default features reproduce today's exact
dependency set and behavior.

### Layer B: khive-vamana-wasm bindings crate

A new crate `crates/khive-vamana-wasm` (`cdylib`, `wasm-bindgen`), not a member
of the default workspace build, packaged for npm.

- **API surface (v0):** `build(vectors: Float32Array, dim, config)`,
  `search(query: Float32Array, k)`, `insert(id, vector)`, `remove(id)`,
  `serialize(): Uint8Array`, `deserialize(bytes: Uint8Array)`. IDs are strings
  on the JS boundary; distances come back as a parallel `Float32Array`.
- **SQ8 first.** The wasm config defaults the SQ8 acquisition tier on, because
  memory is the binding constraint in a tab. Full-precision mode remains
  selectable.
- **Persistence is the host's job.** The crate exposes bytes; the JS host
  decides where they live (OPFS, IndexedDB, HTTP fetch). No storage backend is
  compiled into the wasm module. An OPFS round-trip example ships in the
  package README.
- **Threading: none in v0.** The module is single-threaded (sequential
  fallbacks from Layer A). Wasm threads require cross-origin isolation and a
  shared-memory build; that is a possible v1, not assumed.

### Non-goals (v0)

- **Embedding in the browser.** Query vectors arrive from the host. Local
  embedding models are a separate concern with their own wasm story.
- **Wholesale khive in the browser.** This ADR ports the ANN index, not the
  storage layer, packs, or MCP surface.
- **SIMD128 kernels.** The distance kernels are written for auto-vectorization
  (8-wide chunked loops); measured shortfalls on wasm can motivate explicit
  `core::arch::wasm32` kernels later, behind a feature, with numbers first
  (per the workspace's measure-before-optimizing rule).
- **WebGPU.**

## Consequences

- khive-vamana and khive-quant gain two features each and a bytes API; native
  behavior and defaults are unchanged. Downstream crates need no edits.
- The sequential build path becomes a supported, tested configuration, which
  also benefits constrained native environments (single-core CI runners).
- A new npm artifact means a release-process addition: the wasm package
  versions in lockstep with the crates and publishes from CI on release tags.
  Publishing setup is part of Layer B, not a follow-up.
- The bytes format becomes a compatibility surface between native and wasm.
  The existing fingerprint header already versions the layout; format changes
  keep working through it.

## Acceptance gates

| Gate             | Assertion                                                                                                                              |
| ---------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| wasm check       | `cargo check --target wasm32-unknown-unknown -p khive-quant -p khive-vamana --no-default-features` passes in CI                        |
| Parity           | Seeded build + search with `--no-default-features` produces identical results to the parallel default on the same input                |
| Bytes round-trip | `to_bytes` → `from_bytes` reproduces search results exactly; a file saved by the native path loads via `from_bytes`                    |
| Fingerprint      | Corrupted or truncated bytes are rejected at `from_bytes`, never silently accepted                                                     |
| Browser smoke    | Layer B: build, search, serialize, deserialize, and insert-then-search run under a headless browser test on a small corpus with SQ8 on |
| Size             | Layer B: released wasm module (gzipped) stays under an agreed budget recorded in the crate README                                      |

## Rollout

1. Layer A lands first as one PR (feature gates, bytes API, CI gate). It is
   independently reviewable and carries no new crate.
2. Layer B lands second (bindings crate, npm packaging, browser smoke test).
3. A demo consumer (local-first search over an exported KG) follows separately
   and is not part of this ADR's acceptance.

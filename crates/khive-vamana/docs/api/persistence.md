# Vamana Persistence

**Scope:** `crates/khive-vamana/src/index.rs` — `VamanaIndex::save`, `VamanaIndex::load`, `VamanaIndex::to_snapshot`, `VamanaIndex::from_snapshot`
**ADR refs:** ADR-048 (persistence and snapshot validation requirements)
**Last reviewed:** 2026-06-06

---

## Binary file format (`save` / `load`)

`VamanaIndex::save` writes three files into a caller-supplied directory:

### `metadata.bin`

| Offset | Size | Type   | Field              |
| ------ | ---- | ------ | ------------------ |
| 0      | 8    | bytes  | magic `KHVVAMM1`   |
| 8      | 8    | LE u64 | `num_vectors`      |
| 16     | 8    | LE u64 | `dimensions`       |
| 24     | 8    | LE u64 | `max_degree`       |
| 32     | 8    | LE u64 | `search_list_size` |
| 40     | 8    | LE f64 | `alpha`            |

Total: 48 bytes.

### `graph.bin`

| Offset | Size   | Type             | Field                                       |
| ------ | ------ | ---------------- | ------------------------------------------- |
| 0      | 8      | bytes            | magic `KHVVAMG1`                            |
| 8      | 4      | LE u32           | `num_nodes`                                 |
| 12     | 4      | LE u32           | `medoid`                                    |
| 16     | varies | per-node records | degree (4 bytes) + neighbors (4 bytes each) |

Each node record: `degree: u32` followed by `degree` neighbor IDs as `u32`.

### `vectors.bin`

Raw `f32` values in little-endian IEEE 754 format, row-major:
`num_vectors × dimensions × 4` bytes. Loaded via `memmap2` as a read-only mapping.

---

## Snapshot format (`to_snapshot` / `from_snapshot`)

`VamanaSnapshot` is a self-validating JSON-serializable struct stored in the
`retrieval_snapshots` SQLite BLOB via `khive-runtime`. Fields:

- `format`: must equal `"khive-vamana-index"`
- `version`: must equal `1`
- `namespace`, `model`: routing metadata
- `fingerprint`: `CorpusFingerprint { vector_count, dimensions }` — compared against
  the live embedding store before installation; a mismatch causes silent rebuild
- `index`: `VamanaIndexSnapshot` containing all graph and vector data
- `external_ids`: `Vec<String>` mapping node IDs back to external UUID strings

---

## Validation on load / restore

Both `load` and `from_snapshot` validate:

- Magic bytes and version fields
- `num_vectors > 0`, `dimensions > 0`
- `medoid < num_vectors`
- `adjacency.len() == num_vectors`
- Each neighbor `nb < num_vectors` and `nb != node` (no self-loops)
- No duplicate neighbors per node
- `vectors.len() == num_vectors * dimensions`
- All vector values are finite (no `NaN` or `Infinity`)
- File size matches expected byte count (binary path only)
- No trailing bytes in `graph.bin`

---

## Adjacency representation trade-off

The current `Vec<Vec<u32>>` layout is correct and supports the build-phase
pruning pattern (frequent ownership-taking and re-insertion). Known costs:

- Allocation count: `num_vectors + total_edges` heap objects
- Clone cost: proportional to total edge count (used in batch build snapshots)
- Serialization: `serde_json` nests arrays without flattening

A CSR flat layout (`offsets: Vec<u32>`, `neighbors: Vec<u32>`) would reduce
allocation count to 2, enable `mmap` for the graph itself, and cut serialization
size by roughly one-third for large corpora. Migration is tracked as a
future improvement triggered when `N > 1M` or when mmap-graph is needed for
streaming index updates. No ADR change is required for this internal layout;
a single `VamanaGraph` refactor suffices.

---

## v2 crash-safe save/load

`VamanaIndex::save_atomic` writes `vectors.bin` and `graph.bin` (same formats as v1),
then `lifecycle.bin` (tombstones, free_slots, reverse_adj, ops_since_consolidation),
then atomically renames `metadata.bin.tmp` → `metadata.bin` as the commit record. If a
crash interrupts before the rename, the previous `metadata.bin` (v1 or v2) is still
valid — `load_or_build` never observes a torn v2 commit. Segments are staged under
`.v2new` suffixes so a crash between segment write and metadata rename never corrupts
a live v1-format segment set. The directory entry is fsynced after both the metadata
rename (commit gate) and the final segment-promotion renames.

`VamanaIndex::load_or_build` is the fingerprint-gated restore used by callers holding a
live corpus. Decision tree:
- `metadata.bin` with `KHVVAMG2` magic AND checksums valid AND fingerprint matches →
  fast path (`load_v2_fast`, O(N) — no reverse_adj rebuild)
- `KHVVAMG2` but checksum or fingerprint mismatch → rebuild from commit config, then
  `save_atomic`
- `metadata.bin` with `KHVVAMM1` (v1) → v1 `load`, then `save_atomic` upgrade
- `metadata.bin` missing or corrupt → rebuild from `fallback_config`, then `save_atomic`

`VamanaIndex::load_v2_raw` (private) is the non-rebuilding half of that fast path: it
verifies the v2 commit magic, parses the commit record, checksum-verifies all three
segments against the commit fingerprint, then restores via `load_v2_fast`. It returns
`InvalidFormat` if `path` holds no valid v2 commit (absent, v1-format, torn, checksum
mismatch, or an inconsistent lifecycle segment) rather than rebuilding — `load_or_build`
is the entry point that decides whether to fall back to a rebuild; `VamanaIndex::load`
surfaces the error directly to callers that don't hold a corpus to rebuild from.

## lifecycle.bin format

Written by `write_lifecycle` (`index.rs`) as part of the v2 segmented save. All
fields little-endian:

| Field              | Size                    | Type            |
| ------------------ | ----------------------- | --------------- |
| magic               | 8 B                     | bytes `KHVVLIF1` |
| tombstone_words     | 8 B                     | u64 (count of u64 words) |
| tombstone data      | N × 8 B                 | u64 words        |
| free_slots_count    | 8 B                     | u64              |
| free_slots data     | M × 4 B                 | u32 each         |
| ops                 | 8 B                     | u64              |
| rev_num_nodes       | 8 B                     | u64              |
| reverse_adj records | varies, one per node    | degree (u32) + neighbors (degree × u32) |

`reverse_adj` uses the same per-node record format as `graph.bin`'s adjacency
(degree followed by that many neighbor IDs). `num_nodes` for the tombstone bitvec is
derived from `tombstone_words`; the caller already knows `num_vectors` from
`metadata.bin`.

## v2 fast-load path

`VamanaGraph::restore_reverse_adj` installs a previously serialized in-neighbor list
as `reverse_adj` in O(1) (a move). This is safe to call without redoing the O(N×R)
rebuild because the v2 fast-load path (`load_v2_fast` in `index.rs`) has already paid
that cost validating bidirectional consistency between the loaded `reverse_adj` and
the forward `adjacency` before calling it — the O(N×R) work isn't skipped, it just
happens in the validation step rather than in `restore_reverse_adj` itself.

---

## Safety note (mmap)

The single `unsafe` block in `mmap_vectors` maps `vectors.bin` read-only.
The contract: callers must not mutate or truncate the file while the index
is live. `kkernel` deletes snapshot files atomically before replacing them,
so a live index always holds a mapping to a consistent file version.

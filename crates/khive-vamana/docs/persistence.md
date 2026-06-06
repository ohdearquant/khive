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

## Safety note (mmap)

The single `unsafe` block in `mmap_vectors` maps `vectors.bin` read-only.
The contract: callers must not mutate or truncate the file while the index
is live. `kkernel` deletes snapshot files atomically before replacing them,
so a live index always holds a mapping to a consistent file version.

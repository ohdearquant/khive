# khive-pack-kg Benchmark Ledger

## Run command

```bash
cd crates && cargo bench -p khive-pack-kg --bench kg_bench
```

Dry-run (compile + single iteration, no timing):

```bash
cd crates && cargo bench -p khive-pack-kg --bench kg_bench -- --test
```

## Scenarios

| Group          | Scenario                      | Corpus           | What is measured                    |
| -------------- | ----------------------------- | ---------------- | ----------------------------------- |
| `kg_create`    | `entity`                      | —                | Single entity create (concept kind) |
| `kg_create`    | `note`                        | —                | Single observation note create      |
| `kg_get`       | `by_uuid`                     | 1 entity         | UUID lookup (read hot path)         |
| `kg_list`      | `entity_kind_concept_limit20` | 200 entities     | Paginated list with kind filter     |
| `kg_list`      | `entity_all_limit50`          | 200 entities     | Paginated list no kind filter       |
| `kg_search`    | `entity_fts/100`              | 100 entities     | FTS search over 100-entity corpus   |
| `kg_search`    | `entity_fts/500`              | 500 entities     | FTS search over 500-entity corpus   |
| `kg_search`    | `entity_fts/1000`             | 1 000 entities   | FTS search over 1 000-entity corpus |
| `kg_link`      | `single_edge`                 | 2 entities       | Edge upsert (idempotent re-upsert)  |
| `kg_neighbors` | `hub_100_out`                 | hub + 100 leaves | Outgoing neighbors on 100-edge hub  |
| `kg_traverse`  | `chain_depth/1`               | 10-node chain    | BFS depth 1                         |
| `kg_traverse`  | `chain_depth/2`               | 100-node chain   | BFS depth 2                         |
| `kg_traverse`  | `chain_depth/3`               | 100-node chain   | BFS depth 3                         |

## Release Ledger

### v0.2.8 - 2026-06-08

- Commit: `d3629501c550fd2f3bb7ed350a2b60309d596465`
- Crate version: `0.2.8`
- Khive version: `0.2.8`
- Toolchain: `rustc 1.94.1 (e408947bf 2026-03-25)`, release profile (Criterion)
- Machine: Apple M-series arm64, macOS Darwin 25.5.0, 16 GB
- Feature flags: default
- Command: `cd crates && cargo bench -p khive-pack-kg --bench kg_bench`
- Dataset: in-memory SQLite; entity / note / edge fixtures generated at bench setup; corpus sizes
  per scenario table above
- vs prior: first formal release ledger entry — no prior comparable baseline

#### Create


| Scenario         | Low      | Median   | High     | Outliers  |
| ---------------- | -------- | -------- | -------- | --------- |
| kg_create/entity | 6.311 ms | 6.798 ms | 7.236 ms | 2/50 (4%) |
| kg_create/note   | 2.238 ms | 2.508 ms | 2.794 ms | 1/50 (2%) |

#### Get / List

| Scenario                            | Low      | Median   | High     | Outliers   |
| ----------------------------------- | -------- | -------- | -------- | ---------- |
| kg_get/by_uuid                      | 70.85 µs | 81.20 µs | 94.17 µs | 9/100 (9%) |
| kg_list/entity_kind_concept_limit20 | 183.1 µs | 202.5 µs | 229.7 µs | 3/50 (6%)  |
| kg_list/entity_all_limit50          | 254.0 µs | 279.0 µs | 314.8 µs | 6/50 (12%) |

#### Search (FTS)

| Scenario                  | Low      | Median   | High     | Outliers   |
| ------------------------- | -------- | -------- | -------- | ---------- |
| kg_search/entity_fts/100  | 1.608 ms | 1.739 ms | 1.919 ms | 6/30 (20%) |
| kg_search/entity_fts/500  | 3.427 ms | 3.818 ms | 4.398 ms | 5/30 (17%) |
| kg_search/entity_fts/1000 | 5.879 ms | 6.289 ms | 6.937 ms | 4/30 (13%) |

#### Link / Graph

| Scenario                  | Low      | Median   | High     | Outliers   |
| ------------------------- | -------- | -------- | -------- | ---------- |
| kg_link/single_edge       | 164.8 µs | 192.8 µs | 220.9 µs | 7/50 (14%) |
| kg_neighbors/hub_100_out  | 2.406 ms | 2.931 ms | 3.406 ms | 5/50 (10%) |
| kg_traverse/chain_depth/1 | 242.4 µs | 291.3 µs | 349.5 µs | 4/30 (13%) |
| kg_traverse/chain_depth/2 | 339.2 µs | 369.1 µs | 405.7 µs | 2/30 (7%)  |
| kg_traverse/chain_depth/3 | 556.5 µs | 652.3 µs | 771.9 µs | —          |

- Notes: none

Last reviewed: v0.2.8 (2026-06-08)

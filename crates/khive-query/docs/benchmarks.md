# khive-query Benchmark Ledger

<!-- bench-reference:begin -->

Generated from `perf/reference.json` by `scripts/render-bench-reference.py`; do not edit this block.

Reference baselines for `khive-query`. Values are the committed reference median for each benchmark, in the unit shown; A/A is the ratio between the two same-commit runs. The date is the committer date of the baseline commit.

| Benchmark                                                                                                       | Reference median | A/A ratio | Baseline commit                            | Commit date |
| --------------------------------------------------------------------------------------------------------------- | ---------------- | --------- | ------------------------------------------ | ----------- |
| `khive-query:parse_bench/gql/gql/node_with_limit/median_ns`                                                     | 449.837 ns       | 1.010     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:parse_bench/gql/gql/simple_node/median_ns`                                                         | 415.598 ns       | 1.008     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:parse_bench/gql/gql/two_node_edge/median_ns`                                                       | 709.910 ns       | 1.023     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:parse_bench/gql_complex/gql/node_multi_property_map/median_ns`                                     | 932.250 ns       | 1.022     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:parse_bench/gql_complex/gql/three_node_chain/median_ns`                                            | 1.027 µs         | 1.015     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:parse_bench/gql_complex/gql/three_node_mixed_direction/median_ns`                                  | 1.110 µs         | 1.019     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:parse_bench/gql_complex/gql/undirected_edge/median_ns`                                             | 724.750 ns       | 1.012     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:parse_bench/gql_complex/gql/variable_length_multi_rel/median_ns`                                   | 892.228 ns       | 1.035     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:parse_bench/gql_complex/gql/variable_length_with_where/median_ns`                                  | 1.011 µs         | 1.009     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:parse_bench/gql_medium/gql/node_with_properties/median_ns`                                         | 644.221 ns       | 1.003     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:parse_bench/gql_medium/gql/where_and/median_ns`                                                    | 1.112 µs         | 1.010     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:parse_bench/gql_medium/gql/where_and_or/median_ns`                                                 | 1.266 µs         | 1.017     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:parse_bench/gql_medium/gql/where_eq_string/median_ns`                                              | 998.115 ns       | 1.012     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:parse_bench/gql_medium/gql/where_or/median_ns`                                                     | 1.042 µs         | 1.009     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:parse_bench/gql_medium/gql/where_with_edge_var/median_ns`                                          | 980.276 ns       | 1.009     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:parse_bench/parse_auto/auto/gql_dispatch/median_ns`                                                | 741.229 ns       | 1.019     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:parse_bench/parse_auto/auto/gql_with_leading_whitespace/median_ns`                                 | 511.780 ns       | 1.002     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:parse_bench/parse_auto/auto/sparql_dispatch/median_ns`                                             | 1.364 µs         | 1.005     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:parse_bench/parse_auto/auto/sparql_with_leading_whitespace/median_ns`                              | 1.079 µs         | 1.011     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:parse_bench/sparql/sparql/explicit_range/median_ns`                                                | 1.099 µs         | 1.006     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:parse_bench/sparql/sparql/two_node/median_ns`                                                      | 1.392 µs         | 1.010     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:parse_bench/sparql/sparql/variable_length_plus/median_ns`                                          | 1.422 µs         | 1.010     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:parse_bench/sparql_medium/sparql/kind_and_property_filter/median_ns`                               | 1.821 µs         | 1.002     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:parse_bench/sparql_medium/sparql/three_node_chain/median_ns`                                       | 1.815 µs         | 1.032     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:parse_bench/sparql_medium/sparql/with_property_filter/median_ns`                                   | 1.760 µs         | 1.033     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/compile/complex/node_multi_property_map/median_ns`                   | 6.390 µs         | 1.015     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/compile/complex/three_node_chain/median_ns`                          | 9.914 µs         | 1.023     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/compile/complex/three_node_mixed_direction/median_ns`                | 12.262 µs        | 1.014     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/compile/complex/undirected_edge/median_ns`                           | 8.380 µs         | 1.022     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/compile/complex/variable_length_multi_rel/median_ns`                 | 5.292 µs         | 1.107     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/compile/complex/variable_length_with_where/median_ns`                | 5.355 µs         | 1.082     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/compile/medium/node_with_properties/median_ns`                       | 3.477 µs         | 1.032     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/compile/medium/where_and/median_ns`                                  | 8.611 µs         | 1.011     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/compile/medium/where_and_or/median_ns`                               | 7.276 µs         | 1.016     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/compile/medium/where_eq_string/median_ns`                            | 6.487 µs         | 1.039     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/compile/medium/where_or/median_ns`                                   | 6.678 µs         | 1.038     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/compile/medium/where_with_edge_var/median_ns`                        | 9.436 µs         | 1.064     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/compile/simple/node_with_limit/median_ns`                            | 3.493 µs         | 1.003     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/compile/simple/simple_node/median_ns`                                | 3.419 µs         | 1.036     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/compile/simple/two_node_edge/median_ns`                              | 8.173 µs         | 1.017     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/large/parse_10_nodes_20_properties/median_ns`                        | 7.125 µs         | 1.018     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/large/parse_compile_10_nodes_20_properties/median_ns`                | 40.670 µs        | 1.005     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/sparql_complex/four_edges_six_triples_bounded_paths/median_ns`       | 3.625 µs         | 1.024     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate/complex/node_multi_property_map/median_ns`                  | 188.648 ns       | 1.007     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate/complex/three_node_chain/median_ns`                         | 259.413 ns       | 1.007     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate/complex/three_node_mixed_direction/median_ns`               | 420.954 ns       | 1.008     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate/complex/undirected_edge/median_ns`                          | 266.319 ns       | 1.014     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate/complex/variable_length_multi_rel/median_ns`                | 242.553 ns       | 1.020     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate/complex/variable_length_with_where/median_ns`               | 191.998 ns       | 1.012     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate/medium/node_with_properties/median_ns`                      | 110.181 ns       | 1.018     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate/medium/where_and/median_ns`                                 | 291.484 ns       | 1.002     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate/medium/where_and_or/median_ns`                              | 312.341 ns       | 1.002     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate/medium/where_eq_string/median_ns`                           | 269.070 ns       | 1.008     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate/medium/where_or/median_ns`                                  | 292.109 ns       | 1.003     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate/medium/where_with_edge_var/median_ns`                       | 278.475 ns       | 1.006     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate/simple/node_with_limit/median_ns`                           | 110.082 ns       | 1.021     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate/simple/simple_node/median_ns`                               | 109.179 ns       | 1.005     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate/simple/two_node_edge/median_ns`                             | 257.092 ns       | 1.018     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate_with_warnings/complex/node_multi_property_map/median_ns`    | 197.801 ns       | 1.130     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate_with_warnings/complex/three_node_chain/median_ns`           | 260.353 ns       | 1.009     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate_with_warnings/complex/three_node_mixed_direction/median_ns` | 431.376 ns       | 1.067     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate_with_warnings/complex/undirected_edge/median_ns`            | 271.429 ns       | 1.109     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate_with_warnings/complex/variable_length_multi_rel/median_ns`  | 236.320 ns       | 1.080     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate_with_warnings/complex/variable_length_with_where/median_ns` | 200.407 ns       | 1.109     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate_with_warnings/medium/node_with_properties/median_ns`        | 112.445 ns       | 1.026     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate_with_warnings/medium/where_and/median_ns`                   | 285.833 ns       | 1.000     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate_with_warnings/medium/where_and_or/median_ns`                | 314.855 ns       | 1.052     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate_with_warnings/medium/where_eq_string/median_ns`             | 267.086 ns       | 1.003     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate_with_warnings/medium/where_or/median_ns`                    | 286.595 ns       | 1.002     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate_with_warnings/medium/where_with_edge_var/median_ns`         | 273.819 ns       | 1.006     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate_with_warnings/simple/node_with_limit/median_ns`             | 109.600 ns       | 1.015     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate_with_warnings/simple/simple_node/median_ns`                 | 110.279 ns       | 1.016     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-query:pipeline_bench/query_pipeline/validate_with_warnings/simple/two_node_edge/median_ns`               | 250.018 ns       | 1.004     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |

<!-- bench-reference:end -->

## Run Command

```bash
cd crates && cargo bench -p khive-query --bench parse_bench
```

Or from the workspace root:

```bash
cargo bench -p khive-query
```

## Benchmark Targets

Declared in `Cargo.toml` as `[[bench]] name = "parse_bench" harness = false`.

| Target        | File                     | Harness   |
| ------------- | ------------------------ | --------- |
| `parse_bench` | `benches/parse_bench.rs` | Criterion |

## Benchmark Groups and Scenarios

| Group           | Scenario                              | Input Shape                                |
| --------------- | ------------------------------------- | ------------------------------------------ |
| `gql`           | `gql/simple_node`                     | Single node, no WHERE                      |
| `gql`           | `gql/two_node_edge`                   | Node–edge–node chain                       |
| `gql`           | `gql/node_with_limit`                 | Single node with LIMIT                     |
| `gql_medium`    | `gql/where_eq_string`                 | Edge pattern with string equality WHERE    |
| `gql_medium`    | `gql/where_and`                       | WHERE with AND                             |
| `gql_medium`    | `gql/where_or`                        | WHERE with OR                              |
| `gql_medium`    | `gql/where_and_or`                    | WHERE with mixed AND/OR                    |
| `gql_medium`    | `gql/where_with_edge_var`             | WHERE referencing edge variable            |
| `gql_medium`    | `gql/node_with_properties`            | Node with inline property map              |
| `gql_complex`   | `gql/three_node_chain`                | Three-node chain (two edges)               |
| `gql_complex`   | `gql/variable_length_multi_rel`       | Variable-length with multi-relation        |
| `gql_complex`   | `gql/variable_length_with_where`      | Variable-length with WHERE                 |
| `gql_complex`   | `gql/three_node_mixed_direction`      | Three-node with mixed edge directions      |
| `gql_complex`   | `gql/node_multi_property_map`         | Node with multiple inline properties       |
| `gql_complex`   | `gql/undirected_edge`                 | Undirected edge pattern                    |
| `sparql`        | `sparql/two_node`                     | Two-node SPARQL pattern                    |
| `sparql`        | `sparql/variable_length_plus`         | SPARQL `+` path operator                   |
| `sparql`        | `sparql/explicit_range`               | SPARQL `{1,3}` explicit range              |
| `sparql_medium` | `sparql/three_node_chain`             | Three-node chain                           |
| `sparql_medium` | `sparql/with_property_filter`         | With property filter                       |
| `sparql_medium` | `sparql/kind_and_property_filter`     | Kind plus property filter                  |
| `parse_auto`    | `auto/gql_dispatch`                   | Auto-detect GQL                            |
| `parse_auto`    | `auto/sparql_dispatch`                | Auto-detect SPARQL                         |
| `parse_auto`    | `auto/gql_with_leading_whitespace`    | Auto-detect GQL with leading whitespace    |
| `parse_auto`    | `auto/sparql_with_leading_whitespace` | Auto-detect SPARQL with leading whitespace |

## Environment Notes

- All benchmarks measure parse latency only (no SQL compilation, no DB execution).
- Sample sizes: `gql` and `gql_medium` groups use 200 samples; `gql_complex` and `sparql_medium`
  use 100 samples; `parse_auto` uses 200 samples.
- Run on a quiet machine with no competing processes for stable results.
- Criterion writes HTML reports to `target/criterion/`.

## Release Ledger

### v0.2.8 - 2026-06-08

- Commit: `d3629501c550fd2f3bb7ed350a2b60309d596465`
- Crate version: `0.2.8`
- Khive version: `0.2.8`
- Toolchain: `rustc 1.94.1 (e408947bf 2026-03-25)`, release profile (Criterion)
- Machine: Apple M-series arm64, macOS Darwin 25.5.0, 16 GB
- Feature flags: default
- Command: `cd crates && cargo bench -p khive-query --bench parse_bench`
- Dataset: inline string literals in bench source (no external fixture); parse-only, no DB
- vs prior: first formal release ledger entry — no prior comparable baseline

#### GQL Parse Latency

| Scenario                        | Low      | Median   | High     | Outliers     |
| ------------------------------- | -------- | -------- | -------- | ------------ |
| gql/simple_node                 | 482.2 ns | 497.1 ns | 516.1 ns | 17/200 (9%)  |
| gql/two_node_edge               | 1.029 µs | 1.158 µs | 1.296 µs | 46/200 (23%) |
| gql/node_with_limit             | 556.2 ns | 568.5 ns | 582.8 ns | 12/200 (6%)  |
| gql_medium/where_eq_string      | 1.006 µs | 1.015 µs | 1.026 µs | 28/200 (14%) |
| gql_medium/where_and            | 1.154 µs | 1.215 µs | 1.296 µs | 27/200 (14%) |
| gql_medium/where_or             | 1.212 µs | 1.297 µs | 1.393 µs | 32/200 (16%) |
| gql_medium/where_and_or         | 1.505 µs | 1.639 µs | 1.796 µs | 18/200 (9%)  |
| gql_medium/where_with_edge_var  | 1.171 µs | 1.258 µs | 1.365 µs | 17/200 (9%)  |
| gql_medium/node_with_properties | 735.3 ns | 771.8 ns | 814.8 ns | 23/200 (12%) |

#### GQL Complex Parse Latency

| Scenario                          | Low      | Median   | High     | Outliers     |
| --------------------------------- | -------- | -------- | -------- | ------------ |
| gql_complex/three_node_chain      | 1.074 µs | 1.150 µs | 1.250 µs | 10/100 (10%) |
| gql_complex/var_length_multi_rel  | 918.6 ns | 977.9 ns | 1.049 µs | 15/100 (15%) |
| gql_complex/var_length_with_where | 1.042 µs | 1.091 µs | 1.146 µs | 14/100 (14%) |
| gql_complex/three_node_mixed_dir  | 1.106 µs | 1.124 µs | 1.147 µs | 8/100 (8%)   |
| gql_complex/node_multi_prop_map   | 950.1 ns | 981.6 ns | 1.024 µs | 11/100 (11%) |
| gql_complex/undirected_edge       | 771.7 ns | 813.0 ns | 870.8 ns | 17/100 (17%) |

#### SPARQL Parse Latency

| Scenario                           | Low      | Median   | High     | Outliers     |
| ---------------------------------- | -------- | -------- | -------- | ------------ |
| sparql/two_node                    | 1.380 µs | 1.406 µs | 1.441 µs | 16/200 (8%)  |
| sparql/variable_length_plus        | 1.423 µs | 1.446 µs | 1.477 µs | —            |
| sparql/explicit_range              | 1.144 µs | 1.162 µs | 1.185 µs | —            |
| sparql_medium/three_node_chain     | 1.796 µs | 1.842 µs | 1.908 µs | —            |
| sparql_medium/with_property_filter | 1.672 µs | 1.723 µs | 1.791 µs | —            |
| sparql_medium/kind_and_prop_filter | 1.769 µs | 1.819 µs | 1.883 µs | 10/100 (10%) |

#### Auto-Detect Dispatch

| Scenario                   | Low      | Median   | High     | Outliers    |
| -------------------------- | -------- | -------- | -------- | ----------- |
| parse_auto/gql_dispatch    | 684.1 ns | 693.4 ns | 705.3 ns | 10/200 (5%) |
| parse_auto/sparql_dispatch | 1.330 µs | 1.334 µs | 1.338 µs | 7/200 (4%)  |
| parse_auto/gql_leading_ws  | 450.0 ns | 453.9 ns | 459.0 ns | —           |

- Notes: none

Last reviewed: v0.2.8 (2026-06-08)

## Compilation, validation, and large inputs

`pipeline_bench` adds downstream CPU work to the original `parse_bench` target.
The parse-only environment notes and release ledger above describe that original
target. Neither target opens a database or measures SQL execution.

```bash
cd crates
cargo bench -p khive-query --bench pipeline_bench -- --quick --noplot
cargo bench -p khive-query --benches -- --quick --noplot
```

The owning benchmark component is `ann-query`. New baselines are **pending** a
qualified-host run of that component, which records the commit, toolchain,
machine, flags, command, and sample report with the measurements. Historical
values above remain observations from their recorded release.

| Group            | Scenario names                                             | Measurement                                                                   | Input sizes                                                                                          | Baseline |
| ---------------- | ---------------------------------------------------------- | ----------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------- | -------- |
| `query_pipeline` | `compile/{simple,medium,complex}/{fixture}`                | Public `compile` on a parsed AST, including its internal clone and validation | All 15 existing GQL strings; one namespace; `max_limit=500`                                          | pending  |
| `query_pipeline` | `validate/{simple,medium,complex}/{fixture}`               | Public in-place `validate` on a fresh AST                                     | Same 15 strings                                                                                      | pending  |
| `query_pipeline` | `validate_with_warnings/{simple,medium,complex}/{fixture}` | Public validation and warning collection on a fresh AST                       | Same 15 strings; these valid fixtures return no warnings                                             | pending  |
| `query_pipeline` | `large/parse_10_nodes_20_properties`                       | GQL parsing                                                                   | Generated 10-node, nine-edge chain; 20 string-valued properties on its first node                    | pending  |
| `query_pipeline` | `large/parse_compile_10_nodes_20_properties`               | One parse followed by public compile                                          | Same generated chain; one namespace; `max_limit=500`                                                 | pending  |
| `query_pipeline` | `sparql_complex/four_edges_six_triples_bounded_paths`      | SPARQL parsing                                                                | Five-node chain; four relation triples, one kind triple, one property triple; `{1,3}` and `+` bounds | pending  |

The 15 GQL fixture suffixes exactly reuse the existing inputs:

- `simple`: `simple_node`, `two_node_edge`, `node_with_limit`.
- `medium`: `where_eq_string`, `where_and`, `where_or`, `where_and_or`,
  `where_with_edge_var`, `node_with_properties`.
- `complex`: `three_node_chain`, `variable_length_multi_rel`,
  `variable_length_with_where`, `three_node_mixed_direction`,
  `node_multi_property_map`, `undirected_edge`.

There are 48 new scenarios. In measurement mode (`--bench`, as supplied by
`cargo bench`), each selected scenario prints one `query_call_report` from
1,001 actual single calls, with median defined as sorted rank 501. This report
follows one preflight and runs inside Criterion's first selected callback,
before `iter_custom` begins recording its iteration timing (10 samples,
one-second warmup, three-second measurement target; `--quick` selects
Criterion quick mode).

`--list` and filtered-out scenarios execute no operation, preflight or report
samples. Test mode (`--test`, or no `--bench`) executes one checked operation
per selected scenario and emits no supplemental timing report. Profiling
(`--profile-time`) also omits the supplemental report and preflight, so they do
not enter Criterion's profiler. Small deterministic strings are still constructed
while registering the scenarios; GQL fixture parsing is lazy and occurs during
untimed preparation only when a scenario executes. Listing is not an allocation
measurement. Benchmark IDs, fixture contents and per-call timing boundaries are
unchanged.
Only 1,001 durations are retained at once. Per-call `Instant` overhead is included;
these short-call observations are advisory and should be compared using the same
harness and host, rather than treated as interchangeable with the historical
parse harness estimates.

AST preparation/cloning, result assertions, input/output destruction, sorting,
and reporting are outside each timer. Validators receive a fresh clone before
**every** call, including warmup. Compile inputs are preparsed, while the
compiler's own internal clone and validation remain timed. Parse-plus-compile
retains both outputs until after timing. Criterion receives the sum of actual
call durations; reset and checking still consume additional wall time. Compiler
outputs must preserve requested projections, bind the private namespace, and
emit SELECT/WITH SQL. Large-input checks verify every generated property value
in the AST and compiled parameters; SPARQL checks all four relations and bounds.
Every invocation must succeed.

The complex SPARQL fixture uses supported **flat connected triples with bounded
paths**. The issue's literal nested property-path or nested WHERE-group syntax
is unsupported: `parse_predicate` accepts one relation with an optional `+` or
`{min,max}`, and `triples_to_ast` reconstructs one linear path. That nested-syntax
portion is not measured. This scene measures parsing only; compilation of a
mixed fixed/variable multi-edge chain is also unsupported by the current SQL
compiler. No grammar or compiler behavior is changed for these benchmarks.

## Performance guarantees

All rows are advisory latency trends. No fixed timing threshold, allocation
promise, or new deterministic count gate is introduced. Existing historical
values are not current measurements or guarantees. Baselines for the new harness
remain pending the `ann-query` qualified-host run.

| Operation                    | Input                          | Metric         | Bound                                        | Baseline             |
| ---------------------------- | ------------------------------ | -------------- | -------------------------------------------- | -------------------- |
| GQL parse (simple_node)      | inline simple node             | latency median | Advisory trend; last ledger value 497.1 ns   | historical: 497.1 ns |
| GQL parse (three_node_chain) | complex 3-node chain           | latency median | Advisory trend; last ledger value 1.150 µs   | historical: 1.150 µs |
| SPARQL parse (two_node)      | 2-node SPARQL                  | latency median | Advisory trend; last ledger value 1.406 µs   | historical: 1.406 µs |
| compile (simple GQL)         | simple_node AST                | latency median | Advisory trend against the recorded baseline | pending              |
| validate (medium GQL)        | where_and AST                  | latency median | Advisory trend against the recorded baseline | pending              |
| parse + compile (large)      | 10-node chain, 20-property map | latency median | Advisory trend against the recorded baseline | pending              |

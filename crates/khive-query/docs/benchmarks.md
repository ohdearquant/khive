# khive-query Benchmark Ledger

## Run Command

```bash
cargo bench --manifest-path crates/khive-query/Cargo.toml
```

Or from the workspace root:

```bash
cargo bench -p khive-query
```

## Benchmark Targets

Declared in `Cargo.toml` as `[[bench]] name = "parse_bench" harness = false`.

| Target | File | Harness |
| --- | --- | --- |
| `parse_bench` | `benches/parse_bench.rs` | Criterion |

## Benchmark Groups and Scenarios

| Group | Scenario | Input Shape |
| --- | --- | --- |
| `gql` | `gql/simple_node` | Single node, no WHERE |
| `gql` | `gql/two_node_edge` | Node–edge–node chain |
| `gql` | `gql/node_with_limit` | Single node with LIMIT |
| `gql_medium` | `gql/where_eq_string` | Edge pattern with string equality WHERE |
| `gql_medium` | `gql/where_and` | WHERE with AND |
| `gql_medium` | `gql/where_or` | WHERE with OR |
| `gql_medium` | `gql/where_and_or` | WHERE with mixed AND/OR |
| `gql_medium` | `gql/where_with_edge_var` | WHERE referencing edge variable |
| `gql_medium` | `gql/node_with_properties` | Node with inline property map |
| `gql_complex` | `gql/three_node_chain` | Three-node chain (two edges) |
| `gql_complex` | `gql/variable_length_multi_rel` | Variable-length with multi-relation |
| `gql_complex` | `gql/variable_length_with_where` | Variable-length with WHERE |
| `gql_complex` | `gql/three_node_mixed_direction` | Three-node with mixed edge directions |
| `gql_complex` | `gql/node_multi_property_map` | Node with multiple inline properties |
| `gql_complex` | `gql/undirected_edge` | Undirected edge pattern |
| `sparql` | `sparql/two_node` | Two-node SPARQL pattern |
| `sparql` | `sparql/variable_length_plus` | SPARQL `+` path operator |
| `sparql` | `sparql/explicit_range` | SPARQL `{1,3}` explicit range |
| `sparql_medium` | `sparql/three_node_chain` | Three-node chain |
| `sparql_medium` | `sparql/with_property_filter` | With property filter |
| `sparql_medium` | `sparql/kind_and_property_filter` | Kind plus property filter |
| `parse_auto` | `auto/gql_dispatch` | Auto-detect GQL |
| `parse_auto` | `auto/sparql_dispatch` | Auto-detect SPARQL |
| `parse_auto` | `auto/gql_with_leading_whitespace` | Auto-detect GQL with leading whitespace |
| `parse_auto` | `auto/sparql_with_leading_whitespace` | Auto-detect SPARQL with leading whitespace |

## Environment Notes

- All benchmarks measure parse latency only (no SQL compilation, no DB execution).
- Sample sizes: `gql` and `gql_medium` groups use 200 samples; `gql_complex` and `sparql_medium` use 100 samples; `parse_auto` uses 200 samples.
- Run on a quiet machine with no competing processes for stable results.
- Criterion writes HTML reports to `target/criterion/`.

## Baseline (2026-06-06, post-sweep)

**Toolchain:** rustc 1.94.1 (e408947bf 2026-03-25)
**Machine:** arm64 (Apple Silicon), macOS Darwin 25.5.0

### GQL Parse Latency

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

### GQL Complex Parse Latency

| Scenario                          | Low      | Median   | High     | Outliers     |
| --------------------------------- | -------- | -------- | -------- | ------------ |
| gql_complex/three_node_chain      | 1.074 µs | 1.150 µs | 1.250 µs | 10/100 (10%) |
| gql_complex/var_length_multi_rel  | 918.6 ns | 977.9 ns | 1.049 µs | 15/100 (15%) |
| gql_complex/var_length_with_where | 1.042 µs | 1.091 µs | 1.146 µs | 14/100 (14%) |
| gql_complex/three_node_mixed_dir  | 1.106 µs | 1.124 µs | 1.147 µs | 8/100 (8%)   |
| gql_complex/node_multi_prop_map   | 950.1 ns | 981.6 ns | 1.024 µs | 11/100 (11%) |
| gql_complex/undirected_edge       | 771.7 ns | 813.0 ns | 870.8 ns | 17/100 (17%) |

### SPARQL Parse Latency

| Scenario                          | Low      | Median   | High     | Outliers     |
| --------------------------------- | -------- | -------- | -------- | ------------ |
| sparql/two_node                   | 1.380 µs | 1.406 µs | 1.441 µs | 16/200 (8%)  |
| sparql/variable_length_plus       | 1.423 µs | 1.446 µs | 1.477 µs | —            |
| sparql/explicit_range             | 1.144 µs | 1.162 µs | 1.185 µs | —            |
| sparql_medium/three_node_chain    | 1.796 µs | 1.842 µs | 1.908 µs | —            |
| sparql_medium/with_property_filter| 1.672 µs | 1.723 µs | 1.791 µs | —            |
| sparql_medium/kind_and_prop_filter| 1.769 µs | 1.819 µs | 1.883 µs | 10/100 (10%) |

### Auto-Detect Dispatch

| Scenario                     | Low      | Median   | High     | Outliers    |
| ---------------------------- | -------- | -------- | -------- | ----------- |
| parse_auto/gql_dispatch      | 684.1 ns | 693.4 ns | 705.3 ns | 10/200 (5%) |
| parse_auto/sparql_dispatch   | 1.330 µs | 1.334 µs | 1.338 µs | 7/200 (4%)  |
| parse_auto/gql_leading_ws    | 450.0 ns | 453.9 ns | 459.0 ns | —           |

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

## Release Baseline

| Scenario | Baseline | Date | Commit | Toolchain | Machine |
| --- | --- | --- | --- | --- | --- |
| _(not yet recorded)_ | — | — | — | — | — |

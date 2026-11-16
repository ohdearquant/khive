# Query AST and Error Model

The query AST is the shared contract between the GQL and SPARQL parsers, validation, and SQL compilation. It deliberately models only the values and syntax the query pipeline can preserve and execute without depending on a storage backend.

## `GqlQuery`

`GqlQuery` contains a required first `pattern: MatchPattern`, `additional_patterns: Vec<MatchPattern>`, a `WhereExpr`, ordered `ReturnItem` projections, a `distinct: bool`, `order_by: Vec<OrderItem>`, a non-negative row `offset`, and an optional row limit. The `patterns()` and `patterns_mut()` iterators visit the first path and additional paths in textual order. An empty additional vector represents the existing single-path query.

All paths share the same WHERE, projection, ordering and paging tail. GQL `SKIP` populates `offset`; SPARQL leaves it at zero because its `OFFSET` form is not supported, and initializes `distinct: false` and empty order and additional-pattern vectors. Parsers produce this form; direct Rust constructors must initialize `additional_patterns` and `distinct` and still pass the AST through `validate` or `compile`, which repeats the structural and connectivity checks. This field addition is a Rust source compatibility break; parsed single-path syntax is unchanged.

## `MatchPattern` and `PatternElement`

`MatchPattern.elements` must be `Node, Edge, Node, ...`, begin and end with a node, and contain no repeated node binding within that path. Edge names are unique across the entire query, and no name can bind both a node and an edge. `nodes()` and `edges()` expose typed iterators without allocating, while `has_variable_length()` is true when any edge differs from exactly one hop (`min_hops != 1 || max_hops != 1`). This includes `0..0` and `0..1`, which need the traversal lowering even though their maximum is at most one.

With multiple paths, every edge is canonical and fixed at one hop; a path may contain several such edges. Paths must connect through shared named nodes, including when a later path bridges earlier ones. A repeated node across paths denotes the same substrate and UUID, with every occurrence constraint retained. Anonymous occurrences are fresh. A connected singleton node path adds constraints; an empty additional path or disconnected component is refused.

`NodePattern` carries an optional variable, a pack-owned kind string, an optional governed `entity_type`, and inline property equalities. `entity_type` maps to its dedicated SQL column rather than `json_extract`.

`EdgePattern` carries an optional variable, zero or more relation alternatives, direction, and inclusive hop bounds. Each variable-length segment in a canonical path may have a minimum of zero. Its depth-zero start and end are the same substrate and UUID, accumulated weight is zero, and its final-edge binding is null. Both endpoint constraints still apply. `EdgeDirection::{Out, In, Both}` correspond to outgoing, incoming, and either-direction traversal. A single path may combine fixed steps and multiple bounded variable segments. Their maximum hop counts sum to at most ten; fixed steps do not consume that budget. Bounds `1..1`, including parsed `*1` and `*1..1`, represent fixed steps without a separate lexical marker. Ordinary traversal metrics sum only variable-segment hops and weights. Each edge variable retains its own binding; a variable segment exposes its final edge, including null at zero hops.

## `WhereExpr`, `Condition`, and `ReturnItem`

`WhereExpr` preserves the boolean tree instead of flattening it: `And`, `Or`, a scalar `Condition`, `NotExists(MatchPattern)`, or `True` when the query has no `WHERE` clause. `conditions()` walks outer scalar leaves depth-first from left to right; `for_each_condition_mut()` provides the same traversal for normalization. Both skip scoped absence paths. `is_true()` detects only the explicit `True` variant, not an absence atom with no scalar conditions.

`NotExists` contains one nonempty canonical fixed path, including a singleton, correlated through at least one outer node name. Correlation compares substrate and UUID and retains every inline constraint. Additional names are block-local and cannot be returned, counted, ordered or referenced by outer scalar conditions. Local names may recur in separate blocks; within one block the usual repeated-node, repeated-edge and node/edge conflict refusals apply. An inner edge cannot reuse an outer edge name: edge variables are not correlated into NOT EXISTS, and the name cannot shadow the outer binding. Exhaustive `WhereExpr` consumers must handle the new variant. No general subquery AST or SPARQL syntax is introduced.

Direct-AST `True OR x` and `x OR True` remain true. Lowering retains parameter references when rendering that identity; this is not a general Boolean optimizer.

A `Condition` binds a variable and `PropertyRef` to a `CompareOp` and `ConditionValue`. `PropertyRef::Field` names a dedicated substrate field, `PropertyRef::JsonPath` stores the non-empty identifier segments below JSON `properties`, and `PropertyRef::MetadataPath` stores those below canonical edge `metadata`. These roots are distinct: node metadata and edge properties are unsupported. The distinction prevents an unknown field from silently becoming a JSON lookup. Operators cover scalar comparisons, `LIKE`, literal `CONTAINS` and `STARTS WITH`, list-valued `IN`, and operand-free `IS NOT NULL`. A `ReturnItem` is a whole bound `Variable`, a direct `Property(String, String)`, a typed `Path(String, PropertyRef)` projection, or `Count(String)` for a bound variable; `variable()` returns the binding name in every case. `RETURN e.metadata` projects the stored container, while the existing whole-edge expansion remains unchanged.

`Path` accepts `JsonPath` below a node's exposed properties or `MetadataPath` below canonical edge metadata. A `Field` inside `Path`, empty or invalid path segments, unknown bindings and wrong binding kinds are refused during compilation, including for hand-built ASTs. Existing `Property` constructors retain their behavior. Exhaustive `ReturnItem` consumers must handle the additive `Path` and `Count` variants; this introduces no new SPARQL projection syntax.

Path output names join the variable, root and segments with underscores without case folding: `e.metadata.rank.order` becomes `e_metadata_rank_order`. Different expressions with the same exact output name are refused, including collisions with direct fields, expanded whole bindings or implicit traversal metrics. Repeating the same expression retains its existing duplicate projection behavior. Names remain quoted output labels, not new `AS` aliases or `ORDER BY` references.

`Count(variable)` counts matched binding rows, named `count_<variable>`. Repeated bindings on different matching paths count separately. A null final edge at zero hops contributes zero; a bound node or mandatory synthetic observation edge contributes one. Counts never count distinct IDs implicitly. Unknown bindings are refused during compilation.

When counts appear, every emitted nonaggregate expression is an implicit grouping key. Whole bindings contribute every expanded field. Counts-only empty matches return one integer-zero row; grouped empty matches return no rows. `distinct` deduplicates the requested values after grouping. Both modes omit implicit recursive depth/weight metrics and use only returned values for grouping and deterministic ties. Text uses binary comparison, numeric values retain numeric comparison, and missing/JSON-null leaves share a null group. Grouping and deduplication precede ordering, offset and limit.

## `OrderItem` and `OrderDirection`

Each `OrderItem` carries `variable: String`, `property: PropertyRef`, and `direction: OrderDirection`. `OrderDirection::{Asc, Desc}` makes direction a closed choice; the GQL parser defaults to `Asc`. Order terms reference supported scalar fields or named paths below node properties or canonical edge metadata. Bare containers, namespace fields, unknown bindings, and invalid path segments are rejected, including in hand-built ASTs.

The compiler adds these terms before its existing deterministic tie order without adding result columns. Recursive, explicit-distinct and grouped queries require each term to be derivable from the actual returned projections so `DISTINCT` cannot hide an ambiguous sort value. Existing queries without new path projections, counts or explicit distinct preserve their SQL and parameter sequence when their output names are unambiguous. Recursive queries returning a path order by the selected expressions rather than output-label lookup, including for implicit metrics, so labels differing only by case remain distinct.

Direct Rust consumers must initialize the new `order_by` field and handle the added `MetadataPath` variant when matching `PropertyRef` exhaustively. No serialization contract for the AST is introduced.

## Numeric and literal values

`ConditionValue` distinguishes integers from decimal floats so SQLite receives the same storage class expressed by the source:

| Variant   | Source form                    | SQL parameter behavior                 |
| --------- | ------------------------------ | -------------------------------------- |
| `String`  | quoted text                    | `TEXT`; equality uses `COLLATE NOCASE` |
| `Integer` | digits without a decimal point | exact `i64` / `INTEGER`                |
| `Number`  | digits with a decimal point    | finite `f64` / `REAL`                  |
| `Bool`    | `true` or `false`              | integer `1` or `0`                     |
| `List`    | bracketed values               | operands for `IN`                      |
| `Null`    | internal marker only           | operand marker for `IS NOT NULL`       |

The integer/float split prevents values above `2^53`, including both `i64` bounds, from being rounded through `f64` before comparison (issue #832). Parsed floats must be finite; compilation repeats that check for hand-built ASTs. Quoted numeric strings remain text and are never coerced.

`QueryValue` is the compiler's backend-independent parameter representation. It mirrors only the storage value variants needed at the runtime boundary: null, integer, float, text, and blob.

## `QueryError`

The pipeline classifies failures so callers can distinguish malformed input from unsupported but well-formed requests:

| Variant        | Meaning                                                                            |
| -------------- | ---------------------------------------------------------------------------------- |
| `Parse`        | malformed dialect syntax or an out-of-range/non-finite numeric literal             |
| `Validation`   | invalid AST shape, namespace in query text, unknown relation, or invalid hop range |
| `Compile`      | the AST cannot be lowered, such as an unknown binding or projection                |
| `Unsupported`  | recognized semantics the current compiler cannot represent                         |
| `InvalidInput` | runtime/compiler limit violation, overflow, or non-finite hand-built value         |

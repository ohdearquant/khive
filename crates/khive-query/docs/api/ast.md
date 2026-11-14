# Query AST and Error Model

The query AST is the shared contract between the GQL and SPARQL parsers, validation, and SQL compilation. It deliberately models only the values and syntax the query pipeline can preserve and execute without depending on a storage backend.

## `GqlQuery`

`GqlQuery` contains one alternating `MatchPattern`, a `WhereExpr`, ordered `ReturnItem` projections, `order_by: Vec<OrderItem>`, a non-negative row `offset`, and an optional row limit. GQL `SKIP` populates `offset`; the SPARQL parser leaves it at zero because its `OFFSET` form is not supported. SPARQL also initializes an empty order list. Parsers produce this form; callers that construct it directly must still pass it through `validate` or `compile`, which repeats the structural validation.

## `MatchPattern` and `PatternElement`

`MatchPattern.elements` must be `Node, Edge, Node, ...`, begin and end with a node, and contain no repeated binding names. `nodes()` and `edges()` expose typed iterators without allocating, while `has_variable_length()` is true when any edge has `max_hops > 1`.

`NodePattern` carries an optional variable, a pack-owned kind string, an optional governed `entity_type`, and inline property equalities. `entity_type` maps to its dedicated SQL column rather than `json_extract`.

`EdgePattern` carries an optional variable, zero or more relation alternatives, direction, and inclusive hop bounds. `EdgeDirection::{Out, In, Both}` correspond to outgoing, incoming, and either-direction traversal.

## `WhereExpr`, `Condition`, and `ReturnItem`

`WhereExpr` preserves the boolean tree instead of flattening it: `And`, `Or`, a leaf `Condition`, or `True` when the query has no `WHERE` clause. `conditions()` walks leaves depth-first from left to right; `for_each_condition_mut()` provides the same traversal for normalization; `is_true()` detects the identity expression.

A `Condition` binds a variable and `PropertyRef` to a `CompareOp` and `ConditionValue`. `PropertyRef::Field` names a dedicated substrate field, `PropertyRef::JsonPath` stores the non-empty identifier segments below JSON `properties`, and `PropertyRef::MetadataPath` stores those below canonical edge `metadata`. These roots are distinct: node metadata and edge properties are unsupported. The distinction prevents an unknown field from silently becoming a JSON lookup. Operators cover scalar comparisons, `LIKE`, literal `CONTAINS` and `STARTS WITH`, list-valued `IN`, and operand-free `IS NOT NULL`. A `ReturnItem` is either a whole bound variable or one property projection; `variable()` returns the binding name in both cases. `RETURN e.metadata` projects the stored container, while the existing whole-edge expansion remains unchanged.

## `OrderItem` and `OrderDirection`

Each `OrderItem` carries `variable: String`, `property: PropertyRef`, and `direction: OrderDirection`. `OrderDirection::{Asc, Desc}` makes direction a closed choice; the GQL parser defaults to `Asc`. Order terms reference supported scalar fields or named paths below node properties or canonical edge metadata. Bare containers, namespace fields, unknown bindings, and invalid path segments are rejected, including in hand-built ASTs.

The compiler adds these terms before its existing deterministic tie order without adding result columns. Recursive queries additionally require each term to be derivable from the actual returned projections so `DISTINCT` cannot hide an ambiguous sort value. Queries using neither metadata nor `ORDER BY` preserve the old SQL and parameter sequence.

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

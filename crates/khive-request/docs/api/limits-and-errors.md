# Resource Limits and Error Taxonomy

The parser applies explicit size, operation-count, and nesting bounds before a request reaches execution. `DslError` preserves the failure category and relevant position or field details for the MCP boundary's `invalid_params` response.

## Limits

| Constant              | Value | Enforcement                                            |
| --------------------- | ----: | ------------------------------------------------------ |
| `MAX_OPS`             |   100 | function batches, chains, and JSON arrays              |
| `MAX_OPS_INPUT_LEN`   | 1 MiB | raw trimmed UTF-8 byte length before parsing           |
| `NESTING_DEPTH_LIMIT` |    64 | function-call containers and quote-aware JSON pre-scan |

The 1 MiB input cap leaves headroom for ADR-038's 1,000-item bulk-create contract: a representative payload with roughly 220-byte descriptions is about 276 KiB after envelope overhead, already beyond a 256 KiB cap. The limit still rejects multi-hundred-megabyte inputs early.

Real request payloads are normally two to four containers deep. A limit of 64 remains generous while preventing compact, pathologically nested input from exhausting native recursive descent (CWE-674).

## `value_nesting_within_limit`

This public helper checks a runtime `serde_json::Value`, including handler results about to become `$prev` context. It uses an explicit heap stack and returns `false` as soon as an array or object would exceed the caller's `max_depth`; scalar roots have depth zero. Avoiding native recursion lets it reject hostile depth before cloning or serialization threatens the thread stack.

## `DslError`

| Variant                  | Condition                                                            |
| ------------------------ | -------------------------------------------------------------------- |
| `Empty`                  | no input after trimming                                              |
| `TooManyOps`             | operation count exceeds `MAX_OPS`                                    |
| `InputTooLarge`          | byte length exceeds `MAX_OPS_INPUT_LEN`                              |
| `NestingTooDeep`         | array/object depth exceeds `NESTING_DEPTH_LIMIT`                     |
| `UnexpectedChar`         | wrong delimiter or token at a byte position                          |
| `UnexpectedEof`          | input ends before a required token                                   |
| `InvalidIdentifier`      | identifier violates the ASCII identifier grammar                     |
| `DuplicateArg`           | one operation repeats an argument name                               |
| `InvalidValue`           | function-form value cannot be decoded or reference syntax is invalid |
| `InvalidJson`            | JSON form is malformed or has the wrong shape                        |
| `UnclosedString`         | quoted string has no terminator                                      |
| `UnclosedBracket`        | array, object, or parenthesis has no matching close                  |
| `PrevRefOutsideChain`    | function-form `$prev` appears in single/parallel mode                |
| `PrevRefInJsonForm`      | JSON string contains a `$prev` reference                             |
| `MixedSeparators`        | top level mixes parallel comma and chain pipe                        |
| `EmptyBatch`             | `[]` contains no operations                                          |
| `UnsupportedVerbNesting` | a tool name has more than one dot                                    |
| `WriteKeyConflict`       | two parallel operations claim the same derived write key             |
| `ReservedEnvelopeArg`    | an operation contains an envelope-only field                         |

Display messages are actionable and retain values such as byte position, count, duplicated argument, conflicting tools, or reserved field. The enum implements `std::error::Error` and is surfaced as invalid request parameters rather than an operation-level execution failure.

# Previous-Result References

Chain arguments can refer to the immediately preceding operation result through `$prev`. The parser preserves reference paths in `ArgValue`, and `resolve_prev`/`resolve_all` apply them later against a JSON result.

## Reference grammar

An empty path selects the whole result. Field and zero-based array-index segments may be combined:

| Reference             | Selection                   |
| --------------------- | --------------------------- |
| `$prev`               | whole prior result          |
| `$prev.id`            | object field `id`           |
| `$prev.result.id`     | nested object fields        |
| `$prev[0].id`         | array element, then field   |
| `$prev.items[1].name` | field, array element, field |

Unquoted references require valid identifiers after dots and non-negative decimal indices inside brackets. A missing bracket, empty/non-numeric index, or unexpected character is a parse error.

## Quoted references and escaping

A JSON string containing a valid `$prev` path is promoted to the same `ArgValue::PrevRef` as the unquoted form. Promotion requires an exact `$prev`, `$prev.`, or `$prev[` boundary; `$prevish.id` remains ordinary text.

Malformed bracket syntax inside a quoted string remains an ordinary string instead of becoming a reference. This preserves literal user data while unquoted reference syntax remains strict.

To pass a literal string that otherwise looks like a reference, escape the leading dollar sign:

```text
update(id="\\$prev.id")
```

JSON decoding produces `\$prev.id`; the parser removes that one escape and stores the concrete string `$prev.id`.

## `ArgValue::resolve_prev`

`resolve_prev` operates only on `PrevRef`. It returns the prior result for an empty path, otherwise walks object fields and array elements without cloning. It returns `None` when called on another variant, a field is absent, a value is not an array at an index segment, or an index is out of range.

## `ArgValue::resolve_all`

`resolve_all` recursively materializes an argument into owned JSON. Concrete values are cloned, references are resolved and cloned, and dynamic arrays/objects preserve order while resolving every descendant. If any reference path misses, the entire call returns `None`; partial materialization is never returned.

Runtime results used as future `$prev` context should first pass `value_nesting_within_limit`. That function walks an explicit heap worklist, avoiding recursive clone/serialization of attacker-controlled deep JSON before the depth bound is checked.

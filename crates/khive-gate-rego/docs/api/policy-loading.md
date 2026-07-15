# Policy loading and entrypoints

`RegoGate` can compile one inline module or a deterministic set of `.rego` files before it is
installed as a `Gate`.

## `from_policy_str`

This constructor registers the source as `inline.rego` and selects
`data.khive.gate.decision`. Rego parse or compilation failures return `GateError::Policy`; no gate
is returned with a partially loaded policy.

## `from_dir`

Directory loading is non-recursive and selects only files whose extension is exactly `rego`.
Every directory-entry error is propagated, an empty selection is rejected, and paths are sorted
before compilation to make load order deterministic across platforms. Any unreadable or invalid
file fails the whole constructor.

## `with_entrypoint`

The infallible builder trims and installs a caller-provided rule path without validation. It is for
programmatic values already checked by the application; an invalid path is denied later at
evaluation rather than producing a construction error.

## `try_with_entrypoint`

The operator-facing builder rejects empty or whitespace-only values, paths without a `data.` prefix,
and empty segments such as `data.a..b` or `data.a.`. It then evaluates the path with empty-object
input to confirm that the loaded policy names a compiled rule. Missing rules return
`GateError::Policy`; a poisoned validation mutex returns `GateError::Internal`.

Rule existence validation is defense in depth: runtime evaluation still fails closed for missing,
undefined, malformed, or unserializable decisions.

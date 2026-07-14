# Namespace dispatch (ADR-007)

Technical reference for how `VerbRegistry`-mediated dispatch resolves namespace scope for
reads and writes, per ADR-007 Rev 4/6. Source of the regression coverage: `dispatch.rs`
`mod tests`.

## By-ID ops are namespace-agnostic (PR-A1)

`get` returns a record regardless of the caller's namespace. The namespace on the returned
record still reflects the creator's namespace — it is never rewritten to the caller's. `list`
and `search` still filter by namespace (PR-B scope); only by-ID resolution (`get`, and by
extension `update`/`delete`/`merge`) is namespace-agnostic.

## Default namespace

Two `create` calls with no explicit `namespace=` land in the same `"local"` namespace —
`"local"` is the OSS default, not per-caller.

## Rule 3b: default read-scope widening (Rev 4)

The dispatch token minted by `VerbRegistry` on the default (no explicit `namespace=`) path
widens the READ scope to `['local'] ∪ visible_namespaces`:

- Records written to `"local"` are always visible in a registry-dispatched `list` — the
  shared-brain property.
- A record written to a configured visible namespace (via a directly-minted token) also
  appears in the registry-dispatched list.
- With `visible_namespaces` UNSET (backward-compat), the default read scope is `['local']`
  only, matching the prior (Rev 3) behavior — a record written to a different namespace via
  a directly-minted token does NOT appear in the registry list.
- `'local'` is always included in the default read scope even when `visible_namespaces` does
  not explicitly list it: configuring `visible_namespaces = ["other-ns"]` (without `"local"`)
  still returns records from BOTH `'local'` and `'other-ns'`.

## Explicit `namespace=` is precise, never widened

An explicit `namespace=` param is a precise single-namespace escape, not subject to the Rule
3b widening. With `visible_namespaces=["other-ns"]` configured, `list(namespace="other-ns")`
scopes to EXACTLY `["other-ns"]` and does not include `'local'` or the union set — this
preserves the ability to read a single named set precisely.

## Rule 0: non-local actor config never routes storage

A `VerbRegistry` whose `default_namespace` is a non-local actor (e.g. `[actor] id =
"sample-actor"`, simulated via `--actor sample-actor`) still routes both `create` and `list`
writes/reads through `VerbRegistry::dispatch` (the real MCP path) to `"local"`:

1. A created entity lands in `"local"`, not the configured actor namespace.
2. A subsequent registry `list` returns the entity — write and read both operate on
   `"local"` regardless of the non-local actor configuration.
3. A direct-token `list` scoped to the actor namespace (e.g. `"sample-actor"`) returns an empty
   set, proving storage was never written there.

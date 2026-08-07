# `kkernel kg review`

`kg review` is the read-only first slice of
[ADR-145](../../../docs/adr/ADR-145-local-first-kg-workbench.md). The npm `khive` shim forwards the
same arguments to `kkernel`, so the public invocation is:

```text
khive kg review <changeset.ndjson> --rules <rules.toml> \
  [--reviewer-model-family <family>] [--format text|json|github]
```

The command parses the strict ADR-101 NDJSON-delta change-set, preserves operation order, evaluates
the existing commit-time rule projection, applies the non-overridable ADR-102 tier floor, and emits
the `review_kind: "changeset"` variant of `khive.review.v1`.

It accepts no repository, database, commit, apply, push, or publish arguments. It does not mutate
Git, GitHub, or the live graph. The existing `kg commit` command remains the separate, local-only
ADR-102 commit lane.

## Partial validation is fail-closed

The current commit-time projector evaluates create/link state only. Until the shared pure rule
evaluator required by ADR-101 exists, each update, delete, or merge produces an error-level
`review-rule-coverage` finding. The report then has:

```json
{
  "validation": { "scope": "commit_time_partial_view", "passed": false },
  "review_gate": { "approval_ready": false }
}
```

This is a deliberate containment boundary: a partial projection may explain a change, but cannot
declare it validation-complete.

## Exit status

- `0`: validation passed and every required independent-review gate is satisfied.
- non-zero after a JSON/GitHub/text report: validation findings or review-gate state block
  approval.
- non-zero without a report: the change-set or rules input could not be read or parsed.

JSON output conforms to
[`docs/schemas/khive-review-v1.schema.json`](../../../docs/schemas/khive-review-v1.schema.json) and
is locked as a complete parsed JSON value to the shared
[`khive-review-v1-changeset.json`](../../../docs/schemas/examples/khive-review-v1-changeset.json)
golden vector by the Rust binary integration suite. The same file is directly consumable by the
TypeScript contract tests and can be imported into `apps/kg-editor`. Git and GitHub enrichment is
absent because this command has no authority to establish those identities.

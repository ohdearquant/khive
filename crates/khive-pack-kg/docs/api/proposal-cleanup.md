# Proposal cleanup

`kg.proposal_cleanup` archives resolved proposals in the caller's resolved primary
namespace. Run it through the operator CLI:

```sh
kkernel exec 'kg.proposal_cleanup(older_than="30d")'
```

`older_than` is required. It must be a positive ASCII integer followed by one
lowercase fixed-duration unit: `s`, `m`, `h`, or `d`. A day is 86,400 seconds.
Whitespace, signs, fractions, combined units, unknown fields, and values that
overflow the microsecond duration or cutoff are refused before acquiring the
cleanup writer. There is no default retention period and no caller-supplied clock.

The operator captures one UTC clock value and executes one bound update. Only
`applied` or `rejected` proposals with a null archive marker and an `updated_at`
strictly earlier than the cutoff are eligible. Equality with the cutoff is
retained. The comparison uses the current projection timestamp, so a later
projected comment restarts retention. All proposers in the primary namespace are
eligible; other namespaces and statuses are untouched.

The response is `{"archived": N}`, where `N` is the affected-row count. Repeating
the operation returns zero once those rows are archived. The write sets only
`archived_at`; it does not delete proposals, modify status or prior timestamps, or delete
lifecycle events. Ordinary `list(kind="proposal")` filters archives before
pagination. UUID and short-prefix lookup, archived parent references, and existing
terminal-state review/withdraw refusals retain their previous behavior.

This is an internal subhandler, omitted from public verb discovery. MCP single,
batch, and chained requests refuse to execute it. `kg.proposal_cleanup(help=true)`
may describe the internal operator, including `callable_via_mcp: false`. CLI calls
still require normal write authorization. Atomic batches reject cleanup during
preflight, before any sibling operation mutates state.

See [ADR-046 cleanup contract](../../../../docs/adr/ADR-046-event-sourced-proposals.md#amendment-resolved-proposal-cleanup-2026-10-09).

---
description: Review an open knowledge graph proposal by approving, rejecting, commenting, or requesting changes.
---

# Review

Use `review` after reading the proposal and checking the requested changes against source evidence.

The MCP server exposes one tool, `request`, that takes the verb call as a string:

```text
request(ops="review(id=\"00000000-0000-0000-0000-000000000001\", decision=\"approve\", comment=\"Change matches the cited evidence.\")")
```

Required args: `id`, `decision`. Optional args: `comment`, `namespace`.

Valid `decision` values: `approve`, `reject`, `comment`, `request_changes`, `requestchanges`.

**Namespace rule (ADR-007)**: KG operations always use the shared namespace (`local`). Do NOT
override the namespace with `lambda:*` actor namespaces.

## Workflow

### 1. Read the proposal

Retrieve the proposal before deciding:

```text
request(ops="get(id=\"<proposal-id>\")")
```

Inspect `changeset.kind` and the source IDs. Verify the cited entities exist and the relationship is
correct.

### 2. Verify source evidence

For an `add_edge` proposal, check both endpoints:

```text
request(ops="[get(id=\"<source-id>\"), get(id=\"<target-id>\")]")
```

For a `merge_entities` proposal, compare descriptions and edge counts before approving.

### 3. Decide

**Approve** — evidence supports the change, relation and direction are correct:

```text
request(ops="review(id=\"<id>\", decision=\"approve\", comment=\"Source entity id <paper-id> confirms the introduced_by direction.\")")
```

**Reject** — change is incorrect or cannot be verified:

```text
request(ops="review(id=\"<id>\", decision=\"reject\", comment=\"Direction is reversed. introduced_by should go concept → paper, not paper → concept.\")")
```

**Request changes** — proposal has the right intent but needs a corrected changeset:

```text
request(ops="review(id=\"<id>\", decision=\"request_changes\", comment=\"Weight should be 0.9 (definitional), not 0.4. Resubmit with corrected weight.\")")
```

**Comment** — add context without blocking or approving:

```text
request(ops="review(id=\"<id>\", decision=\"comment\", comment=\"Related proposal <other-id> also touches this entity — coordinate.\")")
```

## Constraints

`review` only accepts proposals in `open` or `changes_requested` state. Attempting to review a
proposal that is already `approved`, `rejected`, `applied`, or `withdrawn` returns an error.

The apply worker runs asynchronously after an approval — the proposal transitions through
`approved → applying → applied`. The reviewer sees an immediate "decision recorded" response; the
apply result surfaces as a separate event.

## Stop condition

Decision recorded. If approved, the runtime applies the changeset asynchronously. If rejected or
change-requested, the proposer must resubmit. Do not manually apply rejected changes.

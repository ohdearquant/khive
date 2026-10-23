# ADR-183: Batched Write Disposition: Commit What Passes, Name What Refuses

- **Status**: Proposed
- **Date**: 2026-09-11
- **Relates to**: [ADR-017](ADR-017-pack-standard.md) (pack verbs and handler return shape),
  [ADR-016](ADR-016-request-dsl.md) (per-op batching at the DSL layer, which this is not),
  [ADR-048](ADR-048-knowledge-section-profiles.md) (`knowledge.import`, which validates before
  it writes and is out of scope), [ADR-174](ADR-174-ordered-streams-append.md) (`stream.batch`,
  whose `atomic` flag this generalizes)

## Context

A verb that takes a list of records and validates each one in a loop returns on the first
refusal. Every other record in the call is refused with it, nothing is written, and the single
error describes only the offending value. The caller holds N records and one error about text
that lives in whichever record failed, with no way to tell which.

This was measured on `knowledge.upsert_atoms`. A run parked 97 atoms on a secret-gate refusal.
Re-sent one atom per call, 90 of the 97 committed; 7 refuse on their own. Of the 97, 57 do not
contain the reported trigger word anywhere in their own fields: the word was in a sibling record
of the same call. The scan is correct and per-record; the coupling is the control flow.

Two consequences follow, and the second is the reason this is an ADR rather than a bug fix.

A refused record and a bystander are **indistinguishable in the response**, so the caller cannot
separate "this contains a credential" from "this shared a call with something that did". The
available workaround is to stop batching, which trades away the verb's purpose for the ability to
tell the two apart.

And the verb is **making a durability decision the caller did not ask for**. Discarding valid
records because an invalid one arrived in the same call is a policy, not a mechanism, and today it
is a policy no document states and no caller can opt out of.

The tree already contains the answer in two places. `stream.batch` takes an `atomic` flag,
defaults it by whether the call carries a fence, and refuses the contradiction outright:
_"atomic=false cannot carry a fence: a fence admits no partial commit"_. The KG `create(items=...)`
bulk form takes the same flag, defaults it to `true`, and in its non-atomic mode collects one
error per failing item by index. That is the contract below, already written twice.

This is **not** the DSL's op-level batching. ADR-016 gives each op in a request its own
`{ok, tool, result}` and one failing op does not abort the others. That rule stops at the verb
boundary; inside a verb, a record list has no such disposition. This ADR gives it one.

## Decision

### 1. A batched write commits the records that pass

A verb that accepts a list of caller-supplied records writes every record that passes validation
and refuses the ones that do not. Success means, precisely: **everything named in `committed` is
durable.** It does not mean every record in the request was accepted.

_(Amendment 1, Decision A: for the two verbs in §4 this disposition applies only when the caller
passes `atomic=false`; the default is all-or-nothing.)_

### 2. The response names both sides, by position

A partial disposition is a successful op. Its result body is:

```json
{
  "status": "ok" | "partial" | "incomplete",
  "committed": [ { "index": 0, "id": "..." } ],
  "refused":  [ { "index": 1, "field": "content", "reason": "..." } ],
  "stopped":  { "index": 2, "reason": "...", "disposition": "not_written" | "unknown" }
}
```

`stopped` is present if and only if `status` is `incomplete`, and is absent from an `ok` or a
`partial` result. Its shape is the one section 6 defines, and it is repeated here because this
block is what an implementer reads: a schema that named the status without the object the status
requires would leave the one field a caller keys on to be discovered from prose.

- `status` is `ok` when every record in the request committed; `partial` when every record was
  attempted and at least one refused; `incomplete` when the call **stopped on a storage failure**
  (§6), whether or not any records remained after the one that failed. `incomplete` takes
  precedence over `partial`.

  The status is defined by the stop, not by the remainder, and the difference is load-bearing on
  exactly one shape. A store failure on the LAST record leaves nothing unattempted, so a
  remainder-based definition would call it `partial` while §6 calls it `incomplete`. That shape is
  also where the indeterminate outcome lives: a caller keying on `incomplete` to know it must
  re-read would skip the re-read on the one call whose `stopped.disposition` may be `unknown`.
  `stopped` rides an `incomplete` result in both cases.
- A record is identified by its **zero-based position in the request list**, never by a
  caller-supplied name. The slug is itself a scanned field, so echoing it would return the very
  text a secret-gate refusal exists to withhold; and two records in one payload may carry the same
  slug, which a name cannot separate and a position can.
- Each refusal names the record's `index` and the `field` that refused, so the caller can locate
  it in the payload it still holds.
- The refusal object as a whole **never carries the matched value**. A secret-gate refusal already
  renders a masked form in `reason`; the disposition adds position, it does not widen disclosure.
- `status`, `committed` and `refused` are fields of the result body. Records are nested objects
  inside `committed` and `refused`; a record's own properties cannot shadow them.
- **Every list this ADR defines rides the successful result body, never an error.** The error
  model's `details` is a bounded set of string pairs (`khive-types::Details`, eight entries, with
  its own truncation indicator), so it can carry the scalars that locate a refusal — `index`,
  `field`, `reason` — and cannot carry a record list. Any disposition that has records to name is
  therefore a result, not an error. _(Superseded by Amendment 1, Decision B: an atomic refusal
  carries a complete typed `refused` list beside the error fields, outside `details`.)_

### 3. `atomic=true` is how a caller asks for all-or-nothing

`atomic` is an optional boolean parameter on every verb this ADR binds. Its default is `false`
for the verbs in §4 that adopt this contract; verbs that already carry the flag keep their own
default (§4). _(Superseded by Amendment 1, Decision A: the default is `true` for both verbs.)_

With `atomic=true` the whole list is validated and written in **one write transaction**: the
first refusal aborts the call and nothing is written. The refusal is an op-level error, not a
`partial` result: the op's `ok` is `false`, and the error's `details` carry the same
`{index, field, reason}` object a partial result would have placed in `refused`. There is no
`committed` list on the error, because nothing committed. _(Superseded in part by Amendment 1,
Decision B: every record receives an admission verdict and the error names every refusing index.)_

This is a change for `knowledge.upsert_domains`, not a preservation: today that handler acquires
the writer inside its per-domain loop, so a refusal on the third domain already leaves the first
two committed. `atomic=true` means what it says, and the implementing change moves the writer
outside the loop for that mode.

A verb whose own contract already forbids partial commit keeps that contract and refuses
`atomic=false` with a stated reason rather than silently ignoring it. `stream.batch` carrying a
fence is the existing case.

### 4. Which verbs this binds

The rule is on the SHAPE, a verb taking a caller-supplied list of records it writes, not on a
fixed list. The census below is what that shape selects today, produced by reading the pack
parameter structs for caller-supplied record lists (`grep -rn --include='*.rs' 'pub [a-z_]*:
Vec<' crates/khive-pack-*/src`) and checking each against the registered verb table; it is a
census at one revision, not part of the rule.

| Verb                       | Record list | Today                                                  | Under this ADR                            |
| -------------------------- | ----------- | ------------------------------------------------------ | ----------------------------------------- |
| `knowledge.upsert_atoms`   | `atoms`     | aborts on the first refusal, nothing written           | partial by default, `atomic` opt-in       |
| `knowledge.upsert_domains` | `domains`   | aborts on the first refusal, earlier domains committed | partial by default, `atomic` opt-in (§3)  |
| `create(items=...)`        | `items`     | `atomic` flag, default `true`, per-index errors        | unchanged; a precedent with its own shape |
| `stream.batch`             | `ops`       | `atomic` flag, default `fence.is_some()`               | unchanged; the precedent                  |

_(Amendment 1, Decision A: both knowledge verbs default to `atomic=true`, with partial commit
opt-in through `atomic=false`.)_

`knowledge.import` is **not** bound. Its input is a filesystem path, not a record list, and
ADR-048 already requires discovery, parsing, validation and secret-scan failures to abort before
the first atom write. That contract stands.

The two precedents keep their defaults. `stream.batch` derives its default from the fence, and a
fenced call with no explicit `atomic` stays atomic. `create(items=...)` stays atomic by default.
This ADR's `false` default applies to the two verbs that adopt the contract, so that a caller of
those verbs who today receives nothing on a refusal begins receiving the records that passed.
_(Superseded by Amendment 1, Decision A.)_

A verb added later that takes a record list adopts this contract at the point it is written. A
verb whose list is of _identifiers to read or delete_ rather than records to write is out of
scope: this is about durability of caller-supplied content.

### 5. The refusal names its record even under `atomic=true`

Independent of the disposition, a per-record validation failure inside a batch loop carries the
record's position and field. This is the half that is true regardless of which way §1 had been
decided, and it lands first: an all-or-nothing batch whose single error cannot be located is
unusable whether or not partial commit exists.

### 6. A storage failure names what committed, and never claims to know an unknown outcome

Under the default disposition each passing record commits in its own transaction. A store failure
on record `i` does not discard the records already durable, and the call says so rather than
failing the op: it returns `status: "incomplete"`, `committed` naming every durable record, and

```json
"stopped": { "index": 2, "reason": "...", "disposition": "not_written" | "unknown" }
```

No record after `i` is attempted. `disposition` is `not_written` when the writer reports a
definite failure and `unknown` when it reports an indeterminate one — the storage layer's existing
`SideEffectsUnknown` writer state, which means the write may or may not have committed.

Under `atomic=true` there is one transaction. A definite storage failure commits nothing, and the
op fails. **An indeterminate outcome is not converted into "nothing committed":** the op fails
with the runtime's existing unknown-side-effects class, which already states that the durability
of that transaction is unresolved.

In both modes an `unknown` disposition is a caller obligation, not a retry the verb performs. The
verb never re-reads and never retries on the caller's behalf, because a blind retry of a batch
whose first attempt may have committed is a duplicate write. The caller re-reads the records it
sent, by their own identity, before deciding.

### 7. A partial result is a success the caller must not mistake for a full one

A partial disposition is a successful op, so the hazard moves: a caller that checks only `ok`
after a 97-record upsert now believes 97 landed when 90 did. Three bindings close that gap without
edits to existing callers _(corrected by Amendment 1, Corrections and Decision D: the bindings make a
partial outcome visible but do not change what a caller reading only `ok` or `succeeded` does)_:

- `kkernel exec --strict` treats a result whose `status` is `partial` or `incomplete` as a failed
  op for its exit code, exactly as it treats `ok: false`. Every strict script keeps the guarantee
  it had.
- The verbose presentation and the MCP summary line print the refused count beside the committed
  count for a partial result, never the ids alone; an `incomplete` result additionally prints the
  stopped index and its disposition.
- The request-level `summary` counts a partial or incomplete op in a field of its own rather than
  silently inside `succeeded`: `summary` gains `partial`, and the request `status` reports it.
  The request level has its own vocabulary, `success` | `partial` | `aborted`, and `ok` is not a
  value in it; `ok` is the op result body's field, defined above. Today the request status is
  derived as `success` unless some op failed or aborted, so a request whose every op succeeded
  partially reports `success`. This binding extends that derivation by one clause: a non-zero
  `summary.partial` makes the request status `partial` as well. This binding is the one that reaches agents. A partial op carries `ok: true`, so
  without it a reader of `summary: {total: 1, succeeded: 1, failed: 0}` sees an unqualified success
  for a call that refused seven records, and `summary` is the field agents read first.

## Consequences

A caller that today treats any error from a batched write as "nothing was written" becomes wrong
the moment §1 ships, which is why `atomic` exists and why `status` is explicit rather than
inferred from the presence of an error. Callers wanting the old guarantee name it.

The verbs in §4 that adopt the contract grow a parameter and a response shape. Their existing
tests assert the all-or-nothing behaviour, so each adopting change carries both arms: the same
input under `atomic=true` must still refuse everything, and under the default must commit the
passing records and name the rest.

The partial path writes records after a refusal has already been found, so the writer is held
across records that the atomic path would never have reached. The existing per-verb list bounds
(5000 atoms, 1000 batch members) are what keep that bounded; this ADR adds no new bound and
changes none. _(Corrected by Amendment 1, Corrections: the 1000 bound belongs to bulk `create`;
`knowledge.upsert_domains` has no list bound at this revision.)_

## Acceptance

Stated before implementation, per verb the census binds. _(Arms 1, 2, 3, 5, 7, 8, 9, 10 and 13 are
revised and arms 14 to 16 added by Amendment 1, Acceptance.)_

1. A two-record batch whose second record refuses commits the first, returns `status: "partial"`,
   `committed` naming index 0 with its id and `refused` naming index 1 with its field.
2. The same call with `atomic=true` writes nothing and fails the op; the error's `details` name
   index 1 and its field.
3. No substring of the matched value appears anywhere in the refusal object or the error, asserted
   against a fixture whose **slug** is the credential-shaped value, so that echoing the slug would
   fail the arm.
4. A batch in which every record passes returns `status: "ok"` and an empty `refused`.
5. A batch in which every record refuses returns `status: "partial"`, empty `committed`, and one
   refusal entry per record, not one entry for the first.
6. Two records carrying the same slug, both refusing, produce two refusal entries with distinct
   indices.
7. `knowledge.upsert_domains` with `atomic=true`, two domains, the second refusing: the first is
   not present in the store afterwards. This arm fails against today's handler.
8. A storage failure injected on the second record of a three-record default-disposition call
   returns `status: "incomplete"` with `committed` naming index 0, `stopped.index` 1, and the
   third record absent from the store. Two arms by writer outcome: a definite failure gives
   `stopped.disposition: "not_written"`, an indeterminate one gives `"unknown"`, and neither arm
   is an op-level error.
9. Mutation control: restoring the `?` that returns on the first refusal must fail arms 1, 5 and 6
   and leave arm 2 green, since arm 2 is the behaviour being restored.
10. `kkernel exec --strict` over a call whose result is `status: "partial"` exits non-zero, and
    likewise for `"incomplete"`; the control is the same call with every record passing
    (`status: "ok"`), which exits zero.
11. The verbose presentation and the MCP summary line for a partial result carry the refused count
    beside the committed count; a fixture with two committed and one refused renders both numbers.
12. A storage failure injected on the LAST record of a three-record call returns
    `status: "incomplete"`, not `partial`, with `stopped.index` 2 and nothing left unattempted.
    This is the arm the §2 definition exists for: a remainder-based reading returns `partial` here
    and the caller skips the re-read that an indeterminate outcome requires.
13. A one-op request whose result carries `status: "partial"` returns a request-level `summary`
    with `partial: 1` and a request `status` of `"partial"`; the control is the same call with
    every record committing, which returns `partial: 0` and a request `status` of `"success"`.
    Both values are read at the request envelope, not at the op body, and `"ok"` appears at
    neither point of this arm. The arm additionally
    asserts that `succeeded` and `failed` are IDENTICAL across the two calls, because that is the
    reading which cannot tell them apart today and the reason this binding exists.

## Amendment 1 (2026-10-23) -- Atomic by default, complete refusal lists, explicit partial writes

**Status**: Proposed 2026-10-23. This draft requires specification approval before dependent
implementation lands. It does not change the status of the base ADR.

**Addresses**: [#1751](https://github.com/ohdearquant/khive/issues/1751).

**Trigger.** The base text makes partial commit the default for `knowledge.upsert_atoms` and
`knowledge.upsert_domains` and states that three bindings close the hazard "without edits to existing
callers" (§7). A partial result still carries `ok: true`, and the request summary counts any op with
`ok: true` as succeeded (`crates/khive-mcp/src/server/batch_execution.rs:160-170`). A caller that reads
only `ok`, or only `summary.succeeded` and `summary.failed`, follows its success branch unchanged when
seven of 97 records refuse. The bindings make the outcome visible to callers that read the new fields;
they do not change the control flow of callers that do not.

Three further facts at this revision bear on the default:

- Atom writes already validate every submission before any write and refuse the whole call
  (`crates/khive-pack-knowledge/src/knowledge/crud.rs:389-406`). The refusal carries the first failure
  only, plus event receipts for existing targets
  (`crates/khive-pack-knowledge/src/knowledge/refusal.rs:136-182`).
- Atom writes accept `dry_run`, which returns a verdict for every record without writing
  (`crates/khive-pack-knowledge/src/knowledge/crud.rs:375-383`;
  `crates/khive-pack-knowledge/src/knowledge/atom_validation.rs:88-100`). A caller can already find
  every refusing record and resend the passing ones in two calls, so stopping batching is not the only
  recovery path the Context describes.
- Domain writes are not all-or-nothing today. The handler validates, looks up and writes each domain
  inside its loop, so a refusal on a later domain leaves the earlier ones committed
  (`crates/khive-pack-knowledge/src/knowledge/crud.rs:606-636, 730-794`), as §3 already notes.

### Decision A -- Default and scope

`knowledge.upsert_atoms` and `knowledge.upsert_domains` default to `atomic=true`. A caller selects
independent per-record commits explicitly with `atomic=false`.

In atomic mode every record receives an admission verdict before any requested write, and all admitted
writes run in one write transaction. Any deterministic refusal fails the op and leaves the requested
atom and domain data unchanged. Refusal events recorded beside the refusal are outside that guarantee
and are reported separately (Decision B).

For atoms this keeps the shipped refusal behaviour. For domains it is a behaviour change: a call that
today commits the domains before the first refusal commits none of them. Release notes and the verb
help state this change.

`create(items=...)`, `stream.batch` and `knowledge.import` keep their own contracts (§4). A verb added
later that takes a record list declares its disposition explicitly; it does not inherit one from the
shape of its parameters.

### Decision B -- Complete refusal list on an atomic refusal

An atomic admission refusal stays an op error (`ok: false`) and carries a typed `refused` array beside
the error's existing fields, outside `details`:

- one entry for every refusing input index, ordered by index, each with a zero-based `index`, a
  schema-owned `field` and a value-free `reason`;
- `validation_complete: true` only when every record received an admission verdict;
- completeness covers item shape, submission checks and deterministic target or admission checks, and
  requires the first applicable reason per refusing record, not every invalid field in it.

The error keeps the first refusal's classification and its scalar location in `details` for existing
readers. Event receipts (`refusal_events`) stay a separate field with their own meaning: they name
events recorded against existing targets, including bystander records, and are not a list of refusing
records. An interrupted admission reports `validation_complete: false` with the unchecked indices and
the infrastructure error; a caller cannot obtain a passing subset by subtracting the known refusals
from it.

No refusal list is packed into a `details` string or shortened silently. If a carrier has a real size
limit, the response states it and names a lossless way to retrieve the full list, or admission refuses
before any requested write.

The base text concluded that a record list cannot ride an error because `details` is bounded
(`crates/khive-types/src/khive_error.rs:193-200`). Runtime error projection already places a
structured array beside the error fields (`refusal_events`,
`crates/khive-runtime/src/error_projection.rs:141-152`), so a typed sibling array is an extension of an
existing shape, not a widening of `details`.

Illustrative atomic refusal (the new fields are specified here; this is not a captured response):

```json
{
  "ok": false,
  "tool": "knowledge.upsert_atoms",
  "error": {
    "kind": "runtime_error",
    "code": "secret_detected",
    "message": "batch admission refused; no requested atom changes committed",
    "domain_disposition": "not_committed",
    "details": { "index": "1", "field": "content", "reason": "secret_detected" },
    "validation_complete": true,
    "refused": [
      { "index": 1, "field": "content", "reason": "secret_detected" },
      { "index": 6, "field": "slug", "reason": "secret_detected" }
    ]
  }
}
```

### Decision C -- Explicit partial mode and settlement

With `atomic=false` the result contract of §2 and §6 applies unchanged: `status` of `ok`, `partial`
or `incomplete`, `committed`, `refused`, and `stopped` exactly when `status` is `incomplete`. Passing
records commit in input order, one transaction per logical record. A domain row and its mirror stay in
one transaction, so partial mode never splits a domain
(`crates/khive-pack-knowledge/src/knowledge/crud.rs:751-761, 782-791`).

On the first storage failure the call stops, keeps every confirmed earlier commit, and marks the
stopped record `not_written` only on definite evidence that it did not commit, otherwise `unknown`.
In atomic mode a storage failure stays an op error, and definite non-commit and unknown outcomes stay
distinguishable. Neither mode retries or re-reads on the caller's behalf.

The storage layer distinguishes `NotStarted`, `TransactionRolledBack` and `SideEffectsUnknown`
(`crates/khive-storage/src/error.rs:20-31`), but the knowledge handlers map storage errors to a string
(`crates/khive-pack-knowledge/src/knowledge/util.rs:110-112`;
`crates/khive-runtime/src/error.rs:988-990`). Carrying the typed outcome through to the response is
required work for both modes; settlement is never inferred by parsing error text.

Successful responses keep today's `created`, `updated` and `total` fields
(`crates/khive-pack-knowledge/src/knowledge/crud.rs:581-585, 796-800`) beside the new disposition
fields. `total` is the submitted count; `created` and `updated` count confirmed commits only.

### Decision D -- Completion signals and migration

All three §7 bindings ship before `atomic=false` is accepted. `summary.partial` counts successful ops
whose records were not all accepted and is a subset of `summary.succeeded`, so the existing
`succeeded` and `failed` accounting is unchanged and fully completed ops are `succeeded - partial`.
A non-zero `summary.partial` prevents request `status: "success"`. `kkernel exec --strict` rejects any
failed, aborted, partial or incomplete op whatever the output format; the current check returns early
for non-JSON output (`crates/kkernel/src/exec/run.rs:284-312`), so that path needs the same rule or
must refuse the combination before execution.

These bindings make a partial outcome visible. They do not make a caller that reads only `ok`, or only
`succeeded` and `failed`, safe for partial writes; that is why partial writes are opt-in. The
classification is registered for these two verb contracts, not inferred from any result that happens
to contain a `status` field. An all-refused call in explicit partial mode is `partial` with an empty
`committed`, as arm 5 of the base text specifies; `--strict` rejects it.

### Decision E -- Delivery order

1. Complete refusal lists and typed storage settlement for the atomic path.
2. The two write modes and their completion bindings, together. The domain default change ships in the
   release that declares it.
3. `atomic=false` is not exposed before the strict, presentation and summary consumers can represent a
   partial outcome.

Migration notes show atomic refusal handling, one-call partial salvage with `atomic=false`, and
reconciliation after an `unknown` outcome. The parameter structs carry no `atomic` field today
(`crates/khive-pack-knowledge/src/knowledge/schema.rs:125-136, 185-189`), so both verbs gain it in step 2.

### Corrections

These change no decision; each is marked where the base text says it.

- §7 says the three bindings need no edits to existing callers. They change what a caller can see,
  not what an existing caller does with `ok` or `succeeded`; see the Trigger.
- Consequences cites "1000 batch members" as an existing list bound for these verbs. That bound
  belongs to bulk `create` (`crates/khive-pack-kg/src/handlers/create.rs:859-864`).
  `knowledge.upsert_domains` checks only that its list is non-empty
  (`crates/khive-pack-knowledge/src/knowledge/crud.rs:594-598`); `knowledge.upsert_atoms` is bounded
  at 5000 (`crates/khive-pack-knowledge/src/knowledge/crud.rs:369-373`).

### Acceptance

The base arms keep their numbers. The arms below replace or extend them; unlisted arms (3, 4, 6, 11,
12) stand, with arm 3 extended as stated.

| Arm | Requirement                                                                                                                                                                                                                                                                                                                                                            |
| --- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1   | The base two-record case passes `atomic=false`. The same payload with `atomic` omitted writes nothing and returns the complete `refused` list.                                                                                                                                                                                                                         |
| 2   | Omitted and explicit `atomic=true`: at least two refusing records separated by a passing one. `error.refused` names every refusing index, `validation_complete` is `true`, and `details` keeps the first refusal's location.                                                                                                                                           |
| 3   | The no-echo requirement covers the atomic error, the partial result, the summary, nested diagnostic fields and receipts. The fixture keeps the credential-shaped slug and adds several refusing fields and records. The assertion checks the matched value, its distinctive segments and any input-derived preview, not every shared character with fixed field names. |
| 5   | Omitted or `true`: `ok: false`, a complete `error.refused`, no requested data changed. `atomic=false`: `status: "partial"`, empty `committed`, one refusal per record.                                                                                                                                                                                                 |
| 7   | Omitted and `true`: a later domain refusal leaves an earlier domain and its mirror unchanged, including pre-existing content. `atomic=false` is the control and commits the passing domain as one record.                                                                                                                                                              |
| 8   | The committed-prefix case runs with `atomic=false`, keeping both writer-outcome variants and the untouched suffix. Added: an atomic-mode storage failure distinguishes definite non-commit from unknown for the whole transaction.                                                                                                                                     |
| 9   | Mutation controls: an early return in partial processing fails arms 1, 5 and 6; truncating atomic diagnosis to the first record fails arms 2, 5 and 6.                                                                                                                                                                                                                 |
| 10  | `--strict` exits non-zero for explicit partial and incomplete results and for atomic failures, and zero for full success, on each supported output format or by refusing an unsupported combination before execution.                                                                                                                                                  |
| 13  | Atomic refusal reads `succeeded: 0, failed: 1, partial: 0`; full success `succeeded: 1, failed: 0, partial: 0`; explicit partial `succeeded: 1, failed: 0, partial: 1`. `partial` is a subset of `succeeded`.                                                                                                                                                          |
| 14  | More than eight refusing records round-trip in full; mixed submission and target refusals and a malformed item inside a valid list are all named.                                                                                                                                                                                                                      |
| 15  | An interrupted admission returns `validation_complete: false` with the unchecked indices.                                                                                                                                                                                                                                                                              |
| 16  | Preservation controls: `create(items=...)`, `stream.batch` and `knowledge.import` keep their contracts, and an unrelated result containing `status: "partial"` is not classified by Decision D.                                                                                                                                                                        |

**Open questions.** These are listed, not decided here:

- The admission bound for `knowledge.upsert_domains` and the largest complete refusal list each
  carrier can deliver, settled before step 2 of Decision E.
- An inventory of callers by the fields they read (`ok`, `summary`, the error shape), to size the
  migration notes. Counting mentions of the verb names does not answer this.
- Whether re-reading by identity after an `unknown` outcome is enough to tell which attempt committed
  under concurrent writes; Decision C does not add an idempotency protocol.

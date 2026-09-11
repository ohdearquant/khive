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

### 2. The response names both sides, by position

A partial disposition is a successful op. Its result body is:

```json
{
  "status": "ok" | "partial" | "incomplete",
  "committed": [ { "index": 0, "id": "..." } ],
  "refused":  [ { "index": 1, "field": "content", "reason": "..." } ]
}
```

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
  therefore a result, not an error.

### 3. `atomic=true` is how a caller asks for all-or-nothing

`atomic` is an optional boolean parameter on every verb this ADR binds. Its default is `false`
for the verbs in §4 that adopt this contract; verbs that already carry the flag keep their own
default (§4).

With `atomic=true` the whole list is validated and written in **one write transaction**: the
first refusal aborts the call and nothing is written. The refusal is an op-level error, not a
`partial` result: the op's `ok` is `false`, and the error's `details` carry the same
`{index, field, reason}` object a partial result would have placed in `refused`. There is no
`committed` list on the error, because nothing committed.

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

`knowledge.import` is **not** bound. Its input is a filesystem path, not a record list, and
ADR-048 already requires discovery, parsing, validation and secret-scan failures to abort before
the first atom write. That contract stands.

The two precedents keep their defaults. `stream.batch` derives its default from the fence, and a
fenced call with no explicit `atomic` stays atomic. `create(items=...)` stays atomic by default.
This ADR's `false` default applies to the two verbs that adopt the contract, so that a caller of
those verbs who today receives nothing on a refusal begins receiving the records that passed.

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
after a 97-record upsert now believes 97 landed when 90 did. Two bindings close that gap without
edits to existing callers:

- `kkernel exec --strict` treats a result whose `status` is `partial` or `incomplete` as a failed
  op for its exit code, exactly as it treats `ok: false`. Every strict script keeps the guarantee
  it had.
- The verbose presentation and the MCP summary line print the refused count beside the committed
  count for a partial result, never the ids alone; an `incomplete` result additionally prints the
  stopped index and its disposition.

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
changes none.

## Acceptance

Stated before implementation, per verb the census binds:

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

# ADR-209: Root Policy Overrides, Resource Grants, and Verifiable Flow History

- **Status:** Proposed
- **Date:** 2026-10-22
- **Grounding commit:** `46cf6ddfe2d9561c49b620b8caa84765b152711f`
- **Depends on:** [ADR-127](ADR-127-authenticated-actor-and-grant-primitive.md) and
  [ADR-143](ADR-143-store-held-caller-grants.md) for the shared grant contract; additionally
  [ADR-192](ADR-192-credential-and-producer-seams.md) and the M2 prerequisites of
  [ADR-193](ADR-193-charter-runs.md) for protected enforcement.
- **Amends, when accepted:** ADR-193 D3, D4, D6, D7, D9, D11, state machine and acceptance;
  [ADR-208](ADR-208-charter-workflows.md) D3, D6, D7, acceptance and override limit; ADR-182
  Amendment 2 item 5, Amendment 6 item 3 and Amendment 7. The M1/M2 boundary remains.
- **Relates to:** [ADR-007](ADR-007-namespace.md),
  [ADR-041](ADR-041-event-provenance-projection.md),
  [ADR-133](ADR-133-incidental-writes-off-the-request-hot-path.md),
  [ADR-162](ADR-162-unified-event-plane-ownership.md),
  [ADR-164](ADR-164-event-sink-boundary.md),
  [ADR-168](ADR-168-event-retention-classes.md),
  [ADR-170](ADR-170-events-daemon-split.md),
  [ADR-180](ADR-180-tool-pack.md),
  [ADR-181](ADR-181-exec-verb-sandboxed-run.md),
  [ADR-182](ADR-182-git-dev-loop-verbs.md),
  [ADR-206](ADR-206-crate-performance-guarantees.md), and
  [ADR-207](ADR-207-runtime-actor-registry.md).

## Requirement

This record serves four product requirements. Each maps to the decision that delivers it, and each
departure from the requirement as stated is named beside it.

1. **The root human can break glass at any time.** D1: root may request a policy exception whenever an
   identified policy hold exists, with no waiting period. Departure: the exception binds to an
   enrolled root key, so break glass exists only once root enrollment exists (D2), and in M1 it
   releases recorded workflow progress, not effects, except an enrolled merge (D4, D11).
2. **Special resources are reached through just-in-time grants.** D3 and D4: an exact, single-use,
   expiring and revocable grant per resource action, through one shared grant service. Departure:
   the merge adapter comes first; other resource kinds are staged to S3 (D11).
3. **Events are indexed by the flow or run they belong to.** D6: typed flow attribution, indexes,
   and one bounded history query.
4. **Recorded history is hash-chained.** D7: one integrity chain per physical store, including
   authority commitments; D8 states which history independent checkpoints make tamper-evident.

## Context

Charters record policy decisions and workflow progress. ADR-193 distinguishes hard, soft and
advisory requirements, and reserves soft overrides for a human-issued grant consumed with an
admission. ADR-208 supplies declared workflows over registered phrases without adding a predicate
language. Neither a charter role nor a parsed `break_glass` clause establishes human identity.

The current deployment places the human operator and automated processes under the same OS user. A
process is the execution principal. Files readable by that user, local actor labels, terminal origin
and peer UID do not independently prove that the human authorized an action. A convention against
opening a database is not access control. ADR-127 and ADR-143 describe an authenticated grant
primitive, but their implementation-status sections say the complete primitive is not yet shipped.

Root needs an explicit way to accept a policy exception and to authorize short-lived access to a
special resource. These are different decisions: granting an ordinary operation need not waive any
policy. Authentication, exact-action binding and single consumption remain necessary even when the
human intends to waive a hard requirement. Allowing an exception to waive its own validity would
make the exception forgeable.

The event history is useful but is not a universal authority ledger. ADR-193 records evidence and
state transitions in one domain transaction. ADR-170 separates the audit lane from the domain store,
and observability delivery can be lossy. `events` has session and aggregate attribution but no typed
flow/run identity or hash-chain sequence at the grounding commit. A chain over recorded events can
reveal changes to committed history; it cannot prove that every action was observed or that a report
was true. Existing data cannot acquire a historical authenticity guarantee merely by being hashed
now.

## Decision

### D1. Separate policy exceptions, resource authority and protocol validity

The system distinguishes immutable policy evaluations, resource permission, root policy exceptions,
and protocol validity. A root policy exception accepts named hard, soft or unknown policy outcomes
for one exact **phase release** or **resource action**. It does not turn those outcomes into passes.
Advisory predicates remain nonblocking. The requirement inventory and the original evaluation stay
recorded, together with the root authorizer, executing principal, reason and proof reference.

An ordinary resource grant supplies permission for its action, subject to applicable policy. A root
exception discharges only its enumerated policy predicates and supplies no implicit resource rights.
An approval bundle may contain both purposes, but each has its own identifier, exact scope and
single consumption. A phase release and a later effect likewise require separate approvals; neither
can spend the other's proof. A later action always evaluates its own current requirements.

Root is a product authorization identity, not OS root, a process label or a charter role. Its
exception path is independent of the optional `break_glass` stanza. That stanza cannot enroll root,
confer root discretion on a role, or suppress an exception record through a false `marks_degraded`
field. The false value is the compiled default when the keyword is omitted; this is not new
`marks_degraded = false` source syntax.

The compiled, versioned action/gate registry classifies policy predicates and protocol invariants.
Requests, charter text and mutable notes cannot reclassify them. Root MAY waive a named policy
predicate. Root MAY NOT waive:

- Verification against the enrolled root key and the deployment's declared authority mode.
- A valid signature over a supported, complete, canonical descriptor with the correct purpose,
  deployment/tenant, executor, action, resource, generation and current policy snapshot.
- Validity interval, revocation and single consumption; protected epoch/replay checks where enabled.
- Valid authority state, supported adapter/input schemas, exact resource preconditions and resource
  authorization. The local review predicates in D4 are policy, not adapter validity.
- The durable evaluation, exception, consumption and receipt required at that transition/effect seam.

Completeness of the mechanism remains mandatory: the subject, authority inputs and complete list of
applicable predicates must be identifiable. Absence of a policy fact instead produces an explicit
`unknown`, which root may acknowledge and waive. An unavailable producer needed only for that waived
fact does not veto the exception. It supplies no evidence; an unauthenticated report is not upgraded
into an authenticated observation. Authentication of the exception and every unwaived fact still
meets the active mode's requirements. Corrupt authority state cannot be relabeled missing policy
evidence.

#### Recording-phase release

Add `charter.release(run_id, approval_id, request_id)`, a domain transition through the shared grant
service, available in M1 as well as M2. The approval id resolves through that service to the signed
release descriptor; the authenticated caller must match its executor. The caller-scoped request id
uses the existing exact-payload command replay contract. Unknown parameters refuse. It changes
recorded procedure progress and performs no external I/O or action admission. Its root exception
descriptor binds `target_kind = phase_release`, policy domain, flow/run, subject generation,
definition/registry digests, current phase **visit**, run/phase revisions, full evaluation snapshot,
every held condition, exact disposition and one declared successor route. The service derives these
values from current state; caller-supplied outcomes are not authoritative.

The release path records the ordinary evaluator's raw result plus a complete diagnostic inventory of
the visit's requirements and holds; ordinary evaluation semantics are unchanged. It persists both
before applying the disposition in the same transaction. A held phase stays held unless every
remaining hold is expressly covered. The exception transition may accept a failed requirement or
missing policy observation, acknowledge a missing trigger/routing observation, accept a failed
action result, or skip a recorded action that is proven not to have started. The record
distinguishes `failure_accepted`, `evidence_unknown` and `not_performed`; it never invents a
trigger, passing observation, action result or successful child completion. Missing evidence alone
is not proof of non-execution.

A `perform` starts its child during the same command that enters its phase. An expected child that
never existed is not a supported skip: missing child creation/state is a mechanism inconsistency.
Root cannot erase an existing child by calling it unstarted. To omit its remaining work, first
record a valid cancellation of that child, then accept its cancelled disposition. A live child must
reach a recorded terminal disposition; an admitted, dispatching or uncertain external attempt must
be reconciled before release. Cancelling a recording run does not assert that an external process
stopped. A terminal failed/cancelled child can be accepted as such. An action with missing results
and possible execution remains unresolved until reconciliation. An unsupported or corrupt child/run
reference is a mechanism error, not a policy exception.

The descriptor selects only a successor already declared by this phase, including its declared
terminal route. An unknown `when` may be bypassed by explicit route selection, without asserting its
label was observed. Topology, visit bounds, live definition and current generation checks remain
mandatory. An existing matching `when` route may already release a failed requirement under ordinary
semantics; such progression needs no exception. No release resets a visit or changes the definition.

One atomic unit verifies the proof, compares revisions and the current semantic snapshot, consumes
its approval, appends the raw evaluation and `phase_released_by_exception` record, records command
replay, and advances the phase projection. Any failure commits none of these. An identical replay
returns the original result; a concurrent loser or conflicting replay cannot release another visit.
The response includes the transition id, original evaluation id, authorizer/executor, proof and
mode, `action_admitted = false`, and `completion_disposition = exception`. Terminal run history and
any parent completion observation carry the release references and this disposition. A downstream
consumer must not treat procedure completion by exception as evidence that its omitted work
occurred.

In M1 the run remains `assurance = recording_only` and `charter.claim` remains unavailable/refusing.
Under M2 this same transition can make a successor phase reachable, but the subsequent claim must
re-evaluate all action requirements, including those previously waived. It records the predecessor
release references as history, never as a reusable waiver. If the action still needs an exception,
root issues a new exact-action exception over its fresh evaluation; it is consumed with the claim.

Root may request an exception whenever an identified policy hold exists. No waiting period, extra
policy approval or stanza is required. An unavailable signer/executor, invalid proof or unresolved
effect is not a policy hold that this mechanism can override. External recovery is recorded as an
external action, never converted retrospectively into an admission. The product questions concerning
delegated exception discretion and later review remain open below.

### D2. Exact descriptors first; protected custody before protected assurance

Both authority modes use the ADR-127 signature and shared-grant contract. S1 validates a signature
against explicit local root enrollment and authenticates the executing principal through the local
service. It records `authority_mode = cooperative_local`. That is exact-byte verification against
local enrollment, **not proof of human-only control against another process sharing the OS user**.
Root keys, enrollment, clocks, code, consumption state and credentials remain replaceable/readable
within that trust boundary. A root label or stanza alone is insufficient even on this path.

S2-A uses `authority_mode = protected`. Its preferred architecture is an independent signing device
with a protected key, trusted display of the complete material descriptor and explicit human
confirmation per approval. The resource verifier pins enrollment outside the requester's write
authority. Display and signature use the same canonical descriptor; a truncated name or opaque hash
alone is insufficient to confirm a destination, revision or equivalent material term.

A local hardware-backed arrangement may substitute only after demonstrating protected enrollment,
invocation controls, exact transaction display, per-approval confirmation and no weaker cached or
fallback path. Non-exportability alone establishes none of those remaining properties. This is a
custody contract, not an assertion about a particular hardware API.

The signed descriptor includes:

- Protocol/version, authority mode, purpose (`resource_grant` or `root_policy_exception`), deployment/
  tenant, root-key id, issuer authority revision and recovery epoch.
- Unique approval id/nonce, authenticated executor binding, not-before, expiry and a reason.
- Target kind (`phase_release` or `resource_action`), stable resource identity, registered action and
  adapter version, all material parameters, preconditions and bounds. No wildcard or free-text scope.
- Where associated with a charter: flow, run, phase/visit, subject revision/generation, definition
  and gate-registry digests, referenced evaluation and semantic policy snapshot, exact waived gate
  instances and original outcomes. A release also binds the dispositions and successor route in D1.

Canonicalization is closed and versioned per action kind. Unknown/duplicate fields, unsupported
versions and ambiguous encodings refuse consumption. The signature covers actual data, not an
expandable URI or mutable title. The verifier derives the descriptor from the actual operation.
Current checks must reproduce the pinned semantic policy inputs/outcomes and meet freshness bounds;
material changes require fresh approval. Observation time alone is not a semantic change.

Protected root enrollment and recovery are privileged out-of-band setup. Replacement keys require
the existing protected authority or a pre-enrolled recovery mechanism. Writable local enrollment, a
terminal flag or a database restore cannot bootstrap protected root. Protected executor state
preserves revocations and epochs across local restoration. S2-A refuses when these facilities are
unavailable; it never falls back to S1. S1 proofs cannot be promoted to protected authority and must
be reissued after activation against current state.

Persist the human approval subject, execution principal, proof reference and authority mode
separately. ADR-207's proposed sealed operation context is a compatible integration point, not proof
that the protected verification path is implemented.

### D3. Ordinary JIT grants authorize one resource action

A root-issued JIT resource grant authorizes one exact registered action for one authenticated
executor until expiry, subject to current policy. It is single-use and non-redelegable, and confers
no grant-issuance right, root-key management, renewal power or policy waiver. Existing tool grants
retain their existing semantics. S1 implements this contract for the merge adapter in D4; it is a
local cooperative authorization, not M2 charter enforcement or same-user containment.

A process may transport and execute root's already signed exception. It cannot select a new gate,
subject or action or decide a later exception. This version implements no delegation of
discretionary exception authority; whether such delegation should be supported remains an open
product question, not a decision that it must never exist.

Expiry and revocation are checked at consumption. Revocation ordered before consumption refuses use;
revocation after consumption does not undo an irreversible effect. Adapters state whether
cancellation of unsent work is supported. Deferred new effects re-admit against current authority;
remembered grant ids and captured contexts are not fresh permission. Returning an old receipt never
authorizes a resend.

Executor binding is credential-derived, not a caller-chosen label. Theft of a proof alone does not
authorize another principal. S1 does not protect the bound executor's readable credentials from a
same-user peer; that peer can impersonate it and race the permitted action. Even in S2-A, compromise
of the intended executor can permit the one approved action. Single use bounds authority, not
intent.

### D4. One shared grant service; merge is the first resource adapter

Implement the minimum ADR-127/143 shared-grant path needed for phase release and `git.pr_merge`. Do
not create a charter-local issuer, revocation table or parallel consumption primitive. Local
cooperative operation and protected operation use the same logical interface with explicit mode and
custody differences. ADR-180 may provide request/discovery UI; its tool patterns and free-text
`scope` never become exact resource rights. Ordinary dispatch/tool authorization remains required.

S1 adds explicit per-repository enrollment in the cooperative exact-action merge path. On that path,
`git.pr_merge` requires a resource grant, evaluates its local policy predicates, and consumes the
applicable resource grant and optional root exception into one attempt before sending. This is
adapter-local authorization, **not `charter.claim`**, and does not certify workflow compliance.
Existing unenrolled calls keep their behavior. A proof cannot enroll a repository or switch modes. A
repository cannot expose an unenforced alternative route and claim S2-A protection.

The merge descriptor binds the immutable destination/repository and pull-request identities,
expected head and target/base identity, method, subject/body digests, visibility, authenticated
executor and platform account, plus the applicable generation and policy/registry snapshot. At S2-A,
enrolled merges additionally use `charter.claim` and the designated protected executor credential
under ADR-193 D7. Phase progress by itself supplies no merge permission.

The signed target is an admission-time term. At its authenticated subject read the adapter compares
the signed target ref and base repository with the pull request's `base.ref` and
`base.repo.full_name`; a mismatch refuses as `target_mismatch`, with the same consumption rules as an
expected-head mismatch, and no exception waives it. The platform merge request carries no target
precondition: its only resource-side comparison is `sha = expected_head`. A retarget that keeps the
head and lands after that read is therefore outside what S1 or S2-A prevents. This is ADR-193's
snapshot limit for the remote boundary, applied to the target. Reconciliation reads the merged pull
request's base, and a merge whose base differs from the signed target records `merged_target_mismatch`:
a distinct outcome, neither success nor an unresolved attempt, and the consumed approval stays
consumed. Binding the effect itself to the signed target needs a resource-side target precondition, or
a control that prevents retargeting for the attempt's duration, shown before the adapter claims it.

#### ADR-182 local review and separation predicates

Amend ADR-182 Amendment 2 item 5, Amendment 6 item 3 and Amendment 7 together. Before refusal or
consumption, the adapter evaluates the following versioned policy instances rather than
short-circuiting at the first review veto. These identifiers are registered contract additions:

| Policy instance                              | Existing local condition                                                                                              | Relation to charter policy                                                                                                             | Root exception effect                                                                         |
| -------------------------------------------- | --------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------- |
| `git.pr_merge.review_decision/v1`            | Aggregate platform review decision is `APPROVED`                                                                      | Platform-decision facet of ADR-193 `review_at_head/v1`                                                                                 | May accept its non-approved or explicitly unknown value                                       |
| `git.pr_merge.independent_review_at_head/v1` | `require_review` finds effective approval at `expected_head` by an account distinct from author and known last pusher | Current-head facet of `review_at_head/v1` and account-separation facets of `no_self_approval/v1`; not the whole of either charter gate | May accept missing, stale, disqualified or unknown review evidence without inventing a review |
| `git.pr_merge.merger_not_opener/v1`          | Configured `merge_by_opener` veto, including login and opening-receipt evidence                                       | Separate ADR-182 merge-executor policy; no implicit equivalence to a reviewer gate                                                     | May accept that exact separation failure                                                      |
| `git.pr_merge.merger_not_last_pusher/v1`     | Configured `merge_by_last_pusher` veto                                                                                | Separate ADR-182 merge-executor policy; no implicit equivalence to a reviewer gate                                                     | May accept that exact separation failure                                                      |

An absent configured dispatch predicate is not applicable, under the registry's fixed rule. The
existing unknown-last-pusher behavior remains explicit; it is not silently promoted to proven
independence. A charter may impose an additional independently named policy requirement.

The adapter accepts only the shared service's verified, exact-action exception result bound to its
attempt and descriptor. It does not accept `root=true`, a role, free-text scope, a caller-supplied
list of waived error codes or an unverified admission id. The versioned registry includes both
charter and local instances in one policy inventory and records the facet relationships above. The
signed exception must enumerate every holding instance at both layers, including distinct
constraints within a broader charter gate; a similar name or a narrower local waiver never waives
that entire gate. Evaluations may share immutable source snapshots, but unlisted or additional
failing predicates still refuse. One exception consumption binds the whole enumerated set to this
attempt. A phase-release proof cannot satisfy this mapping. Rechecking at the effect seam neither
consumes a second approval nor renews one.

An empty review list with an aggregate `APPROVED` value still needs the independent-review
exception. A non-approved aggregate plus no independent review needs both exceptions. A missing
policy producer or bounded unavailable review read yields an explicitly identified unknown only
where the current subject and authority can still be established. Malformed authority records or an
unknown action subject are mechanism errors. Blanket suppression of `require_review` errors is
forbidden.

Mandatory even with these exceptions: current repository/PR identity, actor/credential binding,
resource grant and dispatch authorization, supported method and valid open state, fresh
expected-head and signed target comparison, pre-send receipt and result reconciliation. A received
review-decision response must still name `expected_head`; its mismatch cannot be waived. If that
policy read is unavailable, a fresh authenticated subject read must independently establish the
exact current head. The platform send's `sha = expected_head` remains mandatory in every case. The
adapter records the raw review decision, review/separation outcomes and consumed proof alongside the
attempt. It sends the ordinary merge request, with no administrator flag, force operation, delayed
auto-merge or branch-protection edit. `git.pr_review` does not gain a self-approval exception. The
remote service's own branch protection or authorization can still refuse the merge; the receipt
records that refusal and the consumed approvals remain consumed.

#### Additional resource adapters (S3)

The common contract permits these later adapters; none is required for S1:

| Kind                         | Material subject fields                                                                                       | Effect boundary                                               |
| ---------------------------- | ------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------- |
| Other repository publication | Destination/repository, ref, expected old and new values, operation and visibility                            | Exact remote preconditions and pre-send receipt               |
| Store mutation               | Store identity, registered mutation, exact objects or bounded versioned selector, fields/values and preimages | The mutation transaction; never raw handles or arbitrary SQL  |
| Payment                      | Provider, payer/payee identities, amount/currency, operation and provider idempotency key                     | Provider acceptance plus reconciliation of its durable result |
| Credential use               | Credential identity, exact operation, destination and permitted scope                                         | Broker performs the use without releasing secret material     |

Raw credential export, if later supported, is a distinct disclosure action. Subsequent uses of an
exported secret cannot be promised single-use or revocable. Receipts contain non-secret terms and
proof references, never credentials. Each adapter needs its own positive, refusal and crash cases.

### D5. Consumption follows the real transaction boundary

For an effect in one authority store, fresh verification, grant consumption, domain mutation or
phase release, replay record and immutable authority commitment commit in one transaction. A failed
transaction consumes nothing and publishes no chain head. Two concurrent consumers cannot both win.
These local guarantees apply to the new S1 grant/release path; they do not depend on a witness.

For an external effect:

1. Validate current authority and exact preconditions. Atomically consume the applicable approvals
   into one attempt and persist its complete intent/pre-send receipt and integrity commitment. A
   lost response is resolved by lookup of that same attempt, not creation of a fresh attempt.
2. In S1 the cooperative executor dispatches that attempt through the ordinary adapter. In S2-A the
   protected executor first reserves that approval/attempt in independently durable anti-replay state,
   rechecking signature, epoch, freshness and exact action. Every send uses the adapter's same
   resource idempotency key or compare-and-swap condition. An existing reservation returns its known
   state or requires reconciliation. A reservation is not proof of remote execution.
3. Append the remote receipt/result. Timeout, crash or unavailable response leaves the attempt
   unresolved. Reconcile against resource and executor records before any further effect.

There is no atomic transaction across SQLite, executor and remote service. Partial progress is a
recorded state, never permission to reset the grant. An uncertain send cannot be reopened or retried
with a new approval id to escape uncertainty. Supported idempotent retries follow the adapter's
protocol and create no new authority; the merge adapter retains ADR-182's no-blind-resend contract.
Even proven non-execution does not restore a single-use approval: another attempt needs new
authority.

A provider without safe idempotency or reliable reconciliation cannot support an automatic
exactly-once claim. Its adapter disables automatic retries after uncertainty and exposes manual
resolution. Exact ref CAS does not defeat local restore/replay or an A-to-B-to-A resource
transition. S1 expressly offers no such hostile-rollback protection.

S2-A keeps consumed nonces, current epoch, revocations and effect credentials beyond the requesting
OS user's authority, and closes alternate unguarded effect routes. Restoring the local database
cannot restore executor authority. Periodic hash anchoring is not anti-replay state. If the
requesting user can rewrite the verifier/consumption state or read the effect credential, the
deployment cannot claim protected enforcement regardless of approval cryptography.

### D6. Typed flow attribution and one bounded history query

Add nullable typed `flow_id` and `run_id` to the versioned event envelope and authority receipts.
`flow_id` identifies a workflow instance across its child runs; `run_id` identifies the particular
run. These are stable opaque identifiers, not principal identities. The existing charter parent-run
relationship remains immutable. A root run receives a flow identity; registered child creation
carries it forward. A separately initiated flow has a new identity even if it processes the same
subject.

Keep session id, operation execution id, parser operation index, caller retry id and action attempt
id distinct. A retried execution is not collapsed merely because it reuses a request id. Preserve
existing `op_index`/`ref_resolution` semantics from ADR-133. Add explicit causal references for
queued and resumed work. The runtime resolves authoritative run association from registered context;
unvalidated incoming correlation is labeled as a claim, not authenticated association.

Capture attribution before an event crosses an asynchronous boundary, and explicitly propagate it to
nested operations, child runs, approvals, denials, sends, reconciliations and delayed completions.
Task locals do not automatically propagate into spawned work. Background work outside a flow remains
unassociated; it is not assigned whichever flow happens to be active when a batch is flushed.

Provide `provenance.history`, a bounded read selected by flow or run, optionally including
descendants. It federates domain events, the audit sidecar and the authoritative
charter/grant/attempt records. A projection names its source authority id so the response can join
related facts without presenting duplicates as separate admissions. Use per-store `(flow_id,
sequence)` and `(run_id, sequence)` indexes or equivalent indexed association projections. Authority
records retain their existing per-run sequence as well as their integrity-journal reference.

Responses carry pagination, a per-store committed high-water vector, causal references, and coverage
for the requested interval: legacy association, missing store, known loss, expired payload,
unresolved attempt and access-filtered coverage as permitted by the read policy. A checkpointed page
excludes newer appends above its watermark. Concurrent deletion/retention is disclosed rather than
silently presented as a complete snapshot. Cross-store watermarks are not one simultaneous
transaction or a global causal order; timestamps are display data, not proof of ordering. The query
does not assert complete capture of unobserved external actions. A set of valid returned links does
not prove that a compromised query omitted no matching records. Claiming completeness of the stored
flow projection requires a trusted verifier to check the association index against the full covered
journal ranges; otherwise the response establishes inclusion/consistency only, with projection
completeness unverified. S1 ships that latter result. Verified stored-projection completeness is an
S3 extension, not a prerequisite for adding typed association and indexes.

Apply existing tenant, actor and sink read policy before joining/returning data; a flow id grants no
access. Do not reveal inaccessible record identities or counts through coverage metadata. Namespace
remains attribution/routing under ADR-007, not an added shared-store tenant boundary. A future
hosted embedder supplies trusted tenant scope and per-user authorization outside caller-controlled
input.

Existing events retain their original bytes and nullable association. Evidence-backed backfill uses
a versioned association table that records source evidence, method, author and confidence; ambiguity
stays unassigned. Changes to these associations are journaled as new records. A reconstructed legacy
association is never presented as contemporaneous authenticated context. Session id is not
redefined.

### D7. One integrity chain per physical store, including authority commitments

Maintain a namespace-neutral append-only integrity journal per physical store/writer. In a split
deployment the domain store and events sidecar each own a chain. In a single-store deployment there
is one. A chain spans all namespaces in that store so a namespace cannot simply disappear from the
declared chain coverage. S1 uses a locally configured store roster and reports local consistency
only. S2-H independently enrolls the roster so a trusted verifier cannot accept omission or
replacement of an expected store.

Journal entries have typed record identities: event, authority transition, association correction,
retention manifest, baseline or epoch transition. Within an interval advertised as chained, each
newly persisted event is committed together with its entry and observation projection. Legacy M1
telemetry capture gaps follow D11 and are outside that claim. The new S1 exception/grant path
requires its local authority commitment and refuses its transition if that commitment fails. An
authority transition commits its full canonical decision data and relevant state-change digest in
the domain transaction, rather than relying on a later audit event. Verifying an authority history
also checks its state/projection against those committed transitions; a chain alone does not
authenticate unrelated mutable tables.

For protocol version 1:

```text
record_digest = SHA-256(canonical_record_v1)
link_hash = SHA-256(canonical_tuple_v1(
    "khive-integrity-link-v1", algorithm_id,
    store_id, recovery_epoch, committed_sequence, previous_link_hash,
    record_type, record_id, attribution_header, record_digest
))
```

The tuple has a single documented length-delimited encoding; raw string concatenation is forbidden.
Canonical records have closed, versioned schemas and fixed cross-implementation test vectors. The
versioned attribution header retains the namespace and identity/read-scope references needed to
apply existing access policy, flow/run/operation/attempt references and their association assurance,
event time and retention class. It is committed directly by the link as well as agreeing with the
corresponding full record fields. This permits indexed history and access filtering after payload
expiry without pretending the full payload remains available. Unknown legacy attribution stays
explicit. The header contains no credential or free-form action payload. The event record covers
every semantic envelope field, original outcome, all attribution including
flow/run/operation/attempt, event time and duration, payload and schema versions, and the ordered
observation projection. Authority records additionally cover the exact descriptor, signature/proof
bytes or their immutable cryptographic commitment with a retained proof identity,
consumption/admission, original evaluation and result/reconciliation references. A mutable proof id
alone is insufficient. Secret material is excluded by the record schema before persistence, not
removed from a hashed record afterward.

Sequence is explicit, dense committed order within one store and epoch. It is neither SQLite rowid
nor a timestamp. Compute canonical record digests outside the writer where possible; sequence,
previous hash, link entries and the final head are assigned/updated inside the existing serialized
write transaction. A batch uses the existing acquisition and advances its head once after computing
its ordered links. Failed transactions advance nothing. An identical idempotent append returns the
existing identity/sequence/hash; the same id with conflicting content is an error. Linked records
cannot be edited in place; corrections append a new record referencing the old one.

The implementation inventory must cover single append, batch append, idempotent audit append,
existing-transaction append, generated SQL and direct/embedded paths, including SQL-generated
lineage warnings. Cover schema initialization/migration in both main and standalone events stores. A
wrapper around one `EventStore` method is insufficient. Protocol/schema capability negotiation
prevents a mixed-version writer from silently claiming chained coverage. Existing M1 telemetry
retains its non-enforcing fallback and reports the uncovered range. New grant/release operations and
M2 admissions refuse authority use through an incapable writer.

Initial activation creates an explicitly dated legacy baseline manifest and defines the first
chained sequence. It may commit an inventory of existing rows; it does not prove their earlier
authenticity, completeness or historical insertion order. S1 records store ids, local epochs and
detected restores, invalidates outstanding local approvals on a known restore, and begins an
explicit successor baseline; a hidden hostile rollback remains undetectable. S2 binds store ids and
epochs in independent enrollment. Restoration must reconcile protected consumption state (S2-A) and
the last independently known checkpoint where available (S2-H), establishing a root-authorized
successor epoch; it cannot silently reset sequence, drop a store or resurrect approvals. Hash
algorithm changes require a versioned transition committing the prior head, not an editable setting
that changes old link interpretation.

When S2-H was enabled, its last independently enrolled checkpoint is not optional on restore:
recover its trusted receipt or report the history unavailable. An explicitly missing range stays
missing; a fresh baseline cannot erase it. This is distinct from an ordinary periodic witness outage
on an already verified running deployment.

### D8. Independent checkpoints make only stated history tamper-evident

This is S2-H. S1 keeps the local chain and verifier without claiming an independent checkpoint. S2-H
can activate separately from protected action admission in S2-A.

Send checkpoint manifests to an append-only witness outside the shared OS user's rewrite authority.
A manifest commits deployment/tenant scope, the enrolled store roster, each
store/epoch/sequence/head, the previous checkpoint, observation time and explicit unavailable
stores. The witness authenticates submission and validates monotonic extension using the intervening
link headers from each previously accepted head, or an explicitly supported equivalent consistency
proof. A larger sequence number alone is not an extension proof. It returns a durable receipt with
its own observation time and keeps the prior checkpoints. Its retention/delete authority must not be
available to the same local requester. Verification retrieves a trusted witness receipt
independently of the local database. The witness need not possess effect credentials or human root
signing authority; its role is durable ordering and history retention.

The proposed initial submission target is every 60 seconds or 1,000 committed links, whichever comes
first, with prompt submission after root overrides and enrollment/epoch changes. Record the actual
last receipt and unanchored range. A heartbeat at the same target interval lets an independent
monitor identify silence, including when no new events are reported. These are initial operational
targets requiring measurement, not a claim of guaranteed delivery. Retention and restoration follow
the stronger checkpoint requirements in D7 and D9.

Once enabled, periodic checkpoints remain asynchronous to ordinary actions, including in M1. A
witness outage does not turn observability into a new admission gate. Mandatory M2 authority
durability and protected executor anti-replay checks are separate requirements; they do not rely on
a periodic witness becoming available after an action. A deployment may adopt stricter checkpoint
freshness admission policy only as an explicit later policy decision, with its availability tradeoff
stated.

Local git is not an independent witness. Remote git may serve only if its immutable retention and
verification survive compromise of the local user's credentials; an ordinary rewriteable branch does
not qualify. Another protected machine or suitable append-only service is preferred. A public
timestamp receipt proves a digest existed by a time; it needs an independently retained checkpoint
sequence and monitoring to establish continuity/freshness. Publish only appropriate checkpoint
commitments, not raw payloads, credentials or sensitive action descriptors. Metadata disclosure from
counts and timing must be considered before selecting a public witness.

An anchored prefix can be checked for alteration, deletion or rollback. A local attacker can rewrite
and recompute an unanchored tail, suppress future capture or stop the submitter. Missing heartbeats
show staleness, not the exact missing actions. A hash chain proves neither truth of reported facts
nor completeness of capture. A verifier must never collapse these limits into a single “trusted”
flag.

### D9. Retain chained history first; integrate retention in S3

S1 and S2 do not prune chained payloads, proof material, association records or commitment headers.
Any existing retention worker must exclude those covered records before chain activation, or that
activation refuses. Unchained legacy retention keeps its existing rules. Disk growth is an explicit
cost; storage exhaustion can still stop writes. Exporting a backup alone grants no deletion right.

S3 enables the following retention protocol only after S2-H acknowledgment is available. Apply
ADR-168's class-specific retention decisions, without broadening eligible classes here. When
archival is required, copy, seal and reread-verify the archive before pruning exact covered
payloads. When an accepted class permits expiry without payload archival, record that explicit class
and policy revision. In both cases retain the link headers and record digests needed to verify chain
continuity, and append a retention manifest naming the covered identities/ranges and archive or
expiry disposition. Obtain independent checkpoint acknowledgment of that manifest before destructive
pruning. If the witness is unavailable, defer pruning; current action admission remains unaffected.

Tombstoning an object is a new event; it does not rewrite historical events about the object.
Deleting a payload is not deletion of its link. Never rehash surviving rows to hide a missing
middle. A verifier with a payload can verify its digest; one with only its retained header reports
verified commitment continuity and unavailable content, not verified payload truth. Missing content
without an applicable committed retention manifest is unexplained loss or tampering, never ordinary
authorized expiry. Archives are validated before use. Unclassified or unresolved authority records
remain retained under their applicable rules.

The initial design keeps commitment headers indefinitely. It does not promise bounded total storage.
Segment compaction, legal erasure requirements, and replacement of retained headers by compact
proofs require an additional retention decision. Hashing low-entropy private content is not an
erasure or confidentiality mechanism; sensitive material must not enter public checkpoints or
prohibited payloads.

### D10. Verification and write-path cost are explicit product surfaces

In S1 provide a bounded read-only `provenance.verify` operation over a store/epoch/range or a flow's
referenced ranges. It reports checked heads and any trusted checkpoint receipt (absent in S1), first
mismatch or gap, available and missing stores, legacy baseline coverage, payload availability,
unanchored tail and the authority state checks performed. Distinguish at least anchored
verification, local consistency only, incomplete coverage, legacy data and detected tampering.
Unsupported proof or missing trust configuration for the requested verification scope is an explicit
unavailable result, not success. S1 local verification needs no witness, but a request for anchored
verification without S2-H cannot return anchored success. Cross-store verification checks the
enrolled roster.

S1 verifies new local committed ranges and reports its local trust scope. S2-H adds independent
checkpoint retrieval, incremental scheduled verification and a trusted verifier outside the local
user's replaceable execution path. S2-A startup/restore checks verify authority history and
protected epoch/consumption state before M2 resumes; they do not require periodic witness liveness.
An independent S2-H monitor observes checkpoint and verifier freshness. A local process can suppress
local reports, so a local scheduled job alone cannot establish continuing assurance. Verification
itself does not mutate, “repair” or normalize suspect evidence; repair is a separately recorded
recovery.

Keep hashing in the existing writer/group-commit design, with no new writer acquisition per event
and no network or hardware-signing call per ordinary event. Do not merge the two writer lanes to
obtain a fictitious global order. Hash work is proportional to canonical bytes; indexes and retained
headers add write/storage cost. Large payloads require bounded canonicalization and measured memory
usage. Count added statements, bytes, rows visited and acquisitions at their actual seams.

Record baseline/candidate measurements under comparable commit, workload, toolchain, features,
hardware, pool settings and storage conditions. Include single writes, the existing
`event_append_batch/records/{100,500,1000}` scenarios, idempotent batches, maximum supported
payloads, domain/audit contention, WAL growth, query indexes and verification throughput. Measure
individual latency observations to report p95/p99; Criterion batch estimates cannot be relabeled as
those percentiles. Follow accepted performance policy and the status of ADR-206: proposed timing
machinery is not active merely because this record references it. New baselines remain pending until
measured; no fixed slowdown tolerance or absolute latency promise is invented by this design.

### D11. Staged delivery preserves the recording/enforcement boundary

Stages are product capabilities, not claims that they are already implemented. S1 is the first
release under the shared-user cooperative topology. S2 has two independently activated boundaries;
S3 contains extensions. Accepting this ADR does not activate any of them.

| Stage                                     | Ships                                                                                                                                                                                                                                   | Activation evidence and limits                                                                                                                                                                                                                                                                                                           |
| ----------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **S1 — cooperative local functionality**  | Exact root phase release; the minimum shared-grant service; one-use resource grants and exact root exceptions at the enrolled merge adapter; typed flow/run history; per-store chains and local verifier; no pruning of chained records | A01–A18. Signatures and local transactions are checked, but local root enrollment, clocks, code, credentials and consumption state share one OS trust boundary. Report local consistency and cooperative authorization; no hostile same-user containment, rollback resistance, anchored verification or verified projection completeness |
| **S2-A — protected authority**            | Protected enrollment/signer/display, verifier and executor credential custody, protected replay/epoch/time/revocation state; M2 exact-action charter admission                                                                          | P01–P05, including the S1 functional cases under protected authority. Every in-scope effect route is covered. No protected-mode fallback to S1. Periodic witness availability is not an admission dependency                                                                                                                             |
| **S2-H — independently verified history** | Protected store roster, independent append-only witness, trusted receipt retrieval/verifier and freshness monitor                                                                                                                       | H01–H04. Requires S1's journal, not S2-A. Can verify recorded M1 history without enforcing actions. Anchored prefixes prove committed bytes, not truth or completeness of capture                                                                                                                                                        |
| **S3 — bounded extensions**               | Retention integration; verified completeness of stored flow projections; additional resource adapters                                                                                                                                   | E01–E04. Retention requires S2-H; protected additional adapters require S2-A. Each extension activates separately. None delays S1                                                                                                                                                                                                        |

M1 charter runs remain `assurance = recording_only`; `charter.claim` cannot create an admission. The
new root release changes **recorded progress** and the cooperative merge path changes only
**explicitly enrolled adapter authorization**. Neither is a claim that M1 enforces the workflow. An
unenrolled existing action acquires no new signing, grant or witness dependency. Calling the
cooperative merge adapter does not imply that all other effect routes have been intercepted.

Existing M1 telemetry retains its class-specific durability/failure behavior. Lost capture or a
missing sidecar reports unavailable/gapped coverage; it does not turn best-effort observability into
a permission check on an existing action. After repair, append an explicit gap/baseline transition.
Never advertise an interval as chained if its links did not commit. In contrast, the **new** S1
release and grant-consuming operations require their local authority transaction and commitment: a
failure cannot yield their release or authorized send. Existing mandatory receipts stay mandatory.

M2 additionally requires the ADR-193/208 authenticated producer mechanisms for the facts it actually
uses, protected exact-subject verification and resource custody, protected anti-replay state and
platform protection closing alternate effect routes. A policy fact explicitly waived as absent need
not have an available independent producer; the exception and all unwaived facts still require their
own valid provenance. Policy exceptions waive none of these mechanism checks. The remote service
still applies its own rules and may refuse. Without independently protected custody and complete
interception, a deployment remains cooperative even if all local functional tests pass.

Implement S1 in dependency order: descriptor/mode and shared-service contracts; flow association,
journal coverage and shared transactional consumption; phase/merge integration; history/verification
surfaces and paired acceptance. These may be developed independently where interfaces permit, but S1
completion requires all A cases. Local service implementation is necessary work, not an assumption
that current tool grants already provide it. M1-to-M2 activation never upgrades a recording result
or local proof: start/reconcile under the protected epoch and issue fresh approvals for current
subjects. Existing runs retain their historical assurance. No new predicate language or arbitrary
executable action is added.

#### Decision-to-stage map

| Decision | S1                                                                                    | Later stage                                                                                            |
| -------- | ------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------ |
| D1       | Policy/protocol distinction; append-only root phase release; local adapter exceptions | S2-A: protected release and fresh M2 admission; predecessor releases confer no inherited authority     |
| D2       | Canonical signed descriptor, local enrollment and explicit mode/limits                | S2-A: protected display, enrollment, verification, recovery and custody                                |
| D3       | Exact single-use JIT merge grants, expiry/revocation and no implicit waiver           | S2-A: protected executor binding and rollback resistance; S3: other adapters                           |
| D4       | Shared service and exact merge descriptor; ADR-182 review/separation amendment        | S2-A: charter claim and protected executor; S3: remaining adapter table                                |
| D5       | Atomic local release/consumption/intent; external uncertainty/reconciliation          | S2-A: independent executor reservation and recovery; S3: adapter-specific protocols                    |
| D6       | Typed association, indexes, bounded federated history, legacy uncertainty             | S3: trusted full-range check for verified stored-projection completeness                               |
| D7       | All-path per-store journal, canonical vectors, local baseline/epoch and local roster  | S2-A: protected authority restore/epoch; S2-H: independently enrolled roster and anchored history      |
| D8       | No independent anchoring claim                                                        | S2-H: checkpoints, extension proof, witness receipts and heartbeat monitoring                          |
| D9       | No pruning of chained records/commitments, including exclusion from existing workers  | S3: acknowledged retention manifests, archive/expiry verification; requires S2-H                       |
| D10      | Local read-only verifier and cost evidence for the features shipped                   | S2-A: authority startup/restore checks; S2-H: independent verification/monitoring; S3: extension costs |
| D11      | S1 scope, M1 boundary, explicit opt-in adapter enrollment and no assurance promotion  | S2-A/H and S3: separate activation gates as listed above                                               |

## Consequential amendments

These changes are normative companions at their named stage. Earlier unconditional wording must not
remain in force alongside the exception. References to proposed records do not accept their
unrelated decisions or assert their implementation.

1. **ADR-193 D3/D6 and state machine (S1; M2 clauses at S2-A):** qualify ordinary holding/admission
   rules: a hard gate has no ordinary or delegated override; an authenticated root exception may
   accept its named hard, soft or unknown outcome under ADR-209. Keep raw results immutable and
   protocol validity non-waivable. Replace unqualified evidence-completeness wording with D1's
   complete-mechanism/possibly-absent-policy-fact distinction. Add the `charter.release` transition
   from a current held visit to its declared successor with `completion_disposition = exception`,
   atomic proof consumption/evaluation/replay, and no external admission. Qualify ordinary
   `waiting_* -> ready` and internal completion guards accordingly. Add successor-claim reevaluation
   and separate exact-action exception consumption; neither a prior release nor `ready` grants an effect.
2. **ADR-193 D7/D9 and admission acceptance (S2-A):** the accepted review-independence and separation
   predicates remain required for ordinary admission and are waivable only as exact root policy
   instances. Authenticity and integrity of evidence actually used remain mandatory; waived absence
   does not fabricate independence. Bind the verified exception through `charter.claim` to the
   existing merge adapter, its executor credential and pre-send receipt. Forged, stale, inherited or
   mechanism-waiving exceptions still refuse; add the paired root-positive cases below.
3. **ADR-193 D4 and deferred list (S1/S2-H):** replace the chain deferral with S1 local journals and
   local-consistency limits; replace the anchoring deferral with the separately activated S2-H
   contract. Preserve transaction atomicity, immutable evidence and per-run ordering.
4. **ADR-193 D11 / ADR-208 D3/D6/D7, acceptance and known limits (S1/S2-A):** replace the ambiguous
   statement that M1 blocks nothing: M1 does not enforce external actions, while recorded phases can
   wait on triggers, actions, children and requirements. Add D1's exact root phase release, including
   missing/failed conditions, declared routing, child handling and propagated exception disposition.
   Qualify the ordinary unknown/missing-input rule and ordinary matching-observation-only acceptance
   to recognize this separate transition without manufacturing observations. The parsed `break_glass`
   clause is neither authentication nor enrollment, and false `marks_degraded` never hides a release.
   Replace the blanket soft-override deferral with this staged service contract; keep `charter.claim`
   unavailable in M1 and require fresh protected authority for M2.
5. **ADR-182 Amendment 2 item 5, Amendment 6 item 3, Amendment 7 and their acceptance arms (S1/S2-A):**
   retain ordinary review/separation refusals and add exactly the D4 verified-exception mapping.
   At S1 this applies only to explicitly enrolled cooperative merges; at S2-A it consumes the M2
   admission's mapped policy exception. Preserve both expected-head comparisons, repository and
   account identity checks, dispatch/resource authorization, receipts, reconciliation, and the
   prohibitions on administrator bypass, force operations and delayed auto-merge. Separate root
   waivers of local policy from refusal by the remote resource. `git.pr_review` stays unchanged.
6. **ADR-127/143, ADR-180 and ADR-192 (S1/S2-A/S3):** cross-reference the two approval purposes,
   exact target kinds, explicit cooperative/protected mode, shared consumption and recovery contract.
   Implement the shared service minimum rather than assigning new meaning to existing tool-grant
   patterns or free-text scope. Protected custody remains a deployment prerequisite; additional
   adapters retain D4's separate activation requirement.
7. **ADR-041/133/162/164/168/170 (S1/S2-H/S3):** add flow context and local per-store commitments while
   preserving per-class durability, access policy, batching and split writer routing. S1 excludes
   chained records from destructive retention. S2-H adds independent checkpoints; S3 adds only the
   explicitly acknowledged retention protocol without broadening retention-class eligibility.
8. **ADR index:** list this Proposed record and its staged dependencies.

## Acceptance

These are implementation and activation requirements, not executed-test claims. Use real registered
verbs and their transaction/effect seams with isolated fixtures. A transport fixture may represent a
remote service that permits the ordinary merge request; protected deployment acceptance additionally
checks the real enrolled service. A01–A18 establish S1; P01–P05 establish S2-A; H01–H04 establish
S2-H; E01–E04 apply to their S3 extension. Every case has an explicit stage and decision mapping
below.

At the grounding commit, recording evaluation and progression are in
`crates/khive-pack-charter/src/engine.rs` (`evaluate`, `advance`, `enter`, `end`), with command
replay in `crates/khive-pack-charter/src/pack.rs::with_receipt`. Merge predicates and the send are
in `crates/khive-pack-git/src/remote_handlers.rs`, including `require_review`. The cases below
exercise these seams or their direct replacements, not a separate authorization-only simulation.

### S1: paired functionality and local consistency

- **A01 — M1 boundary (D1/D11).** Existing unenrolled actions need no new grant/signature/witness.
  `charter.claim` remains unavailable/refusing, and no M1 release creates a charter attempt. The
  explicitly enrolled cooperative adapter requires its new local resource authority and labels it
  cooperative. Replaying or switching modes never upgrades a recording approval or grants a new
  send.
- **A02 — release scope and transaction (D1/D2/D5).** Release one current held phase to its signed,
  declared route. Read back the append-only evaluation, authorizer, executor, proof, disposition and
  replay record; the next phase is reachable and `action_admitted` is false. Race two calls, replay
  the winner and change its bytes: there is one transition/consumption, no second visit released,
  and conflicts refuse. A failed record/commit or stale revision releases nothing.
- **A03 — hard, soft and unknown release positives (D1).** At the `engine.rs` `evaluate`/`advance`
  seam, separately hold a hard requirement with a non-pass outcome, a soft requirement with a
  non-pass outcome, and each with no matching observation (`unknown`). Without the exception each
  remains `Eval::Wait`; with the exact root exception each releases once while its raw outcome stays
  fail/unknown. Include all holds in a mixed case; omitting one still holds. A matching `when` on
  the same failed requirement retains its existing ordinary route; one on a different requirement
  does not release it. **Must-fail control:** removing exception-transition handling makes the
  root-positive cases fail, while ordinary matching-route cases continue to pass.
- **A04 — stanza independence (D1).** Repeat the release-positive cases and the merge-positive A07
  cases with no `break_glass` stanza, then with a valid stanza whose omitted `marks_degraded`
  keyword compiles to false. Both succeed and retain the full exception record and exceptional
  disposition. A false flag cannot hide the exception. The fixture uses valid parser syntax, not an
  invented `marks_degraded = false` assignment.
- **A05 — omitted work and child results (D1).** Accept an explicitly missing policy observation,
  failed recorded action or terminal failed/cancelled child without creating a success observation.
  A genuinely unstarted action may be recorded `not_performed`. A `perform` whose expected child
  never existed is a mechanism inconsistency and refuses; cancel an existing child to omit its work.
  A live child or possibly executed/uncertain action refuses release until valid cancellation or
  reconciliation establishes its disposition. A missing child caused by corrupt references refuses.
  Parent/run history carries exception references; downstream requirements never infer that skipped
  work ran. An undeclared route, exhausted visit bound or changed generation refuses.
- **A06 — ordinary JIT positive and controls (D2–D5).** Through the enrolled `git.pr_merge` path
  with all applicable local policies passing and a permissive remote fixture, a valid ordinary JIT
  grant authorizes its exact action once. Assert one consumption, one pre-send receipt, one native
  merge request with the bound head, and one recorded result, with separate authorizer and executor.
  No grant or a mismatched grant refuses; replay yields history and no second request. Add a hard or
  soft holding policy: that same kind of ordinary grant cannot waive it. It cannot issue/renew
  grants, rotate root keys, broaden its target or choose a later exception.
- **A07 — root merge positive at both review seams (D1/D4/D5).** Against `remote_handlers.rs`, keep
  current subject checks and resource authority valid; return a non-`APPROVED` aggregate and an
  empty review list. Without root exceptions the adapter refuses. With exact waivers of both D4
  review instances it reaches the ordinary merge send once and records one effect. Separately return
  `APPROVED` with an empty list to prove the `require_review`/`missing_review` path honors only its
  mapped waiver. Exercise `merger_not_opener` and `merger_not_last_pusher` failures separately and
  together, each with and without its named exception. Preserve all raw results, proof, authorizer,
  executor and single consumption per purpose. **Must-fail controls:** removing exception handling
  at the aggregate-decision seam or inside the mapped required-review/separation path must turn its
  respective root-positive arm red. No fixture may bypass the actual merge handler.
- **A08 — incomplete waiver and protocol controls (D1/D4).** A review-only exception cannot waive an
  additional separation veto, and a release proof cannot authorize a merge. Invalid signature, root
  label, edited role or unregistered key refuses through the honest service. Changed expected head,
  target, repository, method, account, descriptor, purpose or authority mode refuses. A pull request
  whose base differs from the signed target at the subject read, with the head unchanged, refuses as
  `target_mismatch` even with a valid root exception for every review and separation predicate. A valid
  root signature requesting replay, corrupt authority state, missing receipts, unsupported admin/force
  modes or bypass of resource authorization also refuses. Same-user replacement of enrollment/code is
  expressly outside S1's guarantee; this case must not claim to test its prevention.
- **A09 — complete descriptors and policy absence (D1/D2).** An incomplete subject, unidentified
  requirement set, conflicting bytes under one approval id, ambiguous encoding, duplicate/unknown
  field or unsupported version refuses. A precisely identified absent policy fact can be waived as
  unknown without an available producer for that fact; neither it nor a supplied untrusted report
  becomes passing evidence. A changed semantic policy snapshot requires fresh approval; an unchanged
  snapshot with a new observation timestamp alone does not invalidate it.
- **A10 — current authority (D3).** A revoked, expired or not-yet-valid grant refuses before a send.
  Deferred work that passed an earlier informational check but expires or is revoked before
  consumption refuses; valid deferred work consumes once. Revocation after consumption records that
  ordering and does not claim to undo the effect. A different authenticated executor cannot use the
  proof, while the intended executor's compromise remains the documented limit.
- **A11 — local crash and replay (D5/D7).** Inject failure around consumption, authority append and
  commit: local transitions and consumption roll back together, including the journal head. A lost
  commit response resolves by the immutable attempt/command identity without another consumption. If
  the pre-send intent cannot commit, the remote transport sees no send.
- **A12 — external outcome and resource refusal (D4/D5).** Crash after intent, during send and after
  remote success. Preserve consumed approvals and uncertainty; reconcile without blind resend or a
  new idempotency key. A permissive resource yields A07's one effect; the same local root exception
  against a resource enforcing required review yields a recorded remote refusal and no successful
  merge. A head move between local check and remote CAS refuses. A retarget that keeps the head and
  lands after the subject read, against a resource that permits the merge, reconciles as
  `merged_target_mismatch`, never as success, and the case claims no prevention. Its control runs
  through the same recovery path: an uncertain receipt whose confirmed remote merge kept the head and
  the signed target reconciles as success, never `merged_target_mismatch`, with one effect, no resend
  and the approvals consumed. Proven non-effect still does not reopen a grant. Never claim automatic
  exactly-once behavior without a provider contract.
- **A13 — attribution (D6).** Concurrent flows, nested/child runs, late flushes, retries, denials,
  releases and resumed work keep their own typed context captured before queueing. Spoofed incoming
  flow/run fields do not become authoritative association. Initial association is still only as
  trustworthy as the local producer; hashing a false association does not make it true.
- **A14 — bounded history and legacy data (D6/D10).** `provenance.history` returns accessible
  domain, audit and authority records with source identities, pagination and per-store high-water
  bounds. Report missing stores, capture/retention gaps and unresolved attempts without disclosing
  restricted identities/counts. A flow id grants no read access. Backfill keeps original bytes and
  ambiguous legacy association explicit. Local verified links establish no global snapshot,
  historical authenticity or completeness of the stored flow projection.
- **A15 — journal coverage (D7).** Fixed canonical vectors cover all semantic fields and
  attribution. Exercise singleton, batch, idempotent, existing-transaction, generated-SQL and
  direct/embedded writes in both store layouts. Rollback advances neither links nor head; an
  identical append reuses its entry and a conflicting id refuses. Sequence survives vacuum/clock
  changes and out-of-order event times. Mixed-version writers cannot silently claim chained
  coverage; new authority paths refuse an incapable writer, and legacy M1 telemetry follows D11's
  gap reporting, as A14 exercises it.
- **A16 — local verification limits (D7/D10).** Changed bytes, missing links and broken local state
  projections report mismatch/incomplete coverage. A wholly recomputed unanchored history may be
  locally consistent and must never be labeled independently verified. A known restore creates a
  successor local baseline and invalidates outstanding approvals; hidden hostile rollback remains
  undetectable. The verifier is read-only, reports unsupported proof/missing trust explicitly, and
  never repairs evidence. A legacy baseline commits only its present inventory.
- **A17 — no pruning (D9).** Existing retention workers and maintenance routes cannot delete a
  chained payload, authority proof, association or header under S1/S2. An unexcluded delete path
  blocks chain activation. Unchained eligible legacy retention retains its existing behavior. Report
  storage growth and unavailable capture honestly; never rehash survivors to conceal loss.
- **A18 — initial cost (D10).** Equivalent event batches use the same writer-acquisition count and
  no synchronous witness/signing call per ordinary event. Measure S1 hashing/indexing/query/verifier
  costs with D10's comparable workloads, including actual latency samples for any p95/p99 claim.
  Unmeasured results remain pending, not performance guarantees.

### S2-A: protected authority and M2 admission

- **P01 — protected activation (D2/D5/D11).** Demonstrate enrollment/recovery protection, trusted
  exact display, invocation controls, credential custody, clock/revocation behavior, independent
  replay state and closure of every enrolled effect route. Same-user key/config substitution,
  counterfeit display, noninteractive signing, weaker fallback and direct credential use must not
  obtain protected authority. Document residual external administrator powers and outages.
- **P02 — protected functional parity (D1–D5/D11).** Repeat A02–A12 through the protected verifier
  and, for effect cases, real `charter.claim`/merge integration. Successful grants and root
  exceptions still consume once and retain raw outcomes; S1 proofs or a mode downgrade refuse.
  Protected mechanisms may not pass acceptance by rejecting every valid request.
- **P03 — predecessor and action exceptions (D1/D4/D11).** In an M2 run, separately use failed hard,
  failed soft and precisely unknown requirements to hold an internal recording phase, then release
  it and reach its successor. The later M2 claim re-evaluates those requirements. The consumed
  release proof alone refuses action admission; a new exact-action exception covering the
  still-failed/unknown requirements, plus valid resource authority, yields one admission and one
  merge at the actual adapter. An unavailable producer for a waived absent fact does not veto this
  case; missing authentic mechanism inputs or an unwaived failing requirement does. Repeat
  absent-stanza/false-flag and must-fail controls from A03/A04/A07. A release in an M1 run remains
  history when establishing a new protected run.
- **P04 — restore and concurrency (D2/D3/D5/D7).** Restore an older local database and try a
  previously consumed/revoked approval: independent executor state refuses it. Concurrent
  consumes/reservations yield one attempt. Successful authorized epoch recovery preserves lineage
  and requires fresh approvals; forged epochs, unknown protected state or a missing required store
  refuse activation.
- **P05 — protected failure seams (D5/D10/D11).** Crash after local intent, executor reservation,
  send and remote success, including lost replies between stores. Preserve uncertainty and
  reconcile; no rollback resurrects authority. Startup/restore refuses until required authority
  state is verified. A periodic witness outage alone does not block an otherwise valid protected
  admission. Stopping the signer/executor produces unavailable, never a weaker local fallback or
  fabricated effect.

### S2-H: independent history verification

- **H01 — checkpoint protocol (D7/D8).** A witness accepts an actual extension of its previous head
  and refuses a larger non-extending head, a forged epoch or silent omission of an enrolled store.
  An explicitly unavailable store remains in the roster with incomplete coverage and no invented
  head advance. Obtain the receipt independently of the local database; a substituted local receipt
  cannot establish anchoring.
- **H02 — anchored alterations (D6–D8/D10).** Alter an event, observation, proof, association or
  retained payload; delete a middle entry, omit a namespace/store or restore an old head.
  Verification against independent receipts/roster detects the discrepancy. Rewriting before first
  anchoring can commit false bytes; neither the witness nor verifier may report their truth or
  complete original capture.
- **H03 — outage and silence (D8/D10/D11).** Stop submission or the local verifier, including an
  idle period. The independent monitor reports staleness and actual unanchored range. Witness outage
  is visible and does not become an ordinary action-admission gate. Local git or a remotely
  rewriteable branch under the same credentials cannot satisfy independent-witness activation.
- **H04 — verifier contract (D8/D10).** Exercise anchored verification, local consistency,
  incomplete coverage, legacy baseline, missing payload/store, unsupported proof and detected
  mismatch. Cross-store results check the roster without inventing global ordering; trusted
  verification and scheduled checks are outside the local replaceable executable path. Verification
  never changes evidence.

### S3: separately activated extensions

- **E01 — retention (D9; requires S2-H).** For eligible classes, validate any required archive and
  commit a manifest, obtain its independent acknowledgment, then prune only covered payloads. Crash
  before/after each step: no early delete, no lost manifest and no rewritten links. Witness outage
  defers deletion. Tampered archives and unmanifested missing content refuse verification; retained
  headers verify continuity while explicitly disclosing unavailable content. Unclassified authority
  records stay retained. No class becomes eligible merely because this feature exists.
- **E02 — stored-projection completeness (D6/D10; requires S2-H).** A trusted full-range
  journal/index check detects an omitted matching result even if every returned link verifies. Only
  that check may report a complete stored projection for its declared accessible coverage; capture
  truth and unobserved external activity remain outside the claim.
- **E03 — additional adapters (D2–D5; protected forms require S2-A).** Each registered adapter in D4
  demonstrates a valid one-use grant and exact root-policy exception, paired with material-field
  substitution, unwaived-policy, concurrency and crash/reconciliation failures at its actual effect
  seam. Payments bind amount/payee/currency; store effects bind preimages; credential use releases
  no secret. Raw export cannot claim control of later secret uses. No arbitrary SQL or executable
  action substitutes for a registered descriptor.
- **E04 — extension costs (D8–D10).** Measure witness cadence/freshness, retention growth, archive
  and full-range verification throughput and the additional adapters under comparable conditions.
  Retain the 60-second/1,000-link cadence as a target until demonstrated, and do not relabel batch
  estimates as tail latency or claim bounded total storage from retained headers.

## Alternatives considered

| Decision                                                             | Rejected alternative                                                                         | Reason                                                                                                                      |
| -------------------------------------------------------------------- | -------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------- |
| Named root exceptions over hard/soft/unknown policy                  | Absolute hard predicates; universal root/role bypass                                         | The first denies the requested exception, while the second lets a forged request waive its own invalidity                   |
| Append-only phase release with original outcomes                     | Leave every hold to normal observations; insert a synthetic pass/child completion            | An unhandled hold has no root escape; synthetic success falsifies the work record                                           |
| Separate release and action approvals                                | Inherit a predecessor waiver into every successor                                            | Action policy and subject can change; one recorded disposition must not become reusable effect authority                    |
| Mapped local merge-policy exceptions                                 | Ignore local review vetoes; disable the whole review helper or use administrator mode        | The first strands a valid exception; the others waive unnamed predicates or change remote enforcement                       |
| S1 cooperative functionality, S2 protected boundaries, S3 extensions | Wait for all protected infrastructure; ship local signatures as containment                  | The first delays the four functional capabilities unnecessarily; the second asserts isolation the deployment lacks          |
| Exact descriptors with explicit authority mode                       | Console-origin trust, role labels or implicit promotion of local proofs                      | None binds protected human intent; a mode change cannot authenticate an earlier local decision                              |
| Protected signer/display contract                                    | Treat hardware non-exportability as sufficient                                               | Key extraction protection says nothing about invocation or agreement between displayed and signed action                    |
| One-use ordinary resource grants                                     | Require interactive execution after approval; silently include discretionary root delegation | Transport/execution need no new decision, while discretionary delegation is a separate unresolved policy question           |
| Shared grant service, one merge adapter first                        | Reinterpret tool scope; build a second grant service; require all resource adapters for S1   | These respectively lose exact subject semantics, duplicate authority state, or add unrelated delivery prerequisites         |
| Local transaction plus explicit external protocol                    | Claim SQLite consumption and remote send are atomic                                          | No shared transaction or provider contract establishes it                                                                   |
| Typed correlation and one bounded history surface                    | Reuse session ids; JSON-only filters or graph edges alone                                    | They conflate identities or leave indexing and coverage dependent on optional mutable projections                           |
| Namespace-neutral per-store chain                                    | Per-namespace heads; one global deployment sequencer                                         | Namespace omission needs an outer roster; global sequencing couples independent writers without making their effects atomic |
| Independent periodic checkpoints                                     | Local git/head, rewriteable remote branch, per-event network signatures                      | Rewritable history is replaceable; per-event network work adds cost and availability coupling without proving capture truth |
| Retain first, integrate manifest-based retention later               | Rehash survivors; require archival machinery for S1                                          | Rehashing hides loss; no-pruning allows a smaller first release with explicit storage cost                                  |
| Precise companion amendments                                         | Change only ADR-193 D6; put the entire contract in workflow syntax                           | Either leaves contradictory local vetoes or assigns resource/history semantics to the wrong contract                        |

## Known limits and open questions

- S1 is cooperative: same-user access can forge local approvals, replace a verifier/display, race the
  intended executor, bypass the service, rewrite a chain or restore consumed grants. Signatures and
  local consistency checks alone contain none of those peers. S2-A/H make only their separately
  verified custody and coverage claims; tamper evidence is detection, not prevention.
- The human can approve a harmful action or be misled. Exact display reduces substitution risk, not
  decision risk. Signer, executor or remote-service outage can delay root's requested action; no
  in-system mechanism promises operation when all trusted facilities are unavailable.
- Unobserved actions, dishonest reports, initial flow misattribution, pre-baseline history and
  pre-anchor laundering remain outside authenticity/completeness claims. An anchored false report
  is still false. Cross-store checkpoints do not prove a simultaneous global snapshot.
- Remote success can remain unresolved. Expiry cannot undo completed publication/payment or recall
  an exported secret. A merge's signed target is checked at admission only: a same-head retarget after
  the adapter's last read is detected at reconciliation, not prevented. The resource's own protection
  may reject a locally authorized exception.
- **Open product question: may root delegate discretionary policy-exception authority to another
  principal?** This version supports only transport/execution of already specified root decisions;
  it does not settle whether a future delegation mechanism should exist or how it would be bounded.
- **Open product question: must a root exception receive a later recorded review?** The exception
  itself is always recorded. No automatic review duty, deadline or blocking consequence is selected
  here; those require an explicit policy decision and must not be inferred from the audit trail.
- Protected enrollment/recovery, trusted clocks, custody and platform interception require concrete
  deployment evidence. Local lost-key/outage recovery cannot silently confer protected authority.
  A future hosted deployment must bind tenant and human account scope at trusted ingress; a local
  root is not a cross-tenant administrator and a namespace string is not tenant authentication.
- S1/S2 commitment retention grows without a bounded-storage guarantee. S3 must preserve both class
  eligibility and acknowledged commitments; legal erasure and compact replacement proofs need a
  further retention decision. Hashing private low-entropy content is not an erasure mechanism.
- At the grounding commit the charter milestone is M1. Proposed ADRs, stage definitions and acceptance
  cases are not evidence of implemented controls. Timing, protected hardware properties and complete
  interception/coverage claims require their specified measurements and activation checks.

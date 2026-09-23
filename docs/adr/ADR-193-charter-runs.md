# ADR-193: Charter Runs — Procedural Actions Admitted Only on Recorded Evidence

- **Status**: Proposed
- **Date**: 2026-09-22
- **Depends on**: [ADR-127](ADR-127-authenticated-actor-and-grant-primitive.md) (authenticated actor
  and grant primitive), [ADR-143](ADR-143-store-held-caller-grants.md) (store-held caller grants),
  [ADR-182](ADR-182-git-dev-loop-verbs.md) (git verbs, including `git.pr_merge`),
  [ADR-192](ADR-192-credential-and-producer-seams.md) (credential custody stays outside this repository)
- **Relates to**: [ADR-066](ADR-066-autonomous-merge-pipeline.md) (autonomous merge pipeline),
  [ADR-102](ADR-102-tiered-validate-and-merge.md) (knowledge-graph change sets),
  [ADR-142](ADR-142-agentic-process-runtime.md) (process runtime lifecycle matrix)

## Context

A long-running organisation of agents takes consequential actions on behalf of a person: it merges
pull requests, publishes releases, spends money. Today the conditions for such an action are held in
the actor's discipline. A pull request merges when the merging actor believes the required checks are
green at the current head, a review exists for that head, and the reviewer is someone other than the
author. Nothing in the store records that those conditions were checked, against which evidence, at
which moment, and nothing refuses the action when one of them is missing.

A **charter** makes such an action procedural. It declares ordered phases. Each phase waits for a
trigger, evaluates gates over recorded evidence, records what it decided, and only then permits its
action. A person or agent that needs to act sees the item in a derived waiting view; nothing is
queued or delivered.

An earlier scope decision removed a charter language and compiler, a general separation-of-duties
framework and just-in-time one-time token grants as more machinery than the system needed. It kept
policy authoring, an evidence log, and one narrow exception: a break-glass override for the
orchestrator principal, granted by a human, usable once, audited per use. This record keeps that
removal. Charters here are bounded data for a closed set of templates, and their gates are compiled
code. There is no language.

What exists already, and what this record builds on rather than duplicates:

- The GTD pack's status transition table and compare-and-set update discipline, and its pack-owned
  schema plan. Its audit table is written after the task change commits and tolerates failure. That
  is the wrong atomicity for authority evidence, so the discipline is reused but that boundary is not.
- ADR-142's From/To/Trigger/Idempotency matrix and canonical-digest replay key for agent processes.
  Its host-restart rule terminates processes; a waiting charter must survive a restart, so the matrix
  shape is reused but not the runtime table.
- The tool pack's grants and policies. A tool grant selects on actor and tool patterns, status, expiry
  and registry pin; it does not compare an action subject, and an active grant takes precedence over a
  deny policy. That answers "may this actor call this verb", which is one admission prerequisite, not
  an exact-subject authorisation.
- `git.pr_merge` (ADR-182). It consults `tool.check` for the verb name, runs the platform client with
  the calling actor's own credential reference, records a receipt before sending, and offers
  `git.reconcile` for ambiguous outcomes. It is the existing effect path for merges.
- The event store. Its append interface is separate from pack SQL units, and its phase events describe
  background phases, not authority. It can carry projections of charter activity for observability;
  it cannot be the authority record.

## Decision

### D1. A charter pack owns runs, phases, definitions and evidence

A new `charter` pack owns its tables through a pack schema plan. It does not reshape GTD (a charter
run is not a task tree, because a mutable child task can be completed outside the gate), and it does
not extract a shared state-machine substrate from the process runtime now (the two lifecycles differ
exactly where it matters: restart). A shared helper is extracted only after a second domain shows
identical semantics.

### D2. Definitions are bounded data for a closed template; gates are compiled

A definition is immutable data validated against a versioned schema for one **template**. The only
installable template in v1 is `pr_merge/v1`. Rust owns the template's phase topology, the gate
implementations, their parameter schemas and their security floors. A definition may set approved
principals, repositories, a required-check floor and stricter freshness bounds. It may not remove a
mandatory gate, lower its enforcement, add an expression, reorder mandatory phases, name an action
kind the template does not have, or reference a gate outside the compiled registry.

This is what "no language" means operationally: no predicate evaluation, templating, scripting,
arbitrary boolean trees, runtime plugins or verb strings. Publishing rejects unknown fields and
versions, duplicate phase ids, registry mismatches, reordered mandatory phases and weakened floors. A
new topology, gate kind or action adapter is a code change with schema review.

### D3. Gates are pure functions over a recorded evidence bundle

`evaluate(definition, candidate, evidence_bundle, authority_snapshot, evaluation_time)` returns, per
gate, `{gate_id, implementation_digest, status, reason, evidence_ids}` with status `pass`, `fail`,
`unknown` or `not_applicable`. Gates have no runtime handle, network, filesystem, clock, randomness or
model access. Unknown, missing, contradictory, expired or untrusted input blocks. `not_applicable` is
produced only by the gate's own fixed applicability rule over complete evidence; a caller cannot
supply it.

External reads happen in **instruments** outside the gate. An instrument submits an observation
through `charter.observe`. The authenticated caller fixes the producer; a producer registration limits
which observation kinds it may submit and for which repositories. Each observation carries its
source identity, source event or attempt identity, observation interval, completeness
(`complete | partial | unavailable`) and a payload digest; the server records receipt time. Caller
fields never set the producer, assurance, receipt time or an internal kind. Selection is by source
generation and complete inventory, never "newest green". A later relevant failure invalidates an
earlier pass. The same source identity with different bytes is an `EvidenceConflict`. A failed poll
never refreshes the last good sample.

### D4. Evidence is a pack-owned ledger committed with the transition

Evidence rows, the evaluation, the phase transition, the command replay record and any action claim
commit in one SQL atomic unit. A per-run dense sequence orders accepted facts and payload digests bind
evaluations to bytes. Charter evidence is its own table, not a note kind, so generic note and stream
verbs cannot write it. Correction is a new row; nothing updates or deletes authority evidence.

A previous-row hash chain is deferred. A chain whose head lives in the same writable database does not
detect an administrator rewriting the whole history, and it authenticates neither an instrument nor
its observations. Writer authorisation, immutable references and transactional commit come first.
External anchoring is recorded as future work.

### D5. The waiting view is a projection, not a queue

`charter.waiting(actor | role)` returns the phases currently waiting on that actor or on a role the
caller currently holds, including ready actions waiting for their designated actor. It has no read or
acknowledge state. Consumers compose it into the attention surface they already poll; delivering or
acknowledging a notification never advances a phase. The GTD `next` query is not changed.

### D6. Enforcement is hard or advisory in v1; the soft override is the surviving break-glass

A **hard** gate has no override. An **advisory** gate records its result and never blocks on its
predicate. Authentication, evidence integrity and completeness, and the availability of authorisation
are preconditions of the mechanism itself and are hard even for an advisory rule.

**Soft** enforcement is deferred until the shared grant service can consume a grant atomically with an
admission. When it lands, a soft override is exactly the break-glass the earlier scope decision kept:
a human-issued, one-use grant to the orchestrator principal naming the run, phase, candidate
generation, definition and gate digests, the failing evaluation, the named gate instances, a reason
and an expiry. It is consumed in the same unit as one admission, never applies to a new head or a
later attempt, is never inherited by workers, and leaves the overridden result recorded as failed with
`overridden_by`. In v1 every `pr_merge` requirement is hard.

### D7. The charter performs no external I/O; the existing merge verb claims admission

The charter pack holds no platform credential and makes no network call. `git.pr_merge` remains the
effect path. For a repository enrolled in an enforced charter:

1. The initiating actor must be authorised for the phase; the verb claims an admission
   (`charter.claim`) for the exact action descriptor (repository, pull request, expected head, method,
   target, subject and body digests) before it sends.
2. The claim re-evaluates every action requirement against a fresh bundle and current authority in one
   transaction. A consumed admission is not a bearer token another actor can redeem.
3. `git.pr_merge` writes its existing pre-send receipt, which references the attempt id, and sends with
   the platform's expected-head comparison. It never passes the administrator flag for an enrolled
   repository and never enables delayed auto-merge on an admission.
4. The result is reconciled against the git receipt and an independent platform read
   (`charter.result`). A timeout or lost response leaves the attempt `uncertain`; there is no blind
   resend. `git.reconcile` is the reconciliation source; the charter does not keep a second send
   marker.

The merge credential must belong to a designated executor, not to initiating actors. Today
`git.pr_merge` sends with the caller's own credential reference; enforced mode therefore needs an
amendment to ADR-182 binding merge on enrolled repositories to the executor's credential. Credential
production and custody stay outside this repository (ADR-192); this record defines only the seam that
refuses without an admission.

A merge observed on the platform with no matching admitted attempt is recorded as a violation. It is
never converted into a compliant completion.

### D8. Identity separates subject, candidate generation, run and attempt

- The **logical subject** is immutable: forge, repository id, pull request id and target. Names are
  display attributes.
- The **candidate** is the source repository, head, target base and tested integration commit. A new
  head, a changed base or integration commit, or a reopen after a terminal disposition starts a new
  **generation**. A server-owned `subject_epoch` increases on each authorised reconciliation that
  establishes such a change, so a head that moves A→B→A gets a new epoch.
- `run_key = H(protocol, policy_domain, charter_id, definition_digest, subject, subject_epoch,
  candidate)` under a versioned canonical encoding with the existing content-hash primitive.
- One subject row points at the single eligible run across definitions. The subject row, run and phase
  revisions, evidence append and command record change together under compare-and-set. A new
  generation supersedes the old run; phases are never reset.
- Every mutating verb takes a caller-scoped `request_id`. The same key with the same bytes returns the
  stored result; the same key with different bytes refuses. Reading back a consumed admission is
  history, never a renewed right to send.

### D9. Authority comes from the accepted grant seams, not from tool grants

Consequential authority (merge initiation, ADR approval, a future override) is consumed through the
ADR-127 and ADR-143 seams: assurance-bearing identity, canonical action descriptors, atomic
consumption, and startup refusal when a protected action lacks its authenticator or grant service.
That substrate is accepted and not yet implemented, so the enforced milestone delivers the minimum of
it that the merge action needs, as that implementation, not as a charter-local substitute. A tool-level
`allow` stays one prerequisite and never satisfies an exact-subject approval by itself. Existing tool
grant patterns and precedence are not reinterpreted.

Model family, controlling principal and platform account for authors and reviewers are recorded by an
authenticated issuer at execution time and bound to the revision they touched. An editable property
on an actor is not evidence of family.

### D10. A run pins its definition; revocation governs admission

A run pins the definition bytes, schema version, gate registry and implementation digests, action
contract version and each evaluation's inputs and time. New definition versions apply to new
generations. Revoking a definition, a producer or a grant, or raising the minimum policy epoch,
invalidates every unconsumed admission under it in the same serialisation order as claims. Migration
means superseding the run, never mutating it. Historical replay reports `ReplayUnsupported` rather
than silently using today's gate code.

### D11. Two milestones, and only the second is enforcement

- **M1, recording.** Definitions, runs, observations, pure evaluation with replay, the waiting view,
  and the transition matrix. Every run reports `assurance = recording_only` and `charter.claim` refuses.
  M1 makes the procedure visible and measurable; it does not enforce `pr_merge`.
- **M2, enforced `pr_merge`.** M1 plus: the minimum ADR-127/143 authenticated grant path for merge
  initiation and approval; the `git.pr_merge` admission binding and executor credential (ADR-182
  amendment); verified platform protection for enrolled repositories (required checks pinned to their
  producing application, no administrator bypass available to initiating actors, platform approval
  settings consistent with the charter); and the acceptance cases below passing, including the
  negative merge paths. M2 is not complete until every in-scope automated merge route refuses without
  an admission.

## `pr_merge/v1`

Gate groups, all hard in v1:

| Gate                      | Evidence and predicate                                                                                                                                                                                                                                                                                   | Blocks when                                                                                                                                                                                                                                                   |
| ------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Required checks           | Union of the platform's effective rules and the definition's floor, never a subtraction. Full check inventory bound to repository, pull request, head, base and tested integration commit. Match name **and** producing application, workflow and event, and the tested revision. Terminal success only. | A required name is absent, red, pending, skipped, neutral, cancelled, from another producer or event, or bound to another revision; the required set is empty or unknown; the inventory is partial. A newly added live requirement appears through the union. |
| Review at head            | Authorised review attestation with an approve verdict, the exact reviewed head and scope digest, and reviewer execution identity, plus the current platform review state.                                                                                                                                | New head, dismissal, an effective rejection, or missing proof.                                                                                                                                                                                                |
| Family independence       | Author manifest and reviewer execution attestation from the issuer's closed taxonomy. Reviewer family differs from every producing family on the change.                                                                                                                                                 | Unknown family, missing contributor provenance, contradictory manifests.                                                                                                                                                                                      |
| ADR sign-off              | Complete path manifest of the branch diff against the target merge base (old and new paths for renames). Any path under `docs/adr/` requires an exact, head- and scope-bound approval from the named architecture authority.                                                                             | Truncated diff, missing or revoked approval, changed scope. `not_applicable` only after completeness is proven.                                                                                                                                               |
| No self-approval          | Approver and ADR approver disjoint from the production actor roots, from the pull request author's and last pusher's platform accounts, and from their controlling principal.                                                                                                                            | Missing identity mapping, a shared controller, an alias or sub-actor.                                                                                                                                                                                         |
| Scope and protected paths | The exact path-and-status set and its digest equal the scope approved with the review; the rendered body digest binds the summary to it. Changes to CI definitions, charter definitions, authority maps or effect adapters need the policy authority's attestation.                                      | Changed set, edited body after review, incomplete diff. File-count equality is diagnostic only.                                                                                                                                                               |
| Action admissibility      | Fresh open, non-draft state; supported target and method; known mergeability; current authority; the exact action descriptor naming the expected head.                                                                                                                                                   | Unknown mergeability, closed or draft, changed candidate, revoked authority, expired claim deadline.                                                                                                                                                          |

Proposed freshness bounds, evaluated on server time and tightenable in data: dynamic platform
inventories at most 60 s old at claim, a collection span of at most 30 s, at most 5 s of future skew,
and at most 15 s from claim to send. These are design values, not measured service guarantees.

The definition, as data (values ending in `_CONFIGURED` are deployment parameters):

```json
{
  "schema_version": 1,
  "charter_id": "pr_merge",
  "template_id": "pr_merge/v1",
  "version": 1,
  "gate_registry_digest": "BUILD_MANIFEST_DIGEST_CONFIGURED",
  "action_contract": "git_pr_merge/v1",
  "repositories": ["IMMUTABLE_REPOSITORY_ID_CONFIGURED"],
  "principals": {
    "policy_authority": "POLICY_AUTHORITY_CONFIGURED",
    "architecture_authority": "ARCHITECTURE_AUTHORITY_CONFIGURED",
    "identity_issuer": "IDENTITY_ISSUER_CONFIGURED",
    "merge_initiator_role": "merge_chair",
    "merge_executor": "MERGE_EXECUTOR_CONFIGURED",
    "recovery_role": "merge_recovery"
  },
  "freshness": {
    "dynamic_max_age_seconds": 60,
    "max_collection_span_seconds": 30,
    "max_future_skew_seconds": 5,
    "claim_start_deadline_seconds": 15
  },
  "check_policy": {
    "required_source": "effective_rules_union_definition_floor",
    "floor": [
      { "name": "CI gate", "producer": "APP_AND_WORKFLOW_ID_CONFIGURED" },
      { "name": "Secret scan (gitleaks)", "producer": "APP_AND_WORKFLOW_ID_CONFIGURED" }
    ],
    "allowed_event": "pull_request",
    "tested_revision": "integration_commit"
  },
  "phases": [
    {
      "id": "validate",
      "await": "candidate_observed",
      "assignee": { "role": "merge_recovery" },
      "gates": ["candidate_supported/v1", "evidence_complete_trusted/v1", "required_checks/v1"],
      "action": "record_completion"
    },
    {
      "id": "approve",
      "await": "predecessor_completed",
      "assignee": { "role": "independent_reviewer" },
      "gates": [
        "review_at_head/v1",
        "family_independent/v1",
        "no_self_approval/v1",
        "adr_scope_approved/v1",
        "scope_and_policy_paths_approved/v1"
      ],
      "action": "record_completion"
    },
    {
      "id": "merge",
      "await": "predecessor_completed",
      "assignee": { "role": "merge_chair" },
      "gates": [
        "all_merge_requirements_current/v1",
        "exact_action_authorized/v1",
        "platform_enforcement_verified/v1"
      ],
      "action": { "kind": "git_pr_merge", "method": "squash", "expected_head": "candidate_head" }
    }
  ]
}
```

`candidate_head` selects an immutable candidate field; it is not an expression.
`all_merge_requirements_current/v1` re-evaluates every group against a fresh bundle at claim and cannot
be configured to trust earlier pass bits.

## Verb surface

All verbs register through the pack, answer `help=true`, reject unknown fields, and return typed errors
with a stable `reason`. Read verbs return records directly. Visibility never implies permission.

| Verb                                 | Purpose                                                                                                                                          |
| ------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| `charter.publish` / `charter.revoke` | Publish an immutable definition, or revoke one and raise the policy epoch. Policy authority only.                                                |
| `charter.definition`                 | Read a pinned definition and its activation state.                                                                                               |
| `charter.trigger`                    | Open or return the run for a subject and candidate observation. The runtime picks the active definition; the caller cannot select a retired one. |
| `charter.observe`                    | Submit an observation under the caller's producer registration.                                                                                  |
| `charter.evaluate`                   | Read-only diagnostic evaluation, or deterministic replay of a stored evaluation. Grants nothing.                                                 |
| `charter.advance`                    | Evaluate from a consistent snapshot and record the transition. No external effect.                                                               |
| `charter.waiting`                    | The derived waiting view for an actor or a held role, paginated.                                                                                 |
| `charter.get` / `charter.evidence`   | Read a run, or its evidence in sequence order with the same-snapshot head.                                                                       |
| `charter.claim`                      | Effect adapter only: re-evaluate and admit one exact action descriptor, or refuse.                                                               |
| `charter.start`                      | Bound executor only: move a claimed attempt to dispatching immediately before the git pre-send receipt.                                          |
| `charter.result`                     | Reconcile an attempt against the git receipt and a platform read.                                                                                |
| `charter.cancel`                     | Cancel when no effect is in flight; otherwise record intent and require reconciliation.                                                          |

Typed errors include `InvalidDefinition`, `UnsupportedTemplate`, `DefinitionRevoked`,
`PermissionDenied`, `AssuranceInsufficient`, `ProducerNotAuthorized`, `EvidenceConflict`,
`EvidenceIncomplete`, `StaleEvidence`, `SubjectChanged`, `RevisionConflict`, `IdempotencyConflict`,
`IllegalTransition`, `GateRefused`, `GrantUnavailable`, `ActionAlreadyClaimed`,
`ActionOutcomeUnknown` and `ReplayUnsupported`. A refused claim reports `action_admitted=false` and
whether its diagnostic evaluation committed.

## Tables

`charter_definitions` (immutable bytes and digests per `(policy_domain, charter_id, version)`, with
separate activation and revocation records), `charter_subjects` (one row per logical subject with
revision, epoch, current candidate, eligible run and in-flight attempt), `charter_runs` (unique
`run_key`, pinned definition, candidate, state, current phase, revision), `charter_phases` (typed state
and assignee per phase; only the current phase is actionable), `charter_evidence` (per-run sequence,
kind, producer, source identity, interval, completeness, digest, supersedes), `charter_attempts`
(descriptor digest, evaluation, grant consumption, executor, deadline, git receipt reference, outcome;
at most one unresolved attempt per subject) and `charter_commands` (caller, verb, `request_id`,
request digest, disposition).

Every transition is `UPDATE ... WHERE revision = ? AND state = ?` inside the writer transaction that
also reads the subject, definition and authority state. Zero affected rows is a conflict, never
success. Identity, family and controller mappings live in the shared identity and authority services;
the charter stores the snapshots it evaluated, not another editable directory.

## State machine

Run states: `open | completed | cancelled | superseded | invalidated | failed`. Phase states:
`dormant | waiting_trigger | waiting_gate | waiting_actor | ready | executing | uncertain | completed`.

| From                | To                                  | Trigger                                            | Guard                                                                 | Idempotency and recovery                                                         |
| ------------------- | ----------------------------------- | -------------------------------------------------- | --------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| none                | open, first phase `waiting_trigger` | `trigger`                                          | Authorised trigger, active definition, subject CAS                    | Unique run key; equal replay returns the run, different bytes conflict.          |
| `waiting_trigger`   | `waiting_gate`                      | matching trigger / `advance`                       | Predecessor complete, current candidate                               | Trigger consumption and transition commit together.                              |
| `waiting_gate`      | `waiting_gate` / `waiting_actor`    | `advance`, requirement unmet                       | Evaluation completed                                                  | Evaluation and reason recorded.                                                  |
| `waiting_*`         | `ready`                             | `advance`, all hard requirements pass              | Live definition and candidate, complete trusted bundle                | `ready` is provisional and grants nothing.                                       |
| `ready`             | `waiting_*`                         | contradictory evidence or expiry                   | No admitted attempt                                                   | Invalidation recorded; no sweeper needed for safety.                             |
| `ready`             | `completed`, next phase active      | internal `record_completion`                       | Requirements still pass in the same unit                              | Result, evidence and next activation commit together.                            |
| `ready`             | `executing` (claimed)               | `claim`                                            | Full re-evaluation, exact descriptor, no competing unresolved attempt | One unit writes evaluation, consumption, attempt and transition.                 |
| claimed             | dispatching                         | `start` by the bound executor                      | Deadline, authority and candidate unchanged                           | Persisted before the git pre-send receipt; loss after this point is `uncertain`. |
| dispatching         | `completed`                         | `result` with a matching receipt and platform read | Receipt matches the attempt, expected head, method and executor       | Equal receipt replay is a no-op.                                                 |
| `executing`         | `waiting_gate` / `failed`           | proven non-execution or definitive refusal         | No live sender                                                        | New attempt needs a new claim; consumed grants stay consumed.                    |
| `executing`         | `uncertain`                         | timeout, process death, receipt loss               | An effect may have happened                                           | Exclusive until reconciled; never returns to `ready` automatically.              |
| unsent, nonterminal | `superseded` + new run              | new candidate generation                           | Subject CAS                                                           | Old evidence kept; old claims cannot act on the new head.                        |
| unsent, nonterminal | `invalidated` / `cancelled`         | revocation / authorised cancel                     | Serialised before any admission                                       | History kept.                                                                    |
| any                 | same                                | host restart                                       | Persisted state read                                                  | No replay of effects; `executing` without proof of non-send becomes `uncertain`. |

## Relationship to existing decisions

- **ADR-066** says the CI gate wall alone authorises a merge, with no per-change approver. For a
  repository enrolled in an enforced `pr_merge` charter this record supersedes that authorisation
  model: independent review and conditional architecture approval are required. ADR-066's required
  context floor is absorbed as policy input. ADR-066 receives a pointer amendment when this record is
  accepted, so two normative merge policies are never active for the same repository.
- **ADR-102** is unchanged. It governs knowledge-graph change sets, not platform pull requests.
- **ADR-142** contributes its matrix discipline and replay-key idea, not its table or restart policy.
- **ADR-182** is amended at M2: for enrolled repositories `git.pr_merge` claims a charter admission,
  sends with the executor's credential, and never passes the administrator flag. Its receipts and
  `git.reconcile` become the charter's send record and reconciliation source.
- **ADR-127 and ADR-143** are consumed, not replaced; M2 implements the part of them the merge action
  needs. Their stated limits on same-principal isolation carry over unchanged.
- **ADR-192**: credential custody stays outside this repository; the charter defines the refusal seam.

## Acceptance

Paired positive and negative cases, each run against the real verbs:

1. A valid candidate completes with its evidence, evaluated definition and attributable merge receipt;
   replaying the trigger, command or receipt causes no second effect.
2. A required check that is absent, red, skipped, stale, from the wrong producer, event or tested
   revision blocks; a newly required check blocks until present; a partial inventory never proves
   "nothing required".
3. Head change after review, base change, A→B→A, retarget, reopen, a delayed old webhook and two
   concurrent definitions: none redeems an old admission.
4. Review dismissal, same family, same actor root, one controller behind two accounts, missing family
   provenance, truncated diff, ADR rename or delete, body edit, and a same-count different-file scope
   each block.
5. An unregistered producer, a forged producer field and a disabled producer refuse; generic note and
   stream writes cannot create charter evidence.
6. A tool-level allow does not bypass a failed gate; an expired or revoked grant refuses; two
   concurrent claims yield one admission.
7. A failed evidence or claim insert commits nothing; a CAS loser commits no transition; a response lost
   after commit resolves through the same `request_id`.
8. Crash before `start`, after `start` before send, after platform success before `result`, and during
   recovery: none returns to `ready` or lets a second attempt send.
9. (M2) `git.pr_merge`, the direct platform API, auto-merge and a wrong-account path without an
   admission all refuse or are unreachable with the executor's credential. Platform administrators who
   can change protection are outside this guarantee, and that is stated in the deployment profile.
10. Revocation before and after the claim's serialisation point gives the two different recorded
    outcomes; restoring a snapshot with a formerly consumed attempt cannot act under a stale epoch.

M1 passes cases 1-8 and 10 with `charter.claim` refusing; M2 passes all ten.

## Alternatives considered

- **Extend GTD** (a run as a task, phases as child tasks). Rejected: a child task can be completed
  outside the gate, and authorisation becomes a task-tree convention.
- **A shared state-machine substrate now.** Rejected for v1: an abstraction project before the second
  domain is understood, coupling process teardown to obligations that must survive a restart.
- **Gates that read the platform directly.** Rejected: evaluation becomes irreproducible, network calls
  run under transition locks, and replay gives different answers.
- **The event store as the authority ledger.** Rejected: its append path does not share the pack's
  transaction, and its projections may lag.
- **A charter-local executor holding the merge credential.** Rejected: it duplicates the git adapter
  and couples a workflow engine to credential custody.
- **Extending tool grants into capabilities.** Rejected: it would imply exact-subject semantics that the
  pattern-and-precedence model does not have, and create a second capability system beside ADR-127.
- **Recording only, declared done.** Acceptable only as the named M1 milestone: a procedurally invalid
  merge remains possible through ordinary automation until M2.

## Known limits

- **Authority is the expensive part.** The accepted grant substrate is unimplemented, and actors
  sharing one operating-system principal are not isolated from each other. A pack alone cannot deliver
  an enforced merge.
- **The remote boundary is a snapshot.** The platform enforces its own required checks and the expected
  head at merge time; the charter's other predicates are asserted at claim with a short deadline. A
  guarantee that every custom predicate still holds at the platform's commit would need a
  platform-supported reservation, which this design does not have.
- **Provenance can be collected wrongly while still parsing.** Author manifests, the tested revision,
  effective rules and account mappings need one shared instrument implementation per kind, negative
  fixtures for omission, reordering and forgery, and protected issuer configuration. A passing procedure
  does not prove the review was good.

Deferred: soft override activation; charter topologies beyond the template; any definition language;
generic action strings and internal action adapters; watch-and-compare triggers (an existing periodic
consumer refreshes observations and reconciles attempts); cross-charter cascades; thresholds and SLA
logic; hash chaining and external anchoring; merge-queue and stacked pull request modes; in-place
migration of runs.

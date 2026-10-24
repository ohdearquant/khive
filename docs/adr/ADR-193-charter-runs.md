# ADR-193: Charter Runs — Procedural Actions Admitted Only on Recorded Evidence

- **Status**: Accepted (2026-09-22)
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

The sentence "There is no language" is superseded by [ADR-208](ADR-208-charter-workflows.md) (accepted 2026-10-22).

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

Superseded by [ADR-208](ADR-208-charter-workflows.md) (accepted 2026-10-22), which replaces the closed template with charter workflows over registered phrases.

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
never refreshes the last good sample. Amendment 5 specifies the producer registration, the producer
classes and submission channels, and which recorded evidence remains eligible.

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
  history, never a renewed right to send. Idempotency does not preserve a disabled producer
  registration: after the disable, a retry of the same observation refuses, and the stored record
  stays readable as history (Amendment 5 D5).

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
on an actor is not evidence of family. The deployment profile names the issuer's closed family
taxonomy (one value per model provider, plus `human`). Under M2 a review from the same family as any
producer of the change refuses. This gate is hard in every deployment.

Controller independence is a per-definition datum, `independence.controller`, and `pr_merge/v1`
defaults it to `required`. A deployment in which one principal controls every actor and platform
account may declare it `not_required` in its definition; its deployment profile then states the
residual risk in one sentence, and independence is established by actor root and family alone.

### D10. A run pins its definition; revocation governs admission

A run pins the definition bytes, schema version, gate registry and implementation digests, action
contract version and each evaluation's inputs and time. New definition versions apply to new
generations. Revoking a definition, a producer or a grant, or raising the minimum policy epoch,
invalidates every unconsumed admission under it in the same serialisation order as claims. Migration
means superseding the run, never mutating it. Historical replay reports `ReplayUnsupported` rather
than silently using today's gate code. Revoking a producer is the disable of its Amendment 5
registration; under M2 it is rechecked at claim and at start (Amendment 5 D6).

### D11. Two milestones, and only the second is enforcement

- **M1, recording.** Definitions, runs, observations, pure evaluation with replay, the waiting view,
  and the transition matrix. Every run reports `assurance = recording_only` and `charter.claim` refuses.
  M1 makes the procedure visible and measurable; it does not enforce `pr_merge`. M1 still refuses
  unregistered evidence; Amendment 5 D7 states its cooperative recording profile and implementation
  order.
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
| No self-approval          | Approver and ADR approver disjoint from the production actor roots, from the pull request author's and last pusher's platform accounts, and, when `independence.controller` is `required`, from their controlling principal.                                                                             | Missing identity mapping, a shared controller, an alias or sub-actor.                                                                                                                                                                                         |
| Scope and protected paths | The exact path-and-status set and its digest equal the scope approved with the review; the rendered body digest binds the summary to it. Changes to CI definitions, charter definitions, authority maps or effect adapters need the policy authority's attestation.                                      | Changed set, edited body after review, incomplete diff. File-count equality is diagnostic only.                                                                                                                                                               |
| Action admissibility      | Fresh open, non-draft state; supported target and method; known mergeability; current authority; the exact action descriptor naming the expected head.                                                                                                                                                   | Unknown mergeability, closed or draft, changed candidate, revoked authority, expired claim deadline.                                                                                                                                                          |

Proposed freshness bounds, evaluated on server time and tightenable in data: dynamic platform
inventories at most 60 s old at claim, a collection span of at most 30 s, at most 5 s of future skew,
and at most 15 s from claim to send. These are design values, not measured service guarantees;
acceptance case 11 measures them before they become hard.

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
  "independence": { "controller": "required" },
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
  context floor is absorbed as policy input. ADR-066 Amendment 1 lands with this record and
  states this, so two normative merge policies are never active for the same repository.
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
4. Review dismissal, same family, same actor root, one controller behind two accounts (default
   definition), missing family provenance, truncated diff, ADR rename or delete, body edit, and a
   same-count different-file scope each block. The same one-controller fixture under a definition with
   `independence.controller` set to `not_required` admits, which proves the datum is read.
5. An unregistered producer, a forged producer field and a disabled producer refuse; generic note and
   stream writes cannot create charter evidence. Amendment 5 states the registration contract and the
   paired controls for this case.
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
11. (M2, before the freshness bounds become hard) Ten real claim-to-send samples against the platform,
    each recorded with its measured inventory age and claim-to-send latency. The bounds are enforced as
    hard only when every sample fits inside them with margin; otherwise M2 would refuse every merge on
    a slow platform day while appearing to work.

M1 passes cases 1-8 and 10 with `charter.claim` refusing; M2 passes all eleven.

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

Of these, "charter topologies beyond the template", "any definition language" and "cross-charter cascades" are
superseded by [ADR-208](ADR-208-charter-workflows.md) (accepted 2026-10-22).

## Amendment 1 (2026-09-22): the merge method is a per-repository datum

**Status**: Accepted (2026-09-22).

The `pr_merge/v1` definition fixes the merge phase's action to `"method": "squash"`. Some repositories
must merge by merge commit, for example where a deployment platform refuses a head whose commit was
re-authored by a squash, so a template-wide constant cannot enrol them. Each enrolled repository now
carries its method: `repositories` is a list of `{ "id": <immutable repository id>, "merge_method":
"squash" | "merge" | "rebase" }`, and the merge phase's action takes its method from the subject's
enrolment row instead of a literal. The "supported target and method" predicate of the action
admissibility gate reads that row. `charter.claim` refuses a descriptor whose method differs from the
enrolment row, and refuses when the platform's allowed methods for that repository, as observed, do not
include it.

Acceptance (case 12, M2; its refusal arms also run in M1 through `charter.evaluate`): a repository
enrolled with `merge` admits a merge-commit descriptor and refuses a squash descriptor for the same
candidate; a repository enrolled with `squash` gives the mirror result; a method that the platform
disallows refuses even when the enrolment row names it.

## Amendment 2 (2026-09-22): the merge verb produces the claim-time bundle

**Status**: Accepted (2026-09-22).

The freshness bounds require dynamic inventories at most 60 s old at claim, and D3 keeps gates free of
external reads, but the record did not name who observes inside that window. An instrument that observes
on a schedule longer than the bound serves the waiting view and pre-claim evaluation, never the claim.
The producer of the claim-time bundle is the merge path itself: immediately before `charter.claim`,
`git.pr_merge` reads the pull request snapshot and the check inventory at the expected head and submits
them through `charter.observe` under the merge executor's producer registration, then claims. The charter
still performs no I/O. The facts come from the platform, not from the executor: the `check_inventory`
and `pr_snapshot` observations (Amendment 3) that the merge path submits carry the platform response
identity (the platform's request id or entity tag, and the response time) in the D3 envelope's source
event field, so the executor's registration attests the relay and the platform attests the facts. A
claim-time bundle whose source identity is the executor itself, with no platform response identity,
refuses. Rejected alternative: the initiator's instruments observing just before the
initiator calls the verb, which puts two processes and an unbounded gap between the read and the claim.

Acceptance (case 13, M2): a claim whose only check inventory is older than the bound refuses with
`StaleEvidence`; the same claim made through `git.pr_merge`, which observes first, admits; an observation
submitted under a registration that is not the executor's cannot stand in for the claim-time bundle; a
bundle submitted under the executor's registration without a platform response identity refuses.

## Amendment 3 (2026-09-22): observation kinds publish their schemas

**Status**: Accepted (2026-09-22).

`charter.observe(help=true)` returns the JSON schema of every observation kind, and each kind lists its
required fields:

- `check_inventory`: repository id, head, the required contexts by name with the rule source each came
  from (ruleset or classic protection) and its base scope, and per context the producing application and
  workflow ids, event, run id, conclusion and completion time.
- `pr_snapshot`: repository and pull request ids, state, draft flag, head, base ref and sha, mergeable
  state, the repository's allowed merge methods, author and last-pusher account ids, and per reviewer
  the review state and the commit it names.
- `diff_manifest`: merge base, head, the path-and-status set (old and new paths for renames) and its
  digest under the canonical encoding.
- `review_attestation`: reviewer identity, reviewed head, scope digest, verdict, execution identity and
  payload digest.

Every kind also carries the D3 envelope: source identity, source event or attempt identity, observation
interval, completeness and payload digest.

Until the identity issuer exists (M1), a registered review producer may attest from its own verdict
record: payload digest = the record's digest, reviewed head = the head the record states it evaluated,
execution identity = the executor the record names. Such an attestation carries assurance
`self_reported`. It is recorded and evaluated in M1 and never satisfies `review_at_head` or family
independence under M2. The M1 evaluation summary, and the `charter.waiting` row of Amendment 4, name the
assurance of every attestation a passing gate consumed, so an M1 pass that rests on a `self_reported`
attestation reads as such and never as verified.

Acceptance (case 14, M1 and M2): an observation missing a required field of its kind refuses and names
the field; a `self_reported` attestation passes `review_at_head` in an M1 evaluation, and the evaluation
output names `self_reported` beside the passed gate; the same attestation fails the gate under an M2
definition.

## Amendment 4 (2026-09-22): the waiting view carries a cursor and the last evaluation

**Status**: Accepted (2026-09-22).

A consumer that ticks on a timer compares against its last reading. `charter.waiting` rows now carry the
run's revision (its evidence sequence head) and the last evaluation summary `{phase, status,
failing_gate_ids, reason}`, and the verb takes `changed_since=<cursor>`, where the cursor is opaque and
server-issued over the store's commit order; every response returns the next cursor. The caller holds the
cursor, and the server keeps no per-consumer state, so D5's "no read or acknowledge state" still holds. A
row whose run changed after the cursor is returned even if it changed back.

Acceptance (case 15, M1 and M2): after a tick, a second call with the returned cursor and no intervening
change returns no rows; a run that advanced and a run that was invalidated both appear with their new
revisions; a run that moved A to B and back to A appears.

With Amendments 1-4, M1 also passes cases 14 and 15 and the refusal arms of case 12; M2 passes all
fifteen.

## Amendment 5 (2026-10-22): producer registration and evidence eligibility

**Status**: Accepted (2026-10-22).

This amendment serves the project owner's instruction of 2026-10-22, rendered: "So they connect:
charter + gate + privileges, the whole automated permission system has nearly taken shape in khive". It
also supplies the registration that acceptance case 5 has required since this record was accepted on
2026-09-22.

This amendment amends ADR-193 D3, D8, D10, D11 and acceptance case 5; ADR-208 D3, D5 and
D6; ADR-143's caller-right vocabulary and administration constraints, as specified below. It relates to
ADR-007 (namespace attribution), ADR-018 (the Gate), ADR-127 (identity assurance and shared authority),
ADR-180 (tool-use policy), and proposed ADR-209 (root policy exceptions).

### Context

Permission to invoke `charter.observe` is not permission to record every kind
of evidence. ADR-193 requires the caller to determine the producer and requires
a registration restricting its observation kinds and repository scope. ADR-208
adds generic subjects and a producer class to each vocabulary entry. The class
describes the expected source; it does not enroll a principal.

Three contracts must agree: the Gate's permission to submit, the source contract
of the pinned vocabulary entry, and the evaluator's decision about which
recorded evidence remains eligible. Checking only the first leaves role,
subject and source substitution open. Checking only during evaluation permits
unauthorized submissions to change generations or release triggers before a
requirement is evaluated.

The Gate receives the caller, verb and request arguments, not resolved target
records. ADR-127 separates that check from the transactional discharge of a
subject-bound obligation. ADR-143 supplies durable caller authority and its
administration. ADR-180 tool grants do not supply this contract: a grant's scope
is descriptive, and a grant takes precedence over a deny policy.

The recording milestone still refuses invalid evidence. Its absence of effect
enforcement is not an exception to producer admission. A deployment whose
processes share an OS user also needs an explicit limit: checking a configured
principal against a registration does not authenticate that process against
another process able to change configuration or open the store.

### Decision

#### D1. Registration is a standing right in shared caller authority

A producer registration is a typed, reusable `RecordObservation` right managed
by the shared authority service used by the Gate. It specializes ADR-143 caller
authority. It is not an ADR-180 tool grant and is not a single-use resource or
exception approval. Recording an observation does not consume the registration.
This adds a constrained right to the shared service; it does not redefine
existing caller rights or the purposes of single-use approvals.

The authority service owns the registration records, liveness and administration
history. The charter pack owns only its evidence and the authority snapshots it
consumed. No separate `charter.register`, `charter.disable` or charter-local
issuer is introduced. Registration, disable and bounded listing are operations
of the shared grant-administration surface, specialized by this right type.
Their wire names must be declared with that surface before implementation;
this amendment does not assert that such verbs already exist.

Each immutable registration contains:

- Its identity, schema version, exact structural grantee, issuer and delegation
  lineage, policy domain, issuance assurance and creation time.
- A finite, nonempty set of observation permissions. Each permission names an
  exact entry name and entry-contract digest, its producer class and allowed
  submission channel, and one typed subject selector from D4.
- For external instruments and relays, the exact source contract and allowed
  source identities. Where a kind has authority-relevant arguments, a finite
  set of exact typed argument bindings is mandatory. For example, permission
  for `gate.result(name="lint")` does not permit another named check; a floor
  check also pins the check's producing application identity.
- For actor reports, the reporting binding specified in D3. For a claim-time
  platform bundle, the designated executor and bundle purpose.
- A validity interval: `not_before` is required; `expires_at` is either an
  explicit instant or explicitly absent for a standing permission.

Selectors are closed data with exact equality and bounded collections. There
are no patterns, predicates or expressions in registrations. One complete
permission must match; the service cannot combine the kind from one permission
with the repository, arguments or source identity from another. If several
complete permissions match, choose the lexicographically smallest registration
identity and permission index, and retain that choice in the receipt.

The shared administration rule is attenuation: granting requires both the
ordinary right being delegated and administration over its entire constrained
scope. Possessing an observation right alone confers no administration. A
multi-permission change is all-or-nothing. Disable authority is checked against
the stored permission set, not a smaller set asserted by its requester.
ADR-143's structural identity, parent restrictions, lineage validation and
transactional change history apply. Publishing a vocabulary or starting a run
confers no observation right.

Durable capabilities are minted through narrowing delegation and verified on
load, including their seals and live root anchor. A missing verification key
is unavailable, not permission. A local service-held seal authenticates that
service's record within its declared boundary; it does not establish
`ActorSignature` or distinguish peers who can read the same key material.

Disable appends a terminal lifecycle record for that registration. Replacing or
renewing permission creates a new identity; there is no in-place widening or
reactivation. Registration and lifecycle changes, their authorizing context and
reason, and the shared authority revision commit together. Failed history
persistence aborts the change. A generic record or stream write cannot create,
alter or disable this authority.

This requires the minimum shared standing-right service and its Gate integration.
It is not satisfied by the presence of accepted ADRs. The cooperative recording
profile in D7 is an explicitly weaker identity profile; it does not claim the
complete ADR-127/143 authenticated implementation or substitute a second store
for it.

#### D2. The caller fixes the producer; the source is a different identity

For every external observation, `producer` is the structural principal from the
sealed dispatch context. Remove the top-level `producer` request parameter and
refuse it whenever present, including when it equals the caller. Do not silently
ignore it or reinterpret it as a source label. An entry's declared argument
named `producer`, such as a floor check's application identity, remains an
argument of that entry and never controls the observation's producer.

The envelope records source identity separately: the instrument or platform,
source event or attempt, observed interval, completeness, source payload digest,
and server receipt time. Source assertions are checked against the registration
and the kind's versioned schema. Free-form detail carries no producer,
authorization, completeness or assurance semantics. Assurance and receipt time
are derived by the service, never accepted from the request.

A relay is the authenticated submitting principal, not a second caller-selected
producer. A platform observation carries the platform response identity, such
as a request identifier or entity tag with response time, and a retained
response digest bound to the subject and candidate. Such an identifier is a
correlation value, not a cryptographic platform attestation. Trust comes from
the authenticated, authorized relay and its verified collection path. A relay
can still lie within its authority; registration alone cannot detect that.
This qualifies ADR-193 Amendment 2's statement that the platform attests the
facts: a plain response identifier does not prove their origin independently.

The claim-time bundle remains restricted to the designated executor's relay
registration. Another registered platform relay cannot substitute for it.
Caller identity assurance, source assurance, and the run's `recording_only`
assurance are separate fields and must remain separately visible.

Internal producers use a tagged internal origin with a compiled adapter
identity and digest. They are not external principals named `store` or
`charter`. A public request cannot construct this origin. A store sample also
records `requested_by` from the real caller; the caller is not relabeled as the
internal producer.

#### D3. Classes constrain the source contract, not a principal's label

The registry accepts exactly `actor`, `instrument`, `platform` and `system`.
Unknown classes refuse compilation. Each entry also has a versioned submission
contract specifying its channel and evidence schema. These fields enter its
contract digest and the registry digest. A class, entry name or channel never
grants permission by itself. Published vocabulary data cannot install an
internal adapter or overwrite a compiled entry.

Subject declarations are not observation kinds. Their class does not make
them admissible through `charter.observe`. Engine-generated lifecycle and
child-completion names likewise cannot be enrolled as external observation
kinds.

| Class        | Admission contract                                                                                                                                                                                                   |
| ------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `actor`      | An explicitly registered caller reports for one named run and an authorized reporting binding. Action results additionally name the exact action instance and visit.                                                 |
| `instrument` | An external registered instrument may use `charter.observe` only for its exact kind, source, argument bindings and subject scope. Entries assigned to a built-in store reader are produced only by `charter.sample`. |
| `platform`   | A registered relay supplies the platform envelope required by D2. A compiled reader of already stored platform-derived data may instead use `charter.sample`, with its store provenance explicit.                    |
| `system`     | Only a compiled internal transition or a specifically registered built-in store reader produces the observation. Public `charter.observe` always refuses.                                                            |

For actor phrases and triggers, the permission binds an exact definition digest,
entry and declared reporting role; that role must resolve to the caller in the
run's immutable bindings. Admission policy must select one reporting role for
each such entry in a definition. An ambiguous or absent mapping refuses run
admission rather than inferring the role from the phase owner or accepting any
run participant. The mapping is fixed policy data, not a charter expression.
Procedures needing different reporters for the same entry must use distinct
entries until a separately specified per-use binding exists.

For action results, the expected reporter is the actor bound to the action's
explicit `by` role, or to the phase owner if `by` is absent. If neither exists,
use the phase's persisted initiating-principal assignee. An explicitly named
but unbound role refuses; it does not fall back to the initiator. Results bind
the run, phase visit, action occurrence and exact arguments. A prior visit's result
cannot complete a later action. Actor phrases may be recorded ahead of a future
phase in their named run, preserving ADR-208's first-visit evidence rule.
They do not fan out to other runs on the subject.

Run creation authorizes its proposed role bindings through the same shared
policy architecture. Merely writing oneself into `bindings` is not enrollment.
Role membership and producer permission are intersected, never substituted for
one another. Changing a reporting assignment requires a new authorized run;
old evidence retains its original binding.

The compiled store-reader mapping includes `task.status`,
`graph.edge_present`, `inbox.unread_older_than`, `memory.recorded` and
`loop.log_written`. It also includes the platform-class `message.arrived`
and system-class `schedule.fired` on their supported store subjects. These
exceptions are explicit channel declarations, not permission for every kind in
either class to use a store reader. `charter.sample` accepts no caller-selected
outcome and cannot invoke an arbitrary instrument. Its caller still requires
Gate permission for the sampling operation and scope. Internal lifecycle and
child-completion observations remain confined to the engine's typed transition
path.

A sample establishes what the store contained. It does not authenticate the
earlier write that put that state there. Reading a fabricated message or edited
task through a trusted reader must not promote it to independently verified
external evidence. Likewise, registration does not make a self-report
independent, raise an entry's enforcement ceiling or satisfy an M2 independence
requirement.

#### D4. Scope is an exact typed subject selection

Every permission names a policy domain, one subject kind, and one selector:

1. A finite set of exact subject identities, using that kind's versioned
   canonical reference; or
2. A finite set of exact `(forge identity, immutable repository identity)` pairs,
   for a subject kind whose registered resolver establishes repository membership;
   or
3. Explicit `all_subjects_of_kind` for a kind whose registered contract is
   repository-independent. This deliberately covers existing and future subjects
   of that exact kind in the domain. It is a separately authorized broad scope,
   useful for repeated task or message procedures, never an omitted-field default.

Repository scope and subject kind are conjunctive. Permission on pull requests
in a repository does not permit issues in it. An exact subject permission is
narrower than the applicable broad selector. Repository-bearing kinds cannot use
`all_subjects_of_kind`; an unknown kind contract cannot claim to be
repository-independent. Omitted scope, wildcard, prefix and regular-expression
matching are unsupported. A broad selector requires the grantor to hold and
administer that same breadth; observing one subject cannot authorize its grant.

For an opaque reference without a canonicalizer, exact byte equality is the
only supported subject matching. A display name or `<owner>/<repo>` prefix is
not an immutable repository identity. Existing references are not rewritten.
A resolver may establish an additional immutable identity binding, with its
own version and provenance. Conflicting, absent or ambiguous bindings refuse
repository-scoped admission. Until that resolver exists for a kind, use exact
subject references; do not infer a repository from placeholder columns or
caller-supplied labels.

The Gate matches request-side permission constraints against a canonical
observation descriptor. This is still a claim about the request. At the
transaction seam the shared authority service discharges the Gate's bound
obligation against the actual run, subject, generation, registry entry and role
binding before any write. A request naming an allowed repository but another
repository's subject refuses. A caller cannot substitute `domain`, a namespace
or a source field for this equality check.

External instrument and relay observations may fan out only to explicitly
derived eligible runs of that subject. All selected targets are checked before
any effect. If any selected target is unauthorized, the command refuses whole;
it must not skip that run silently. If no target runs exist, an ordinary
observation returns an explicit not-recorded result, with no run evidence or
progression. A
generation-establishing source event may update the subject independently of
open runs only after its own registration and complete source envelope have
been checked, with its admission receipt and generation-event record committed
atomically. An empty target set is not an authorization bypass. A run selector
can narrow target runs, never the subject-wide meaning of a generation change.

Namespace remains attribution and read filtering. No by-ID namespace equality
check is added. Subject binding verifies the identity of the claimed observation;
it does not isolate arbitrary stored records or create a tenant boundary.

#### D5. Admission and authority freshness share the evidence transaction

The shared authorization component composes with the configured Gate. An outer
allow, including `AllowAllGate`, does not manufacture a producer permission.
Protected evidence-writing paths require an installed component and a sealed
admission context; direct pack invocation without one refuses. Unknown or
unavailable authority never becomes permission.

The initial Gate check decides caller and request permission. The same shared
service then validates its obligation at the evidence transaction, using the
resolved descriptor and current authority. Handlers do not independently
resolve principals, maintain permission lists or implement another policy
engine. This follows ADR-127's Gate/action-seam split without changing
ADR-007's request-side Gate input contract.

Registration state, lineage, scope, source contract, role binding and validity
are checked at the serialization point of the append. Evidence, its admission
receipt, generation change, evaluation, progression and command replay record
commit atomically. The minimum local implementation places the participating
authority records in the same transactional store. An external authority
backend requires an explicit equivalent fencing protocol; a cached check or a
remote query immediately before the write is not equivalent. No network read
occurs inside the charter transaction.

Unregistered, disabled, expired, wrong-kind, wrong-source, wrong-role or
out-of-scope submissions refuse with `ProducerNotAuthorized` and a stable
reason. Missing verification infrastructure is reported as unavailable and
also writes nothing. Malformed envelopes and the removed `producer` field
produce typed input refusals. Refusal appends no charter evidence and changes
no subject, run, phase or command-success state. Ordinary denial telemetry is
not charter evidence.

Each evidence admission retains the producer, identity/source assurance,
registration identity and immutable permission digest, checked authority
revision and lineage, normalized subject and generation, entry-contract digest,
source envelope, relevant role/action binding, and server receipt time. The
receipt embeds the checked values; it does not depend on a best-effort Gate
audit event remaining available. Payload bytes and the full provenance envelope
have separately versioned canonical digests.

Within each target run, the source-event conflict key includes the producer,
source identity, kind, subject and generation. It does not include the
registration identity: renewal cannot make conflicting bytes for the same source
event appear new. Compare a canonical content digest over the fact and its
source envelope, excluding server receipt time and admission metadata. The full
provenance digest separately binds those server-established fields. Identical
resubmission returns the existing receipt without refreshing its age or
registration. Conflicting material bytes refuse as `EvidenceConflict`.

Current submission authorization precedes command replay. In particular, a
disabled caller's `charter.observe` retry refuses, even for a previously
successful request. Its committed evidence remains readable through authorized
history reads. This qualifies ADR-193 D8: an idempotency key does not preserve a
revoked submission right. While authorized, an exact retry returns its original
result and performs no sampling, append, fan-out or progression.

#### D6. Disable changes current eligibility, never recorded history

Disable prevents new submissions under that registration. A fresh evaluation
also checks the continuing liveness, validity and lineage of the registration
named by each external evidence receipt. A new registration cannot validate
an old receipt. A missing or unverifiable receipt is ineligible. Internal
receipts instead verify the pinned internal adapter contract and applicable
current policy. Reading or replaying evidence never changes it.

Selection retains the subject-generation, completeness, source and phase-visit
rules. It examines the latest relevant source state before accepting its current
trust. A disabled or otherwise ineligible later observation cannot be filtered
away to reveal an earlier passing observation. Partial, unavailable or
conflicting newer evidence likewise cannot refresh an older pass. The result
holds with the reason and affected evidence identities until fresh eligible
evidence or a valid explicit policy exception resolves it. Contradictory
required sources remain blocking; no caller selects the green member of an
inventory.

An evaluation is pure over its captured evidence and authority snapshot. Store
the exact definition and registry/implementation digests, candidate and visit,
evidence identities and envelope digests, authority snapshot, evaluation time,
mode and resulting input digest. A diagnostic current evaluation takes a fresh
snapshot; historical replay uses the original snapshot and time. Disable
changes the current authority revision and any resulting evaluation input
digest, not the definition, old evidence, old evaluation or old digest. Missing
historical inputs or evaluator versions yield `ReplayUnsupported` or an explicit
unavailable result, never replay with today's authority silently substituted.

Completed historical progress is not undone. Current views identify affected
historical conclusions, and every future transition checks its required
evidence against current authority. The waiting view must account for authority
revision changes as well as evidence changes, so a stale cached ready state
cannot remain actionable. At M2, current authority checks at claim and start
invalidate unsent admissions dependent on disabled producers under ADR-193 D10.
An already dispatched effect is reconciled; disable does not assert its reversal.

Disable and append/transition/claim/start have a defined serialization order.
If disable commits first, the competing new use refuses. If the use commits
first, its receipt records that earlier authority snapshot; subsequent use is
still refused. No positive cache may extend the registration past this boundary.

#### D7. Bootstrap is explicit; M1 still refuses unregistered evidence

The composition root provisions an explicit authority administrator and finite
authority envelope before external observation admission becomes available.
It uses the shared service's bootstrap lineage and durable root anchor. This
does not depend on any charter observation. Registering the first producer is
therefore possible without already having a producer.

Bootstrap and any optional seed registrations commit with one unique
initialization marker and their attributed history. An explicit empty seed set
is valid. Concurrent initializers must agree on the administrator, authority
envelope and seed digest; divergent initialization refuses, rather than unioning
two configurations. The marker is independent of the number of live
registrations. Restart, an empty registry or disabling the last producer never
reimports seeds or grants authority to the first caller. A post-initialization
configuration edit is not a registration update.

Administration is independent of producer liveness, so disabling all producers
does not disable the administrator. Recovery of lost administrative authority
is an explicit composition-root recovery with a new epoch and retained history.
It does not fabricate an observation or silently reactivate old permissions.
Protected recovery must satisfy the separate custody and epoch requirements of
the protected deployment.

M1 permits an explicitly configured cooperative recording profile. In it, the
producer is the exact caller principal resolved by dispatch and the receipt
states `identity_assurance = configured_local`. This marker is neither
`ActorSignature` nor `DaemonBearer`. It records permission checked against a
configured identity, not verified process identity. A deployment with an armed
authenticator records the actual assurance it established instead. No failed
authentication falls back to a configured label.

This is the bounded M1 qualification to authenticated-producer wording, not a
claim of the complete ADR-127 authentication path. All registration, scope,
class, history and refusal rules still apply. `self_reported` remains a source
assurance available to a registered review reporter, never a permission for an
unregistered caller. M1 runs remain `recording_only`, and `charter.claim`
continues to refuse. M2 requires authenticated provenance and the independent
production paths required for the facts it uses. Neither local enrollment nor
M1 evidence is promoted on activation.

Implementation order: until the minimum standing-right service of D1 exists,
`charter.observe` for external kinds returns unavailable and writes nothing,
while `charter.sample` store readers and internal transitions run. Acceptance
case 5 is gated on that service. Its control is the D5 arm in which missing
verification infrastructure is reported as unavailable and writes nothing; its
positive control is a registered producer admitted once the service exists.

Existing evidence without admission receipts remains unchanged and is labeled
legacy, with registration unverified. Do not infer registrations from distinct
stored producer strings or attach a newly issued registration retroactively.
Historical displays remain available; new evaluations requiring registered
evidence need new observations. Preserve old evaluator semantics only as
explicit historical replay, with their assurance visible.

Where ADR-209 is accepted and implemented, root may accept a specifically
identified missing or rejected policy fact through its exact exception path.
The absence does not become evidence. Root exceptions do not authorize forged
producers, edit a class, enroll a principal or turn rejected evidence into a
pass. Root may separately administer registrations through authorized shared
administration. Bootstrap does not depend on the exception facility, and
accepting this amendment does not activate that proposed facility.

#### D8. Keep the evidence contract in ADR-193

This amendment is the authority for producer admission and history. ADR-208 D3
references its class/channel and eligibility rules; D5 references its subject
selectors; D6 states that M1 does not enforce external effects but does enforce
evidence admission. ADR-143 adds the standing right, constrained administration
and lifecycle rules in D1 without changing other rights. ADR-018 continues to
define the Gate, with the transactional obligation discharge supplied by
ADR-127. ADR-209 references this contract for evidence actually consumed and
retains its separate exception semantics. These companion edits accompany
acceptance, so the records do not describe competing registration mechanisms.

### Acceptance

These are required implementation cases, not claims of executed verification.
Use real dispatch and evidence transactions with paired positive controls.

1. **Registration and administration.** A scoped registered producer records its
   allowed kind. The same caller without registration refuses. An ordinary
   producer cannot register itself, widen its scope, disable another registration
   or delegate administration. A valid constrained administrator can provision
   the producer. Failure to write change history leaves no authority change.
2. **Identity.** Every top-level `producer` value refuses, including the caller's
   own value and reserved internal names. Omitting it records the dispatch
   principal. Principals with equal display strings but different structural
   fields remain distinct. Changing an entry's `args.producer` cannot change
   the authenticated producer and still obeys that argument's permission binding.
3. **Kind, arguments and channel.** A permitted `gate.result` for one exact
   `name` refuses another. A store-reader kind refuses public observe even from
   a registered external instrument; a genuine sample succeeds. An external
   system observation refuses, while the compiled `schedule.fired` sampler
   succeeds on actual fired state and refuses otherwise. Unknown classes and
   data-defined internal adapters refuse, as do attempts to enroll subject
   declarations or internal lifecycle names for external submission. Changing
   class/channel/schema changes the entry digest and requires corresponding
   new permission.
4. **Actor binding.** The registered reporter bound to the designated role
   succeeds for its named run. An unbound caller, another bound role, another
   run and another action visit each refuse. A self-written role binding alone
   grants nothing. Missing and ambiguous reporting mappings refuse admission of
   a run that requires them. A phrase recorded before its first consuming phase
   remains eligible under that run's unchanged binding.
5. **Platform relay.** A valid registered relay with the correct response
   envelope succeeds. Missing source identity, wrong source, wrong candidate
   and wrong claim-time executor refuse. A syntactically plausible response id
   without verified collection provenance never receives independent-platform
   assurance. A store sample retains store provenance even if its source row
   claims an external origin.
6. **Scope.** An exact subject grant permits that subject and refuses a similar
   prefix, another kind and another domain. A repository grant permits a subject
   resolved to its immutable repository and refuses another repository, an
   unbound display reference and a request that lies about the repository.
   An explicit repository-independent kind grant covers a newly created subject
   of that kind but no other kind or domain; the same selector for a
   repository-bearing kind refuses. No combination of partially matching
   permissions admits a request. Changing
   namespace alone neither grants permission nor changes the subject identity.
7. **Pre-effect refusal.** Failed producer admission changes no evidence,
   generation, phase, child run or success receipt, including on trigger paths,
   empty target sets and multi-run fan-out with one unauthorized target. Generic
   note and stream writes cannot construct evidence or producer authority.
   Direct pack calls with no sealed admission context refuse. An outer
   `AllowAllGate` does not bypass the producer component; missing component
   prevents activation of evidence-writing paths. A permitted generation event
   can update its subject with no open run only with its admission and
   generation-event records; the unregistered equivalent refuses without change.
8. **Disable and races.** Record evidence, disable its registration, and refuse
   new observations without restart, including through a second serving
   process. Force both serialization orders of disable versus append and
   transition. Only an operation ordered before disable may commit its use.
   Repeat with expiry, revoked lineage and unavailable authority. Where M2 is
   active, repeat at claim and start; dispatched attempts retain reconciliation.
9. **History and no resurrection.** A fresh evaluation rejects use of disabled
   evidence while replay of the original evaluation returns its original result
   and digest. Disable the producer of a later failure: an older green result
   must not reappear as a pass. A new registration does not revive old receipts.
   Fresh valid evidence restores eligibility. Evidence bytes, completed history
   and pinned definition digests never change during these operations.
10. **Replay and conflict.** An authorized exact retry returns the original
    receipt and age without progression or fan-out to a newly opened run.
    Conflicting bytes under the same event identity refuse even after renewing
    registration. Disable causes subsequent observe retries to refuse; an
    authorized history read still returns the original record. Failed append or
    receipt persistence rolls back the complete command.
11. **Bootstrap.** A fresh store can provision an administrator and then its
    first producer. Empty seeds remain empty after restart. Identical concurrent
    initialization is idempotent; divergent initialization refuses. Disabling
    the last producer does not reopen bootstrap or remove administration.
    Unprovisioned administration never authorizes the first incoming caller.
12. **Milestones and legacy evidence.** M1 refuses unregistered submissions
    rather than lowering assurance to accept them. A registered self-report is
    labeled and cannot satisfy an M2 independent-source requirement. Legacy
    records remain unchanged, unverified and ineligible for new registered-evidence
    decisions. Same-user configuration impersonation may pass in cooperative
    mode and must never be reported as detected or contained by registration.
13. **Exception separation.** Once the ADR-209 exception facility is available,
    an exact root exception can release a policy hold with no eligible producer
    for that fact. It records the absence and exceptional disposition. It cannot
    make a forged observation admissible, enroll its submitter or waive an
    unavailable authority service. Other unwaived facts still need eligible
    evidence; M1 release still creates no action admission.

Mutation controls must make the corresponding cases fail when the
registration check, transactional authority recheck, reporting-role comparison,
resolved-subject comparison, or no-resurrection rule is removed. A suite that
refuses every observation does not satisfy the positive controls.

### Alternatives considered

| Choice                                       | Rejected alternatives and reasons                                                                                                                                                                                                      |
| -------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Shared standing producer right               | A charter-owned issuer/table duplicates caller authority, administration and revocation. Reusing ADR-180 grants supplies neither bound scope nor the required precedence.                                                              |
| Separate registry and registration           | Embedding principal lists in vocabularies couples live revocation to immutable definitions; inferring authority from a class lets vocabulary publication enroll callers.                                                               |
| Dispatch-derived producer                    | A caller-selected producer permits impersonation; a second relay-principal field creates another unverified identity when the caller already is the relay.                                                                             |
| Explicit class and channel contracts         | Banning all instrument observations breaks external instruments; allowing every kind to every registered caller defeats the class; engine-only system production excludes the existing scheduler sampler.                              |
| Role- and instance-bound reports             | Any bound role may falsely report another role's work; making every actor report subject-wide allows it to complete unrelated runs.                                                                                                    |
| Exact typed scope with explicit breadth      | String patterns conflate display names and stable identity; implicit kind-wide permission includes unrelated subjects without an administrative decision; exact-subject-only enrollment imposes a new grant for every task or message. |
| Current eligibility with historical replay   | Ingest-only checks let disabled evidence authorize future decisions; deleting or rewriting evidence destroys the earlier decision's reproducibility.                                                                                   |
| Explicit shared bootstrap                    | First-caller enrollment is self-authorization; reseeding an empty registry resurrects disabled authority; vocabulary publication is not enrollment.                                                                                    |
| Evidence amendment with companion references | Putting registration only in ADR-208 leaves the accepted evidence contract incomplete; putting it only in ADR-209 makes ordinary evidence admission depend on optional exception and history capabilities.                             |

### Known limits

- Registration constrains which reports an admitted principal may submit. It
  does not prove that an authorized instrument, reviewer or relay tells the
  truth. Store sampling proves stored state, not the legitimacy of every
  upstream mutation. Independent verification is a source-contract property.
- A same-user process that can replace caller configuration, credentials,
  authority state or the verifier can impersonate an enrolled principal or
  bypass the service. Cooperative recording does not detect or prevent that.
  Protected operation requires independently established custody and ingress
  controls; a future hosted deployment also establishes user scope at trusted
  ingress and an appropriate store boundary, not through namespace equality.
- Response identifiers and payload hashes are useful bindings, not signatures
  of a remote service. Source authentication remains only as strong as the
  registered collection path and its custody.
- Current disable can make an earlier result unusable for future decisions
  without declaring the old observation false. Completed history remains
  history. Restore of an unprotected authority store can resurrect local
  permission; protected epoch and recovery checks are separate prerequisites.
- Exact enrollment and digest pinning require administrative updates when a
  source contract, subject set or reporting assignment changes. The shared
  service and the per-kind repository resolvers must be implemented and
  verified; existing schemas and accepted decisions alone provide neither.
- No new charter expression language, tenant isolation mechanism, effect
  credential custody or root signing facility is introduced by this amendment.

## Amendment 6 (2026-10-23): durable encoding of producer registrations

**Status**: Accepted 2026-10-23. It does not change the status of the base ADR or of earlier amendments.

This amendment serves the project owner's instruction of 2026-10-22, rendered: "So they connect:
charter + gate + privileges, the whole automated permission system has nearly taken shape in khive".
Amendment 5 and ADR-143 Amendment 3 serve the same instruction; this amendment fixes representation
choices that their durable registration leaves open.

It amends Amendment 5 D1 and D3 and ADR-143 Amendment 3 M3.2, M3.3 and M3.8, as specified below. It
adds no right, verb, table or migration.

### A6.1. Terminal state of a record holding both rights

ADR-143 Amendment 3 M3.2 lets one grant record carry `RecordObservation` and `GrantAdmin` together.
M3.3 names `disabled` as the terminal state for `RecordObservation` and `revoked` for other supported
grants, and names no state for a record holding both. Such a record has one whole-record terminal
state: `disabled` if any `RecordObservation` right is present, otherwise `revoked`. Either name ends
every right on the record. The typed history read names every right the record carried (A6.6), and
`authority.list` shows the recorded terminal state for the whole record, so a `GrantAdmin` right
that ends under the name `disabled` is read from history, not inferred. A mixed
grant is not refused, split into several identities, or terminated in part.

### A6.2. Keys of the actor reporting binding

D3 binds an action result to its run, phase visit, action occurrence and exact arguments. The keys
are the ones the run engine already uses:

- Run: the run's UUID, which the engine mints as a version-4 UUID. A registration stores its 16
  bytes and accepts only the canonical lowercase hyphenated text at its boundary.
- Phase visit: the phase identifier and the round, an integer of at least 1. A phase that has not
  been entered holds round 0 and each entry adds one, so round 0 names no visit and refuses.
- Action occurrence: the visit and the action name. The definition checker refuses a phase that
  performs one child twice, so a published definition cannot make an occurrence ambiguous. A
  registration naming such a definition also refuses; that check is defense in depth over a
  definition that cannot be published, not the mechanism.

### A6.3. Claim-time platform bundles in the M1 profile

D1 requires a claim-time platform bundle to pin its designated executor and bundle purpose, and no
decision enumerates the purposes. ADR-143 Amendment 3 M3.1 leaves `charter.claim` unavailable in
the M1 profile. In that profile a registration for a claim-time bundle refuses with a named outcome
of the `RightNotSupported` family; it is never dropped silently. The ordinary platform relay is
supported. This is a limit of the M1 profile, not a permanent exclusion: the profile that enables
`charter.claim` enumerates the purposes.

### A6.4. Validity interval

Instants are `khive_types::Timestamp` values, microseconds since the Unix epoch. The interval is
half-open: a registration is valid from `not_before` inclusive until `expires_at` exclusive,
`expires_at` must be later than `not_before`, and at `t == expires_at` the registration has expired.
An explicitly absent `expires_at` is a standing permission. ADR-143 Amendment 3 M3.8's
expiry-at-the-seam control includes the equality instant.

A grant request names no interval: M3.2's grant item carries the grantee, namespace, parent and
rights, and this amendment adds no request field. The server derives the interval inside the writer
transaction from one clock read, the same instant at which the chosen ordinary parent (or the
bootstrap source) is checked: `not_before` is that verification instant and `expires_at` is the
source's `expires_at`, absent when the source is standing. Because the source is valid at that
instant, the derived interval is never empty. A registration is therefore never valid for an instant
before it was issued, so evidence recorded earlier cannot become eligible under it. The
administrative proof is checked at the same instant and does not shorten the interval. The derived
interval is returned with the record.

### A6.5. Digests, durations and the record bound

Contract digests (entry contracts, source contracts and canonical references) are BLAKE3-256 under
one algorithm tag in this version; an unknown tag refuses. Duration-typed argument values are whole
seconds, the unit of the charter language's duration literal. The canonical durable record of one
grant is bounded at 2 MiB, separately from M3.2's 1 MiB canonical request bound, because the record
adds server-derived fields and their authentication to the request's content. Every collection in
it holds at most 256 entries. An over-limit record refuses with the applicable bound and the
measured size, before issue and before insert; it is never truncated.

### A6.6. Revocation is never refused for size

An issued grant stays revocable. The terminal transition, the revoke or retire event and the revoke
authorization snapshot name each target, and each administering grant of the caller, by its UUID and
the BLAKE3-256 digest of its preserved canonical body, together with the terminal state and the
action and event identifiers. None of them copies a right set, so their size does not grow with the
record they end, and a record at the 2 MiB bound revokes with a result inside M3.2's 1 MiB replay
bound. The history read and `authority.list` decode the preserved grant bytes, check the digest and
return every former right beside the terminal state. The preserved canonical grant body is retained
after the terminal transition and is never purged in this version, because the history read names
every former right only while those bytes exist.

### Known limits

- A claim-time platform bundle cannot be registered in the M1 profile (A6.3).
- The binding keys fix how a report names its run, visit and occurrence. Whether the reporter is
  the bound actor remains D3's check.

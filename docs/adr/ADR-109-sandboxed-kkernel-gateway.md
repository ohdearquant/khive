# ADR-109: Sandboxed kkernel Gateway for Untrusted Execution (Phase C)

**Status**: Proposed\
**Date**: 2026-07-11\
**Authors**: khive maintainers\
**Depends on**: ADR-018 (Authorization Gate, as amended by ADR-129), ADR-016 (Request DSL),
ADR-017 (Pack Standard), ADR-007 Rev 7 (Namespace as Attribution-Only)\
**Related**: ADR-108 (Git Write Surface - composition point, Fork (d) below), ADR-085 (Code
Pack - precedent for an admin-CLI-only surface distinct from the agent-facing MCP surface),
khive-cloud API-key scope model (design input for Fork (b))

This ADR enumerates forks with trade-offs rather than picking silently. The forks below were
resolved through design review; each fork's resolution is recorded in place, and the
Resolutions section summarizes all four rulings.

## Context

khive's existing trust model (ADR-003, reaffirmed in ADR-018 "Trust boundary alignment with
ADR-003") has exactly two tiers today:

1. **Operator binary** (`kkernel sync`, `kkernel db migrate`, `kkernel pack list`, and every
   other CLI subcommand except `kkernel mcp`) - runs with `AllowAllGate` unconditionally.
   Operators are trusted by definition: they have local shell access to the machine running
   khive.
2. **Agent binary** (`khive-mcp` / `kkernel mcp`) - the MCP `request` surface, gated per
   ADR-018. The gate defaults to `AllowAllGate` too (personal-local deployments), but a
   deployment can install a `RegoGate` or custom `Gate` to enforce per-actor policy.

Both tiers assume the caller - operator or agent - is a principal khive already trusts to
reach the full verb catalog (subject to whatever Gate is installed). Neither tier has a
notion of a caller that is _not_ trusted to see the full catalog at all: a sandboxed agent
running someone else's prompt, an external tool integration, or a process khive's operator
does not fully control. Today, such a caller either gets the full MCP surface (if it can
reach `kkernel mcp` at all) or nothing (if it cannot reach khive at all) - there is no
constrained middle tier.

This gap matters for two concrete scenarios: an agent running under prompt-injection risk
(it read attacker-controlled content and might be steered into calling verbs it should not),
and a genuinely external, semi-trusted tool integration that should be able to do a narrow,
declared set of things and nothing else. Both need a surface with:

- A fixed verb allowlist smaller than the full catalog.
- Guaranteed namespace pinning (the caller cannot escape into another namespace's data).
- Rate/budget caps (a runaway or malicious caller cannot exhaust resources).
- No admin CLI verbs reachable at all (the operator-tier commands above are categorically
  off-limits, not merely gated).
- No filesystem-path-bearing arguments accepted (a sandboxed caller must not be able to
  direct khive to read or write an arbitrary host path).
- Fail-closed behavior on anything outside the declared contract - an unrecognized verb, an
  out-of-allowlist argument shape, or a Gate infrastructure error must refuse. ADR-129 now
  requires the base Gate to refuse infrastructure errors too; the gateway rule remains necessary
  for its additional allowlist, argument-shape, namespace, and budget boundaries.

This ADR specs a gateway **mode** for this third trust tier. How that mode is packaged
(Fork (a)) and how the sandboxed caller authenticates (Fork (b)) were presented to design
review as the forks below; each is resolved in place.

## Decision

A **gateway contract** is introduced: a declared, closed set of `(verb, arg-shape)` pairs a
sandboxed caller may invoke, plus the namespace it is pinned to and the rate/budget caps
that apply. The contract is enforced at (or before) the same `VerbRegistry::dispatch` seam
ADR-018 already uses for gate consultation - this ADR does not introduce a second dispatch
path; it introduces a stricter policy input and a pre-dispatch allowlist check ahead of the
existing `Gate::check` call. The four forks below were presented to design review; each is
resolved in place, with the full set of rulings summarized in the Resolutions section.

### Hard rules (not forked)

1. **Verb allowlist is closed and explicit.** A sandboxed caller's request is checked
   against the canonical `pack.verb` id (per ADR-018 Amendment 1's canonicalization step -
   this rules out an alias-based bypass of the allowlist the same way it rules out an
   alias-based bypass of Gate policy). A verb not on the declared list is denied before pack
   dispatch, not passed through to the pack handler for it to reject.
2. **Namespace is pinned, not caller-suppliable.** ADR-007 Rev 7 Rule 3 already
   establishes that an explicit `namespace=` request parameter is the only escape from the
   default `'local'` write/read scope. For a sandboxed caller, that escape is itself closed:
   the gateway ignores or rejects any caller-supplied `namespace` argument and always
   substitutes the contract-declared namespace. This does not add a new namespace mechanism

- it constrains which value the existing parameter is allowed to carry for this caller
  class.

3. **No admin CLI verbs.** The gateway mode never exposes the `kkernel` subcommands listed
   in Context (`sync`, `db migrate`, `pack list`, `git-ingest`, `code-ingest`, `reindex`,
   etc.). These are not verbs on the `VerbRegistry` surface at all today (they are `clap`
   subcommands on the `kkernel` binary, a structurally separate entry point from `kkernel
 mcp`), so "no admin CLI verbs" is largely already true by construction for any caller
   reaching khive via MCP. This rule exists to make that structural fact an explicit,
   checked contract property rather than an incidental one, and to ensure no future verb
   that wraps admin functionality (a hypothetical `kg.migrate` MCP verb, for example) is
   ever added to a sandboxed allowlist without deliberate review.
4. **No filesystem-path-bearing arguments.** Verbs whose arguments accept a filesystem path
   (for example, a hypothetical local-source `git.digest(source=<local path>)`, per ADR-088
   Amendment 1's `DigestSource::Local`) are either excluded from the sandboxed allowlist
   entirely, or the contract for that verb declares path-shaped arguments as forbidden and
   the gateway validates the argument shape before dispatch (rejecting an absolute path, a
   `file://` URL, or a value containing path separators, depending on the verb). Which of
   "exclude the verb" vs. "constrain its arguments" applies per verb is part of the
   capability declaration (Fork (c)), not fixed globally here.
5. **Rate and budget caps are enforced, not merely declared.** ADR-018 §"Why no obligation
   enforcement in v1?" states `Obligation::RateLimit` is declared by policy but not enforced
   by the runtime. This ADR requires that gap to close for the gateway mode specifically:
   a sandboxed caller's dispatch path must consult and enforce a rate/budget counter (calls
   per window, and optionally a cost-unit budget per ADR-103's resource-attribution model)
   before dispatch proceeds. This is new runtime behavior beyond what ADR-018 ships today,
   scoped to the gateway path only - it does not retroactively require rate-limit
   enforcement for the operator or ungated-agent tiers.
6. **Fail-closed on anything outside the contract.** Any of: an unrecognized verb, an
   argument shape that does not match the declared contract, a caller-supplied namespace
   override attempt, a Gate infrastructure error (`Err(GateError)`), or a rate/budget cap
   exceeded - all result in denial or typed refusal. For Gate infrastructure errors this now
   matches ADR-129's base fail-closed posture; the gateway-specific validation failures remain
   additional refusals rather than a gateway-only reversal of the base Gate.

### Fork (a): Process boundary

**A1 - Separate gateway binary.** A new binary (e.g. `khive-gateway`) links the same
`VerbRegistry`/pack machinery as `khive-mcp` but is compiled with the allowlist/pinning/cap
logic built into its own dispatch wrapper, never exposing the unconstrained
`VerbRegistry::dispatch` entry point at all.

- Pro: the constrained surface is enforced by the binary's own structure - a sandboxed
  process that can reach only this binary cannot reach the unconstrained path even if the
  gateway contract has a bug, because the unconstrained dispatch function is simply not
  linked into anything the sandboxed process can invoke.
- Con: a new binary means a new build/release/distribution artifact, new integration-test
  surface, and a second place pack registration (`inventory::submit!`, per `kkernel`'s
  `_pack_links` force-link pattern) must be kept correct as packs are added or removed.

**A2 - Mode flag on kkernel/khive-mcp.** `kkernel mcp --gateway <contract-file>` (or an
equivalent flag) runs the existing `khive-mcp` binary but constructs the `VerbRegistry`
with the allowlist/pinning/cap wrapper active for the whole process lifetime, instead of the
normal unconstrained dispatch.

- Pro: no new binary; reuses the existing MCP transport, daemon-spawn (ADR-049), and pack
  registration exactly as-is; a deployment simply launches `kkernel mcp` differently for a
  sandboxed caller than for a trusted one.
- Con: the safety property becomes "the flag was set correctly at launch," a configuration
  fact rather than a structural one - a misconfigured launch (flag omitted, or a bug in flag
  handling) silently reverts to the full unconstrained surface, which is a materially worse
  failure mode than A1's "the binary the process can reach never had the unconstrained path
  at all."

**A3 - Daemon-side policy profile.** The warm daemon (`kkernel mcp --daemon`, ADR-049)
already serves multiple client connections; A3 extends it to recognize a per-connection (or
per-socket) policy profile, so the same daemon process serves both a trusted agent
connection at full capability and a sandboxed connection at constrained capability
simultaneously, distinguished at the transport/connection layer.

- Pro: one warm daemon serves every caller class, avoiding a second cold-start process for
  the sandboxed path (relevant since ADR-049's whole premise is that daemon warm-up cost -
  ANN/embedder state - is expensive to pay per-process); matches the existing multi-client
  serving model (ADR-096, "Warm Daemon Per-Request Identity") which already threads distinct
  attribution identities through one shared backend.
- Con: highest implementation complexity of the three - the daemon must correctly
  demultiplex connections to policy profiles and there is exactly one shared process whose
  compromise (a bug in the demultiplexing logic) affects every caller class at once, unlike
  A1 where the sandboxed and trusted paths are different binaries entirely.

**Resolution (Open Question 1 - process boundary)**: the configuration-profile
option (A2) is rejected outright: a silent misconfiguration reverting to the full verb
surface defeats the purpose of this ADR. This is resolved as a structural boundary. The
recommended implementation shape is a thin gateway binary (A1) that connects to the warm
daemon as a client, a proxy, so the sandboxed process can only ever reach the constrained
binary while warm ANN and embedder state is still reused from the daemon. A standalone
constrained binary is the fallback if the proxy hop proves infeasible. The in-daemon
demultiplexer option (A3) is rejected for v1: a demux bug would have whole-surface blast
radius. A3 is revisited only after the contract mechanism is proven in production.

### Fork (b): Authentication of the sandboxed caller

**B1 - Gate-mediated identity, per ADR-018.** The sandboxed caller authenticates however the
transport already supports (an MCP client identity, a socket-level credential), and the
resulting `ActorRef` (`kind`, `id`) is what the Gate's `GateRequest.actor` field carries -
no new authentication mechanism, only a new `Gate` implementation (or `PackGatePolicy`, per
ADR-018's pack policy extension point) that recognizes a "sandboxed" actor kind and applies
the allowlist/pinning/cap contract as policy.

- Pro: zero new authentication machinery; consistent with ADR-018's existing model where
  "how an operator's gate maps authenticated identities to allow/deny is operator policy,
  implemented behind the trait" - the gateway contract is exactly such a policy.
- Con: the existing `ActorRef.kind` is a free-form string (`"user" | "agent" | "lambda" |
 "anonymous" | custom`) with no notion of a verified credential - nothing in ADR-018
  today cryptographically authenticates that a caller claiming `kind = "agent", id =
 "sandboxed-x"` actually is that principal. B1 alone does not add that; it only routes an
  already-established identity through policy.

**B2 - API-key scope model, relating to khive-cloud.** A sandboxed caller presents an API
key (structurally similar to what khive-cloud's tenant model uses - capacity/API-key based
per the pricing/access model already in design for the cloud tier) whose scope _is_ the
gateway contract: the key itself encodes (or is looked up to yield) the verb allowlist,
namespace pin, and rate cap, rather than those being a separately configured policy the Gate
consults.

- Pro: a single artifact (the key) is the capability - easy to issue, revoke, and audit per
  key; matches a pattern khive-cloud already needs for tenant API keys, so the mechanism is
  reusable rather than gateway-specific; keys can be scoped per integration without touching
  Gate policy configuration for each one.
- Con: introduces a new credential type and its storage/validation/revocation lifecycle
  (key hashing, expiry, rotation) that does not exist in khive today outside the cloud-tier
  design; for the OSS/self-hosted deployment this ADR is scoped to, standing up key issuance
  infrastructure is a heavier lift than B1's "just configure a Gate policy."

**Resolution (Open Question 2 - authentication of the sandboxed caller)**:
transport-level identity now (B1), with a documented migration path to key-based
authentication (B2) once the cloud key infrastructure exists. Contract documentation must
state plainly that `ActorRef` identity is transport-level, not cryptographic, in
open-source deployments.

### Fork (c): Capability declaration format

**C1 - Static allowlist file/config.** A TOML or JSON file lists permitted `(pack.verb,
arg-constraints)` tuples, read at gateway startup (whichever Fork (a) shape hosts it),
analogous to how `RegoGate` policies live in files an operator configures (ADR-018).

- Pro: simplest to author, review, and diff in a PR - a capability grant is a visible,
  version-controllable artifact; no new policy-language dependency.
- Con: argument-shape constraints (rule 4's path-argument exclusion, for example) are harder
  to express richly in a flat config format than in a language with actual predicates; a
  static file also means capability changes require a restart/reload, not a live grant.

**C2 - Gate policy objects (Rego), extending ADR-018's existing mechanism.** The gateway
contract is expressed as Rego rules evaluated by the same `RegoGate`/`regorus` engine
ADR-018 already ships, with the allowlist, namespace pin, and rate cap as `decision`/
`obligations` fields the gateway's pre-dispatch check consults - no new policy engine, the
gateway is "just" a stricter default-deny Rego policy plus the enforcement additions in
rules 2, 4, 5, and 6 above that go beyond what `Obligation` enforcement does today.

- Pro: reuses ADR-018's policy language and engine entirely; a capability grant and a
  general Gate policy are authored in the same language, reducing the number of
  configuration surfaces an operator must learn; Rego's `default decision := {"decision":
 "deny", ...}` pattern (the documented explicit-deny idiom in ADR-018's own example
  policy) is a natural fit for rule 6's fail-closed requirement.
- Con: couples the gateway's capability model to Rego/regorus even for the simplest
  allowlist cases, where C1's flat format would suffice and be easier to audit at a glance;
  Rego expressiveness is double-edged - a capability contract that can express arbitrary
  predicates is also harder to statically review for "does this actually enforce a closed
  allowlist."

**Resolution (Open Question 3 - capability declaration format)**: a restricted subset
of C2, constrained Rego on the existing policy engine, with a required default-deny template
and a validation lint that rejects any contract failing to declare a closed verb allowlist.

### Fork (d): Relationship to Phase B (git writes from sandboxed callers)

ADR-108 (Phase B, this pair's companion ADR) specs write verbs (`git.commit`, `git.branch`,
`git.push` at minimum) reachable by a trusted/semi-trusted caller through the normal gate.
A sandboxed caller under this ADR's gateway mode invoking a Phase-B git-write verb is the
literal composition of both specs, and needs explicit treatment rather than an implicit
"the gateway contract will sort it out":

- If Phase B's write-verb allowlist entry is present in a sandboxed caller's contract at
  all, every hard rule from _both_ ADRs applies simultaneously: force-push denial
  (ADR-108 rule 1) and gateway fail-closed-on-anything-outside-contract (this ADR's rule 6)
  compose without conflict, since both are deny-biased. But ADR-108's Fork (d) explicitly
  scoped its write surface to "content the calling agent itself produced" and marked
  fork-PR/external-content writes as categorically out of scope, not merely unpolicied. A
  sandboxed caller is, by definition, the caller class most likely to be executing
  prompt-injected or externally-influenced instructions - which makes ADR-108's Fork (d)
  boundary (2) (no fork-diff-write capability at all, rather than trusting policy to gate
  it) the load-bearing protection here, not this ADR's allowlist alone. ADR-108's Fork (d)
  resolved toward keeping fork-content write capability unbuilt rather than gating it with a
  `source_trust` field, which keeps this ADR's threat model (Prompt-injected agent, below) at
  its current severity; a future ADR that builds fork-content write capability would need to
  re-review this composition before a sandboxed contract could include any git-write verb.
- Standing policy, per the resolution of Open Question 4 below: a sandboxed gateway contract
  does not include any ADR-108 write verb. This composition is revisited only via a new ADR,
  once a specific, reviewed contract is drafted and demonstrated need is shown.

**Resolution (Open Question 4 - composition with the git write surface)**:
standing policy, no git write verb from the ADR-108 surface may appear in any sandboxed
contract. This is revisited only via a new ADR after the write surface ships with
demonstrated need for a narrower, sandboxed-safe composition.

## Threat Model

**Prompt-injected agent.** A sandboxed agent processes attacker-controlled content (a
fetched web page, a file, a tool result) that contains instructions steering it toward
calling khive verbs the operator did not intend. The gateway contract is the primary
defense: even a fully successful injection can only reach the declared allowlist, in the
pinned namespace, under the rate cap. This is why rule 6 (fail-closed on anything outside
contract) is load-bearing rather than advisory - an injected agent will, by construction,
attempt to probe or exceed the contract, and the failure mode for any probe must be deny,
never a permissive fallback. The composition risk with ADR-108 (Fork (d) above) is the
sharpest instance of this: a prompt-injected agent with git-write capability could be
steered into committing or pushing attacker-chosen content, which is exactly why this ADR's
standing policy excludes write verbs from sandboxed contracts.

**Exfiltration via verbs.** A sandboxed caller with legitimate read-verb access (`search`,
`get`, `neighbors`, `context`) could be used to exfiltrate data outside its intended
namespace scope, either by requesting a broader read than intended or by chaining reads
across records the operator did not mean to expose. Mitigations: rule 2 (namespace pinning,
not caller-escapable) bounds the data surface to the contract-declared namespace regardless
of what the caller requests; the capability declaration (Fork (c)) should, whichever format
is chosen, allow per-verb argument constraints (e.g., a contract that permits `search` but
caps `limit`, or excludes `context`'s `hops`/`fanout` expansion parameters) so a narrow verb
grant cannot be used to walk far beyond the intended scope in one call. This ADR does not
fully specify per-verb argument constraint shapes - that is part of Fork (c)'s resolution.

**Resource exhaustion.** A sandboxed caller, malicious or merely buggy, issuing verbs in a
tight loop (a runaway ANN search, repeated large `context` traversals) could degrade shared
daemon resources (ADR-049's warm ANN/embedder state is a shared, contended resource across
all callers per ADR-096's per-request-identity model). Rule 5 (enforced rate/budget caps)
is the direct mitigation; this ADR requires it be actual enforcement, not the
declared-but-unenforced `Obligation::RateLimit` ADR-018 ships today, specifically because a
sandboxed caller is exactly the class of caller a resource-exhaustion threat model assumes
is present. Budget accounting may reuse ADR-103's cost-unit resource-attribution model if
that lands first; this ADR does not require it, only that some enforced counter exists for
the gateway path.

## Alternatives Considered

| Alternative                                                                                                       | Why not adopted                                                                                                                                                                                                                                                |
| ----------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Rely on `AllowAllGate` plus operator discipline (document "don't expose this to untrusted callers")               | This is the status quo and is exactly the gap this ADR exists to close; documentation is not enforcement, and ADR-018 itself calls `AllowAllGate` "a footgun in multi-user environments."                                                                      |
| Extend the existing Gate to a "strict mode" that fail-closes on `Err` globally, without a separate allowlist tier | Addresses only rule 6's fail-closed requirement, not the allowlist/pinning/cap/no-admin/no-path requirements this ADR's caller class needs; conflates "stricter infra-failure handling for everyone" with "a genuinely narrower surface for one caller class." |
| Sandbox at the OS/container level only (seccomp, container isolation), no khive-level gateway                     | Orthogonal, not a substitute - OS-level sandboxing constrains what the process can do to the _host_, not what verbs it can invoke against khive's own data once it can reach the MCP transport at all. Complements this ADR rather than replacing it.          |

## Implementation Plan

- Enforced rate and budget caps (rule 5) are new runtime work, not an assumed capability of
  the existing dispatch path: `Obligation::RateLimit` is declared-but-unenforced today
  (ADR-018), and this ADR requires actual enforcement for the gateway path. This is a named
  implementation-phase item: the gateway dispatch path must consult and enforce a
  rate/budget counter before dispatch proceeds, and that counter is built as part of this
  ADR's implementation, not inherited from ADR-018.
- The gateway process boundary (resolution of Open Question 1: a thin proxy binary in front
  of the warm daemon) is new build/release surface and is scoped as its own implementation
  item, separate from the pre-dispatch allowlist/pinning check.
- The capability declaration format (resolution of Open Question 3: constrained Rego with a
  default-deny template and a validation lint) requires the lint itself to be built; it is
  not a byproduct of writing the Rego policy.

## Consequences

### Positive

- Gives khive a genuine third trust tier, closing the "full surface or nothing" gap between
  the operator and trusted-agent tiers documented in Context.
- Enforced rate/budget caps for the gateway path close a real gap ADR-018 left open
  (`Obligation::RateLimit` declared-not-enforced) - for this caller class specifically.
- A documented, structural place to compose with Phase B (ADR-108) rather than an implicit
  or accidental interaction, per Fork (d).

### Negative

- New enforcement code path (pre-dispatch allowlist/pinning/cap check) is new surface to get
  wrong; a bug here that fails open, rather than closed, defeats the entire ADR - this is
  exactly why rule 6 is stated as a hard rule rather than left to per-deployment policy.
- The chosen Fork (a) shape (a thin proxy binary, with a standalone constrained binary as
  fallback) is nontrivial new engineering, per the Implementation Plan above.
- Fork (d) ties this ADR's write-verb boundary to ADR-108: the standing policy of excluding
  every ADR-108 write verb from sandboxed contracts holds regardless of how ADR-108's own
  forks resolve, but any future narrower composition needs its own ADR and re-review of the
  threat model above.

## Resolutions

1. **Process boundary (Fork (a))**: A1 separate binary, A2 mode flag, or A3 daemon-side
   policy profile. **Resolved**: the configuration-profile option (A2) is rejected outright.
   A thin gateway binary that proxies to the warm daemon (A1) is the recommended shape; a
   standalone constrained binary is the fallback if the proxy hop proves infeasible. The
   in-daemon demultiplexer (A3) is rejected for v1 and revisited only after the contract
   mechanism is proven in production. See the resolution under Fork (a) above.
2. **Authentication of the sandboxed caller (Fork (b))**: B1 Gate-mediated `ActorRef`, B2
   API-key scope model, or B1-now-B2-later. **Resolved**: B1-now-B2-later - transport-level
   identity now, with a documented migration path to key-based authentication once cloud key
   infrastructure exists. See the resolution under Fork (b) above.
3. **Capability declaration format (Fork (c))**: C1 static allowlist file, C2 Rego policy
   objects, or a constrained C2 variant with a required default-deny lint. **Resolved**: the
   constrained C2 variant - Rego on the existing policy engine, with a required default-deny
   template and a validation lint that rejects any contract lacking a closed verb allowlist.
   See the resolution under Fork (c) above.
4. **Relationship to Phase B (Fork (d))**: whether any sandboxed contract may ever include
   an ADR-108 git-write verb. **Resolved**: standing policy, no ADR-108 write verb may
   appear in any sandboxed contract. Revisited only via a new ADR once the write surface
   ships with demonstrated need. See the resolution under Fork (d) above.

## Amendment 1 (2026-08-24): Phased implementation — the read-only connection tier

**Status of amendment**: Proposed (implements a subset of this ADR; the full sandboxed
gateway of the base decision remains Proposed and deferred as stated below).

The base ADR specs one caller class — a fully sandboxed, untrusted caller — and resolves its
process boundary to a structural one (Fork (a), A1: a thin gateway binary, never the
unconstrained dispatch entry point). That is the right shape for an untrusted caller and it is
a nontrivial new build. This amendment separates the caller classes by **input provenance** and
commits to building the smaller, lower-risk one first, on machinery that already ships, while
keeping the base ADR's structural boundary as the untrusted-tier answer.

### Caller classes by input provenance

The trust question is not _which_ process connects, it is _what input that process has already
consumed_ by the time it calls a verb:

- **Tier A — trusted.** A caller operating only on operator-directed or first-party
  instructions. Full verb catalog, subject to whatever `Gate` the deployment installs (status
  quo; `AllowAllGate` by default).
- **Tier B — semi-trusted, read-scoped.** A caller trusted to _read_ the graph but which should
  not _mutate_ it, and whose inputs are first-party (it has not consumed attacker-influenced
  content). The motivating case is an observer process that reads first-party state to make a
  decision and must not write back. This amendment builds the control for this tier.
- **Tier C — untrusted.** A caller that has consumed content it does not control (a fetched
  page, a third-party document, an external tool result) and could therefore be steered into
  calling verbs the operator did not intend. This is the base ADR's caller class.

### What this amendment builds: the Tier-B read-only connection

A **read-only connection mode**: a per-process khive connection whose `Gate` denies every
mutating verb and permits only the read-verb set, enforced at the dispatch seam the runtime
already consults on every verb. All of it reuses shipped machinery:

1. **Enforcement seam already exists.** `VerbRegistry` dispatch already builds a `GateRequest`
   carrying the _actual_ verb and calls `Gate::check` before the pack handler runs; a `Deny`
   returns `PermissionDenied` with a reason (ADR-018, as amended fail-closed by ADR-129). No new
   dispatch path is introduced — this is the same seam the base ADR's rule names.
2. **The enforcement invariant is a closed read-verb set held in code, intersected with
   policy.** Read-only mode installs a composite gate: a verb dispatches only if it is a member
   of the canonical read-verb set — a closed, machine-checkable list shipped in code — **and**
   the installed policy allows it. The default-deny Rego policy of the base ADR's Fork (c)
   resolution (constrained Rego on the existing engine, a required default-deny template, and a
   validation lint) is retained as the configurable layer, and it can only _narrow_ the surface
   further, never widen it: the in-code set membership check runs regardless of what the policy
   engine returns, so a policy bug, a permissive branch, or an errant allow path cannot
   authorize a mutation. The validation lint rejects a read-only policy that is not default-deny,
   that lacks a closed allowlist, or whose allowlist names any verb outside the canonical read
   set. A verb added by a future pack is denied until explicitly classified and listed, not
   permitted by omission.
3. **Membership of the read set is decided by effect, not by name — and effect is judged over
   the whole dispatch lifecycle, not the handler alone.** The authoritative source is a
   per-verb effect classification declared where verbs register — registry-level metadata the
   read-only gate consults at dispatch, not prose documentation (the existing verb-category
   metadata is documentation-only and does not qualify as an enforcement source). A verb
   qualifies as read only if **its entire dispatch closure** — the handler, every nested
   operation the handler invokes, and every dispatch-lifecycle hook that fires for it —
   performs no persistent domain-state change. The lifecycle write classes are enumerated and
   each has a defined disposition under read-only mode:

   - **Domain writes** (rows in domain stores: records, edges, notes, messages, profiles,
     grades, schedule state). Any domain write anywhere in the closure classifies the verb as
     mutating. Incidental writes count: a verb with read-shaped naming that updates state as a
     side effect — the mark-read class of communication verbs, which the durable-audit ADR
     already classifies as writers — is a mutating verb for this tier and is excluded from the
     read set.
   - **Adaptive dispatch hooks** (the recall-scoring hooks that fold implicit signals into
     scoring state when read verbs execute). These are domain-state writes triggered by reads.
     Under read-only mode they are **suppressed**: the read verb dispatches and returns its
     result, and the hook's fold is skipped for that dispatch. Suppression is the mode's
     obligation, not the classification's — a read verb stays classified read, and the mode
     guarantees its dispatch writes nothing.
   - **Store-internal index maintenance** (the ANN lifecycle: recall-triggered index
     rebuilds, consumer-watermark rows, checkpoint and compaction). These are persistent
     writes triggered from the read path that never enter verb dispatch, so no verb gate —
     composite or otherwise — can see them; a gate-only process mode would admit them. They
     are suppressed at the storage layer: read-only mode opens the backing store read-only
     (point 4), and the store's own read-only guard is the same one the index lifecycle
     already honors. A recall under read-only mode returns its result without maintaining
     the index.
   - **Audit-plane appends** (the runtime's own record of what was dispatched, permitted, or
     refused). These are runtime-plane, not domain writes: they record dispatch outcomes and
     are not reachable as a domain write by any argument the caller controls. On a read-only
     backing store they follow ADR-028 Amendment A2 rule 5 verbatim: the registry omits the
     `EventStore` rather than attempting a known-failing append, and every successful
     non-help operation carries the envelope-level advisory
     `audit_persistence_skipped_read_only`. The observability obligation is discharged by
     that per-response advisory — the skip is visible to every caller on every operation —
     not by a write no read-only store can accept.
   - **Non-durable telemetry** (in-memory counters, metrics). Out of scope; no durable state.
   - **Deferred writes belong to the dispatch that scheduled them.** A write scheduled by a
     dispatch but executed after the response returns — a tracked background task, a
     post-return continuation — is part of that dispatch's effect closure, and the mode's
     guarantee covers it. Two enforcement paths exist and both are required. A background
     path that re-enters verb dispatch (the recall pipeline's serve-ledger record is
     dispatched as a verb from a tracked background task) passes through the same process
     gate, so its mutating verb is denied under read-only mode; the scheduling site must
     treat that denial as the mode operating, not an error to escalate — under read-only
     mode it skips the scheduling or logs the refusal at debug level. A background path
     that writes a store directly without re-entering dispatch (the recall-telemetry event
     append) passes no verb gate, so its write site must itself consult the mode and
     suppress domain writes; fail-closed, a background store write not classified under the
     taxonomy above is a domain write and is suppressed.

   The **canonical read set** is the in-code closed list this classification produces: exactly
   the registered verbs whose dispatch closure — deferred writes included — performs no
   domain write and requires no suppression beyond the adaptive-scoring and
   index-maintenance classes above. The
   code constant is the normative artifact; the bundled read-only policy's allowlist is
   generated from it and validated against it, so the two cannot drift. Any verb without a
   declared effect classification is treated as mutating (fail-closed), so the writer census
   cannot rot open as the catalog grows — and the classification is verified behaviorally,
   not by declaration alone: the completeness lint gains a census arm that dispatches every
   read-classified verb against an instrumented store, **joins the runtime's tracked
   background tasks for that dispatch before asserting** (the runtime already tracks them;
   an assertion that races the background lane would pass vacuously), and asserts zero
   domain writes (audit-plane appends excepted per the taxonomy above).

   **Synthetic gate-plane operations are enumerated, not defaulted.** The runtime issues
   gate checks for operations that are not registered verbs. The fail-closed default above
   governs unclassified _verbs_; a synthetic operation the runtime itself issues is instead
   explicitly enumerated and classified alongside the verb classification. Today the sole
   synthetic string is `"authorize"`, and ADR-129 Amendment 1 already classifies it: a
   pseudo-verb is checked against the full authority its result grants, which for the
   token-minting path is **Write** on the primary namespace. This amendment does not
   reclassify it. The composite read-only gate nevertheless carries an explicit enumerated
   admission for `"authorize"`: token minting is permitted under read-only mode. That is a
   mode-local admission decision, not a reclassification, and it is safe for three reasons,
   each a property of the boundary the mode binds to (point 4). A `NamespaceToken` is a
   process-internal object — it is not serializable, and its sealed constructor prevents
   construction outside the authorization path — so the authority it nominally grants
   cannot leave the read-only process. Every verb dispatched inside that process passes
   the same composite gate, so no mutating verb can be exercised through a minted token.
   And a write that bypasses verb dispatch — a runtime-API call made by code holding the
   token — is refused by the read-only backing store; on that path the storage-layer
   binding, not a mint refusal, is the enforcement, and it holds whether or not a token
   exists. Denying the mint would add no enforcement to any of these paths while breaking
   an admitted read verb: coordinator-backed `search` fans out by minting a per-backend
   token for each backend runtime (`authorize_with_visibility`, which issues this same
   `"authorize"` gate check), so a mint denial fails every coordinator-backed search. The
   audit-attachment authorization passes under the same admission and attaches nothing,
   because the registry has already omitted the `EventStore` per the audit taxonomy above
   and the per-response advisory makes that skip visible to every caller. An unenumerated
   synthetic operation remains fail-closed mutating, so the enumeration cannot rot open as
   new synthetic operations appear.
4. **The mode binds to the process boundary it enforces at.** `kkernel mcp` gains a flag that
   sets the process `Gate` to the composite read-only gate instead of `AllowAllGate` for that
   process's lifetime. Binding is local by construction: a read-only process dispatches every
   verb in the process that parsed the flag, and it never forwards verbs to — and never
   auto-spawns — a shared resident daemon, because a forwarded verb would be checked by the
   daemon's own gate rather than this one, and the daemon connection protocol carries no gate
   identity today. The same reasoning bars the process from BEING the daemon: daemon mode
   hosts components that write stores directly without entering verb dispatch (the scheduler's
   pending-event path appends notes outside `VerbRegistry` today), so a verb-dispatch gate
   cannot bound them. The read-only flag therefore **rejects daemon mode at startup** —
   `--read-only` combined with `--daemon` is a launch error, refused before any component
   starts — and the invariant is stated generally so it survives new components: a read-only
   process admits no component that writes a store outside verb dispatch; a component that
   cannot demonstrate that is not started in this mode. The mode also binds the **storage
   layer**, not the gate alone: the flag opens the backing store read-only, so store-layer
   maintenance that no verb gate can see (the index-maintenance class in point 3) is
   suppressed by the same guard the storage engine already honors. A launch that cannot open
   the store read-only fails; it does not fall back to a writable handle. Read verbs
   tolerate this: a read-only connection performs no domain writes
   (the gate denies mutations before any handler runs, and the adaptive hooks are suppressed
   per point 3, and audit persistence is skipped with the ADR-028 A2 advisory per the
   taxonomy above), so local dispatch never contends for the resident daemon's writer lane
   at all. Extending gate identity into daemon connection admission
   and spawn configuration — so a restricted client could safely attach to a shared daemon —
   is structural-gateway work and stays deferred with the untrusted tier. This is the base
   ADR's A2 (mode-flag) process boundary, **not** A1 — see the honest-scope note below.
5. **Refusal is loud, attributable, and typed on the wire.** The `Deny` reason states that the
   verb was refused because the connection is read-only, and the MCP surface carries it as a
   structured refusal object, never a flat serialized error string a client must parse.
   Today's wire path serializes authorization refusals into ordinary error strings, so the
   structured refusal is in-scope wire work for this amendment, not an existing property being
   restated. The wire contract is normative:

   **Placement.** The request-DSL envelope's per-operation failure entry is
   `{ "ok": false, "tool": "<verb>", "error": <value>, "reason"?: "<string>" }` — the
   `error` field is a JSON value, not a fixed string: the serializer emits a plain string
   for most failures and a structured object for khive-typed and retryable failures. This
   amendment does not touch that field in either shape (no existing client's error handling
   breaks). The refusal adds one structured sibling field on the same entry, following the
   envelope's existing pattern of typed fields beside `error` (`reason` today): a `refusal`
   object, present exactly when the process gate denied the operation, absent otherwise. A
   batch refusing one op reports it beside its siblings' results; a single-op request
   carries it on that op's entry.

   **Schema.** The `refusal` object carries exactly these fields:

   ```json
   {
     "class": "read_only" | "denied",
     "verb": "<the refused verb, as dispatched>",
     "mode": "read_only",
     "effect": "read" | "mutating" | "unclassified"
   }
   ```

   `class` is the machine-matching key; `effect` is defined for every class and reports the
   verb's classification (`read` = in the canonical read set, `mutating` = classified as a
   writer, `unclassified` = fail-closed default). Clients match on `class`, never on the
   `error` value, which remains presentation-only.

   **Class mapping.** `class` is decided by WHICH gate refused, in dispatch order, so every
   refusal lands in exactly one class by construction:

   - `class: "read_only"` — the process-mode check this amendment adds refused the verb
     (its `effect` is `mutating` or `unclassified`). The mode check runs before the
     authorization gate and is this amendment's own code, so it can attribute itself
     deterministically. The connection is healthy; the verb is out of contract for this
     mode.
   - `class: "denied"` — the authorization gate refused a verb the mode admitted
     (`effect: "read"`). This class deliberately claims no finer provenance than the deny
     decision itself carries: the gate returns a deny with a diagnostic reason string for
     policy narrowing and for policy-evaluation outcomes that resolve to deny alike, and
     policy-load failures never reach dispatch — a broken policy fails gate construction.
     A wire class that promised to distinguish "your policy said no" from finer deny causes
     would be asserting provenance the deny decision does not carry. Splitting `denied` is
     deferred work, gated on the gate seam itself growing typed deny provenance; until then
     the diagnostic reason string is the only finer signal and remains presentation-only.

   A gate **infrastructure error is not a refusal and is not in this schema**. When
   `Gate::check` itself fails (returns an error rather than a decision), the runtime aborts
   dispatch with its existing distinct `GateUnavailable` error — inherited from the
   authorization-audit contract, not converted to a deny — and that outcome travels the
   ordinary per-operation error path: no `refusal` object, the standard error entry, exactly
   as every other dispatch-aborting infrastructure failure does today. The two-class
   `refusal.class` set is therefore complete over what it covers — deny decisions — and the
   gate-outage branch keeps its own already-typed representation instead of being flattened
   into either class.

   Ordinary runtime errors on a permitted read verb are untouched — no `refusal` field, and
   a read-only connection reports storage timeouts, not-found, and validation errors exactly
   as an unrestricted one does. A client running against a restricted connection can
   therefore report the restriction rather than misreading it as khive being down, without
   string matching.

### Honest scope: what this does NOT serve

The base ADR **rejected A2 (a launch-flag/config process boundary) outright** for the untrusted
caller, because a silent misconfiguration (flag omitted, or a bug in flag handling) reverts to
the full surface, and for an untrusted caller that is an unacceptable failure mode. That
rejection stands. This amendment uses A2 **only for Tier B**, where the threat being defended
against is _the operator's own misconfiguration of a trusted-to-read process_, not an adversary
probing for an escape. For Tier B a launch-flag boundary is proportionate; for Tier C it is not,
and this amendment does not offer it as one.

Concretely, the read-only connection is **not** a containment for a Tier-C untrusted caller:

- A read-only connection still exposes the read verbs, and read verbs are an exfiltration
  surface (the base ADR's "Exfiltration via verbs" threat). An untrusted caller should not hold
  even read access to arbitrary graph state.
- The control for a Tier-C caller is therefore **absence of a khive connection at all**, decided
  at the orchestration/deployment layer, not a read-only policy. A caller that has consumed
  untrusted input is given no khive surface.
- The one future case where a Tier-C caller legitimately needs _live read_ access is exactly
  what the base ADR's A1 structural gateway (thin proxy binary, namespace pinning, enforced
  rate/budget caps) is for. That build stays deferred until such a caller is real; the rate-cap
  enforcement (base ADR rule 5) is a later phase and is **not** part of this amendment.

### Implementation plan (Tier-B phase)

- The per-verb effect classification at the verb-registration seam (point 3), with a
  completeness lint: every registered verb carries a classification, an unclassified verb is
  mutating by default, and the mark-read class of incidental writers is classified as mutating
  with a test pinning that classification.
- The lifecycle census arm of that lint (point 3): every read-classified verb is dispatched
  against an instrumented store, the runtime's tracked background tasks are joined before
  asserting, and the dispatch is asserted to perform zero domain writes, with audit-plane
  appends excepted per the write-class taxonomy.
- Adaptive-hook suppression under read-only mode (point 3): a test that a read verb which
  normally triggers the scoring-fold hook dispatches with the fold skipped, and that the same
  dispatch outside read-only mode still folds.
- Deferred-write suppression under read-only mode (point 3): the recall pipeline's
  background lane as the pinned case — a test that under read-only mode the serve-ledger
  verb dispatch is not escalated as an error and the direct telemetry-event append does not
  write, with the same recall outside read-only mode writing both.
- The composite read-only gate (point 2): in-code canonical read-set membership intersected
  with the policy decision, plus a test that a policy hand-crafted with an extra allow branch
  for a mutating verb is still denied by the composite gate.
- A bundled default-deny read-only Rego policy whose allowlist is generated from the canonical
  read-set constant and validated against it.
- The validation lint (base ADR Fork (c)) that rejects a read-only policy which is not
  default-deny, does not declare a closed allowlist, or names an allowlist entry outside the
  canonical read set.
- The `kkernel mcp` launch flag that installs the composite gate and pins dispatch local
  (point 4): with the flag set, daemon forwarding and daemon auto-spawn are disabled, with a
  test asserting a read-only process never opens the daemon connection path.
- The daemon-mode rejection (point 4): `--read-only` combined with `--daemon` is a launch
  error, with an end-to-end test asserting the process exits before any component starts —
  no scheduler, no store handle, no partial startup.
- The storage-layer binding (point 4): the flag opens the backing store read-only, with a
  test that a launch which cannot open the store read-only fails rather than falling back
  to a writable handle.
- Store-maintenance suppression under the storage binding (points 3-4): index-maintenance
  writes that no verb gate can see — the ANN rebuild/watermark/compaction lifecycle
  triggered from the read path — with a test that a recall under read-only mode returns its
  result while performing no persistent index write, and the same recall outside read-only
  mode still maintains the index.
- The synthetic-operation enumeration (point 3): a test that the enumerated `"authorize"`
  admission holds under read-only mode — coordinator-backed `search` returns results, no
  audit append is attempted because the registry omits the `EventStore`, and every
  successful non-help operation carries the `audit_persistence_skipped_read_only`
  advisory; a test that a write attempted through a minted token is refused at the
  storage layer (the backstop the admission relies on); and a test that an unenumerated
  synthetic operation is denied.
- The structured `refusal` field on the per-operation envelope entry (point 5), carrying the
  normative schema and class mapping above with the `error` field untouched in both its
  existing shapes, with tests that a denied mutating verb returns `class: "read_only"`
  naming the refused verb, that an incidental writer such as a mark-read verb is denied,
  that a policy-narrowed read verb returns `class: "denied"` with `effect: "read"`, and
  that an allowed read verb still dispatches with its ordinary result and errors untouched
  (no `refusal` field).

### Consequences of this amendment

- **Positive.** Ships a real server-side read-only boundary now, on shipped machinery, closing
  the gap where a process's read-only-ness depended on client-side configuration alone. Keeps
  the base ADR's structural boundary honest for the untrusted tier rather than quietly
  substituting the weaker A2 form for it.
- **Negative.** A2's failure mode (a misconfigured launch reverts to the installed base `Gate`)
  is real; this amendment accepts it _only_ for Tier B and says so in the flag's own
  documentation. The effect classification must be maintained as the catalog grows — the
  fail-closed default (unclassified means mutating) makes a new verb fail safe (denied), which
  is the correct direction, but a read verb wrongly left unclassified is a false denial to fix,
  not a hole. Pinning dispatch local means a read-only process forgoes the resident daemon's
  warm caches and shares the store only through concurrent-reader semantics; acceptable for the
  observer-shaped Tier-B caller, and revisited only when gate identity reaches the daemon
  connection protocol.

## References

- ADR-018 - Authorization Gate; `Gate`, `GateRequest`, `GateDecision`, `Obligation`,
  `PackGatePolicy`; this ADR's rate enforcement and gateway-specific validation refusals are
  scoped additions. Gate infrastructure-error handling is inherited from ADR-129, not a
  scoped delta from ADR-018.
- ADR-018 Amendment 1 - canonical verb identity; the allowlist check in rule 1 depends on
  the same canonicalization step to avoid an alias-based bypass
- ADR-016 - Request DSL; the wire surface the gateway's pre-dispatch check intercepts
- ADR-017 - Pack Standard; `VerbRegistry`, `HandlerDef` - unchanged by this ADR, only the
  dispatch path gains a pre-check
- ADR-007 Rev 7 - Namespace as attribution; rule 2's namespace-pinning constrains, but does
  not alter, the existing `namespace=` parameter mechanism
- ADR-096 - Warm Daemon Per-Request Identity; informs Fork (a) A3
- ADR-103 - Resource Attribution Model; potential source of the cost-unit accounting rule 5
  may reuse
- ADR-108 - Git Write Surface; the companion Phase B ADR this ADR's Fork (d) composes with
- ADR-085 - Code Pack; precedent for a deliberately admin-CLI-only surface distinct from the
  agent-facing MCP surface (`kkernel code-ingest`), informing the "no admin CLI verbs" rule

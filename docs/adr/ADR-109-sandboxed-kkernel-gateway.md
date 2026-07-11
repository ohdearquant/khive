# ADR-109: Sandboxed kkernel Gateway for Untrusted Execution (Phase C)

**Status**: Proposed\
**Date**: 2026-07-11\
**Authors**: khive maintainers\
**Depends on**: ADR-018 (Authorization Gate), ADR-016 (Request DSL), ADR-017 (Pack
Standard), ADR-007 Rev 7 (Namespace as Attribution-Only)\
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
  out-of-allowlist argument shape, or a Gate infrastructure error must deny, not fall
  through to a permissive default the way ADR-018's base Gate fails open on infrastructure
  errors.

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
   exceeded - all result in denial. This explicitly reverses ADR-018's fail-open-on-`Err`
   posture (see Rationale below) for the gateway path only; the base Gate's fail-open
   behavior is unchanged for the operator and trusted-agent tiers.

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
 "deny", ...}` pattern (already the documented fail-closed idiom in ADR-018's own example
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

## References

- ADR-018 - Authorization Gate; `Gate`, `GateRequest`, `GateDecision`, `Obligation`,
  `PackGatePolicy`; this ADR's enforcement additions (rules 5 and 6) are explicit, scoped
  deltas from ADR-018's declared-not-enforced `RateLimit` and fail-open-on-`Err` defaults
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

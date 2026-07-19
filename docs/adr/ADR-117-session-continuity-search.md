# ADR-117: Session Continuity — Cross-Session Search and Remote Ingestion

**Status**: proposed
**Date**: 2026-07-19
**Authors**: khive maintainers
**Depends on**:

- [ADR-007](ADR-007-namespace.md) — Namespace as Attribution-Only Open String (Rule 8: a pack with
  a principal-scoped backend carries its own isolation contract; the broker's server-side principal
  scope is the Gate for that backend)
- [ADR-013](ADR-013-note-kind-taxonomy.md) — Note Kind Taxonomy (the `session` note kind the T1 verbs
  read and write)
- [ADR-014](ADR-014-curation-operations.md) — Curation Operations (deletion is a curation-layer
  operation that removes data, not a view-layer filter)
- [ADR-018](ADR-018-authorization-gate.md) — Authorization Gate (the pre-dispatch `Gate::check`
  contract the isolation requirement must compose with, and whose fail-open default a follow-on ADR
  carves a fail-closed verb class out of)
- [ADR-021](ADR-021-memory-pack.md) — Memory Pack (the hybrid FTS + vector recall with RRF fusion
  the search capability reuses rather than re-implements)
- [ADR-025](ADR-025-verb-speech-acts.md) — Verb Speech Acts (the Assertive category `session.search`
  joins)
- [ADR-083](ADR-083-session-pack-t1-verbs.md) — Session Pack T1 Verb Surface (the existing
  `store`/`list`/`resume`/`export` verbs over the `notes` table, and the separate mirror aux tables
  this ADR searches)

---

## This is a direction ADR

This document establishes the **direction** for session continuity: the capability, the properties it
must have, and the requirements those properties impose. It deliberately does **not** define the
schema, verb APIs, or enforcement mechanisms that satisfy those requirements. Each of those carries a
proof obligation — a schema migration, an isolation test, a deletion lifecycle — that belongs to a
focused implementation ADR with its own spec-gate pass, not to a direction document.

The mechanism-and-proof work is split into named follow-ons (see **Follow-on ADRs** below):

- **ADR-117a** — session identity and tenant isolation
- **ADR-117b** — deletion and retention
- **ADR-117c** — continuity bridge (a search hit resumes and exports)
- **ADR-117d** — remote cross-machine ingestion

The properties named in D2, D4, D5, and D6 are stated here as **requirements**, never as delivered
behavior. They become delivered only when the follow-on that carries each one lands, with its proof.

**Capability-consumption rule.** Nothing markets, documents as available, or takes a runtime
dependency on session continuity until the follow-ons a given slice needs are **live**. The v1
single-machine search slice needs ADR-117a (identity + isolation) and ADR-117b (deletion) live, and
ADR-117c (continuity bridge) for hits to be useful; cross-machine search additionally needs
ADR-117d. Until then, session continuity is a direction with in-flight mechanisms, not a shipped
feature.

---

## Context

The session pack (`khive-pack-session`) persists agent-session data in **two distinct stores**, a
split that [ADR-083](ADR-083-session-pack-t1-verbs.md) makes deliberately:

1. **Caller-authored session notes.** The four T1 verbs (`session.store` / `list` / `resume` /
   `export`) read and write the shared `notes` table. These are records a caller explicitly stores.
2. **Passively-mirrored transcript tables.** A background mirror tails Claude Code, Codex CLI, and
   ChatGPT exports and writes three auxiliary tables — `sessions` (one row per transcript, carrying
   `provider_session_id`, source, cwd, git_branch, timestamps), `session_messages` (the parsed
   message text plus raw line), and `session_mirror_cursor` (a byte-offset cursor). ADR-083 keeps
   these separate from the notes precisely because one is caller-authored and one is passive
   ingestion.

Two facts about the mirror as it exists today shape every requirement below, and both were verified
against source rather than assumed:

- **The persistence identity is a bare global primary key.** `session_messages.id` and `sessions.id`
  are each `TEXT PRIMARY KEY` with no account or tenant component
  (`crates/khive-pack-session/src/vocab.rs`). `session_messages.id` is populated from
  `ParsedEvent.uuid` — the Claude Code top-level `uuid`, the ChatGPT `message.id`, or the synthesized
  `"{session_id}:{byte_offset}"` for Codex (`crates/khive-pack-session/src/mirror/parse.rs`). Those
  are **provider event ids**, unique enough for the single-writer mirror's idempotency, but not
  tenant-scoped: two accounts that produced the same provider event id would collide on one primary
  key. The `namespace` column is nullable and is not part of any key. **The mirror is single-tenant
  by construction.**
- **There is no search over message content.** `session.resume` fetches a single session by id;
  `session.list` is a recency browse. The transcript text sits in `session_messages` and is not
  indexed for retrieval.

The capability this ADR sets direction for — search across an account's own session history, reaching
back over retained history and (later) across machines — cannot be layered onto that mirror without
first making the identity tenant-scoped, defining how isolation is enforced where rows exist, defining
how the most sensitive data the substrate holds is deleted, and defining how a search hit reconnects
to `resume`/`export`. Those are the requirements below; the mechanisms are the follow-ons.

---

## Decision

### D1 — A `session.search` verb, FTS5 first

The capability is one Assertive verb:

```
session.search(query, limit?, since?, source?, cwd?)
  -> [{ session, score, snippets }]
```

It runs keyword retrieval over `session_messages.text`, scoped to the caller's authenticated tenant
(the isolation requirement, D4), and returns matching sessions ranked by score with the matching
snippets. The optional filters narrow by recency (`since`), originating tool (`source`), and working
directory (`cwd`).

The search corpus is the mirror's `session_messages`, where transcript text lives. Caller-authored
session notes (Context store 1) are metadata records, not transcript text, and are out of the search
corpus in v1; a returned hit's identity is nonetheless required to resume and export (D6).

Version 1 is **FTS5-only**. Session-search queries are keyword-rich by nature — a user searching their
own history remembers the terms they typed — and full-text retrieval covers that query class directly.
FTS5 over `session_messages.text` needs an index and a triggered content table, the same pattern the
note substrate already uses. Ranking and fusion primitives are shared with the memory pack (ADR-021),
not re-implemented; a later vector signal (D2) fuses in without a signature change.

The verb **shape** is decided here. The verb **does not ship until ADR-117a lands**, because a search
verb that returns transcript rows is unsafe to expose before the tenant-isolation enforcement it
depends on exists and is proven (D4). This gating is a hard condition, carried from the direction to
the follow-on.

### D2 — Message identity must be tenant-scoped (requirement → ADR-117a)

**Requirement.** Before search can be exposed, the mirror's persistence identity must be **tenant-
scoped**, so that no two accounts can collide on one row and no query can reach another account's rows
by construction. The mirror is single-tenant today (Context); making it scoped is the first thing
ADR-117a must do — for example a uniqueness contract over `(account, provider_session_id, event)` or
an equivalent scoping, decided with the migration in ADR-117a.

**Ruling: identity is not a content hash.** Whatever the scoped identity is, it is not a hash of
message content. This is a decided design position, not a deferred mechanism:

- A content-only identity **collides across tenants** — identical message text in two accounts would
  map to one row, leaking or losing data across the isolation boundary.
- A content-only identity **collapses legitimate repeated events** — the same text occurring twice in
  one session (a command retyped, a repeated tool result) is two events, and hashing them to one
  identity silently loses the second.

A content hash is still useful, but as a **dedup / integrity adjunct** on the scoped id, never the id:
it detects whether a re-streamed event bearing the same scoped identity carries the same content
(integrity on idempotent re-stream), and it gates a future embedding backfill so an unchanged event is
not re-embedded.

A vector signal is a gated later addition, not part of v1. The scoped-identity schema must be
embedding-ready (a stable per-event id plus the content hash make a later embed backfill idempotent
and re-runnable), but embedding every message is a cost that scales with history size and rides the
same serialized retrieval path that khive#1121 addresses, so the vector add is gated on that work the
same way the multi-engine default change (khive#1116) is. Until vector search ships, every surface
describes the capability as **full-text search across sessions**, never semantic search.

### D3 — Cross-machine ingestion is a principal-scoped backend (requirement → ADR-117d)

**Requirement.** Spanning multiple originating machines under one account is a **principal-scoped
backend** in the sense of [ADR-007](ADR-007-namespace.md) Rule 8: a follow-on ADR (ADR-117d) must
define the mechanism, authentication model, migration path, and failure modes, and the backend's
**server-side per-principal scope is the Gate for that backend** (ADR-007 Rule 8 §3) — it composes
with the single-seam authorization model, it does not sit outside it.

The direction is that the existing mirror gains a **remote sink**: instead of writing only a local
table, it can stream parsed events to a per-account ingestion endpoint. ADR-117d must give that
endpoint a real, non-optional contract: authentication that binds a credential to an account (the
authenticated principal, never a client-supplied account string, decides which account events land
under); an encrypted transport; origin verification and replay protection; and enumerated failure
modes that reject and surface rather than silently accept or drop. The byte-offset cursor already
collapses cold backfill and live tailing into one loop (a fresh machine starts at cursor zero), so no
separate uploader tool is needed; re-streaming stays idempotent by the scoped identity (D2).

Cross-machine ingestion is **off the v1 single-machine critical path**. v1 search reads the local
mirror; ADR-117d extends the corpus to every machine that streams to the account. The direction ADR
names it so the identity (D2) and isolation (D4) requirements are designed to hold across machines
from the start, not retrofitted.

### D4 — Tenant isolation must be fail-closed, enforced where rows exist (requirement → ADR-117a)

Session transcripts contain everything a user typed and was shown. In a multi-tenant deployment one
account's sessions must be unreachable by another account's query. This is a hard requirement on the
search capability; ADR-117a carries the enforcement mechanism and its proof. The direction ADR fixes
the **constraints** that mechanism must satisfy, because those constraints are what make a naive
design wrong:

- **The [ADR-018](ADR-018-authorization-gate.md) Gate cannot be the row-isolation mechanism.** The
  Gate is a pre-dispatch check over request metadata (`actor`, `namespace`, `verb`, `args`,
  `context`) that returns Allow/Deny. It runs **before** the handler and therefore never sees matched
  rows; and by ADR-018's fifth principle it **fails open on infrastructure errors**. Row-level
  isolation can therefore be neither a property of the Gate's pre-dispatch decision nor a consequence
  of the Gate failing safely.
- **The scope must be the authenticated tenant, never a caller argument.** A caller-supplied
  `namespace` is forgeable; resolving scope from it is the prior v1 anti-pattern ADR-007 removed.

ADR-117a must therefore enforce isolation **where the rows exist** — a non-widenable predicate at the
handler seam, keyed to the authenticated tenant — and make the verb **fail-closed by construction**:
`session.search` requires a positive authenticated tenant scope to execute, so that ADR-018's fail-open
default cannot leak session data (fail-open yields no authenticated scope, and no scope yields no
results). ADR-117a codifies this at the contract level by amending ADR-018 to designate a **fail-closed
verb class**, with `session.search` as its first member — and the enforcement structure is
construction-primary: the safety property holds in the shipped seam without depending on the amendment
landing first, and the amendment is contract-level codification of a property the seam already
guarantees. ADR-117a lands this enforcement, a cross-account isolation test, and a no-scope-refusal
test **in the same change as `session.search`** (the D1 gate). Isolation is a property proven by test,
not asserted in prose.

### D5 — Deletion and retention must be executable across every derived surface (requirement → ADR-117b)

**Requirement.** Deletion and retention over session transcripts must **remove data, not hide it**
([ADR-014](ADR-014-curation-operations.md)), and must be complete across every surface the mirror data
is derived onto. ADR-117b carries the verb APIs, the tombstone lifecycle, and the surface-by-surface
semantics; the direction ADR fixes the scope those verbs must cover and the retention model.

- **Retention is bounded by a configurable storage cap, not a time window.** Within the cap, history
  is kept indefinitely, so search reaches back across the full retained span; at the cap the account
  ages out its oldest sessions first, and that aging is an executable, surfaced operation, never a
  silent background deletion.
- **User-initiated deletion is available from day one**, at two granularities — a single session and a
  full account wipe.
- **Deletion is not complete until every derived surface is handled.** ADR-117b's contract must
  enumerate, with verb-level semantics: the `session_messages` rows and the `sessions` row; the FTS5
  index (the triggered content table), so deleted content is not still matchable; future vector rows
  (when D2's vector signal ships), on the same path; `session_mirror_cursor`, where a deleted session
  is tombstoned by scoped id so a subsequent re-stream of the same source file does not resurrect it;
  remote copies at the ingestion sink (D3) and any in-flight / retry state for that account; and any
  cached or hydrated search results. Retention aging is the same oldest-first operation over the same
  surfaces.

### D6 — A search hit's identity must resume and export (requirement → ADR-117c)

**Requirement.** `session.search` reads the mirror's `sessions` / `session_messages`, while
`session.resume` and `session.export` operate on the T1 note store and today accept only UUID/hex ids
that resolve to session notes (ADR-083). A search result therefore returns a mirror session identity
(`provider_session_id` and its `sessions` row key), and that identity must be **resolvable by `resume`
and `export`** — a hit is useless if it cannot be reopened or exported.

ADR-117c must pick **one** bridge and resolve it against ADR-083's UUID-only resolution — not leave it
as an open alternative. The two candidate shapes are (a) `resume`/`export` accept the mirror session
identity directly, or (b) a defined resolution maps the mirror identity to the T1 store. ADR-117c
chooses one and specifies it; the direction ADR only requires that the chosen bridge exist so every
`session.search` result carries an identity guaranteed to resume and export. The caller-authored note
store and the mirror store stay distinct per ADR-083; the bridge connects their identities for
continuity, it does not merge the schemas.

---

## Follow-on ADRs (the mechanism carriers)

Each follow-on gets its own spec-gate pass. This direction ADR is signed off on the **coherence of its
requirements**; each follow-on is signed off on **proof of its mechanism**.

| Follow-on                                          | Carries                                                                                                                                                                                            | Hard conditions                                                                                                                                                                                                   |
| -------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **ADR-117a** — session identity + tenant isolation | The scoped-identity migration (D2); the fail-closed handler-seam predicate keyed to the authenticated tenant (D4); the ADR-018 fail-closed-verb-class amendment; and `session.search` itself (D1). | The scoped-identity migration, the enforcement predicate, a cross-account isolation test, and a no-scope-refusal test all land **in the same PR** as the enforcement. `session.search` does not ship before this. |
| **ADR-117b** — deletion + retention                | The deletion verb APIs and tombstone lifecycle, executable across every surface D5 enumerates; the storage-cap retention model and its oldest-first aging operation.                               | Deletion is proven complete across all enumerated surfaces, including the FTS index and (when present) vector rows and remote copies.                                                                             |
| **ADR-117c** — continuity bridge                   | The single resume/export bridge for a search hit's identity (D6), resolved against ADR-083's UUID-only resolution.                                                                                 | Exactly one bridge shape, specified; no unresolved alternative.                                                                                                                                                   |
| **ADR-117d** — remote cross-machine ingestion      | The principal-scoped ingestion backend (D3): authentication and credential-to-account binding, encrypted transport, origin verification and replay protection, enumerated failure modes.           | Off the v1 single-machine critical path; the identity (D2) and isolation (D4) requirements must hold across machines by design, not retrofit.                                                                     |

---

## Consequences

**What this ADR delivers.** A coherent direction and a fixed set of requirements: the capability
(`session.search`, FTS5-first), the identity property it needs, the ingestion model that later spans
machines, the isolation property that makes it safe to operate multi-tenant, the deletion and retention
model, and the continuity bridge back to resume/export. It delivers the **decomposition** into
follow-ons that can each be proven at their own gate, and the sequencing and gating that binds them (D1
gated on ADR-117a; the capability-consumption rule gating any dependence on the whole).

**What it does not deliver.** No schema, no verb implementation, no enforcement code, no test. Those
are the follow-ons. Nothing in this ADR is a delivered property; the properties become real only as
ADR-117a/b/c/d land with their proofs.

**Cost.** The dominant future cost is D2's vector mode: embedding every message is compute and storage
that scales with history size, which is why v1 is FTS5-only and the vector add is gated on khive#1121.
Retrieval latency is the same shape measured for `memory.recall`, so khive#1116 and khive#1121 are
direct cost levers on this capability.

**Sensitivity.** Holding full transcript history is a data-sensitivity obligation. That is exactly why
identity scoping (ADR-117a), fail-closed proven isolation (ADR-117a), authenticated ingestion
(ADR-117d), and executable deletion (ADR-117b) are load-bearing requirements with their own gates, not
optional polish folded into a direction.

**Surface delta, once the follow-ons land.** One new verb (`session.search`); one internal
principal-scoped ingestion endpoint; deletion and retention verbs; one ADR-018 amendment (the
fail-closed verb class); and the continuity bridge for resume/export. The existing four T1 verbs are
unchanged by this direction; if ADR-117c's chosen bridge extends `resume`/`export`, that change is
specified and proven in ADR-117c, not assumed here. The local single-source mirror remains for the
single-machine case; the remote sink is an added ingestion mode, not a replacement.

---

## Alternatives considered

**One mega-ADR that specifies all mechanisms inline.** Rejected. Isolation, deletion, and continuity
each carry a proof obligation — a schema migration, a deletion lifecycle, a resolved identity bridge —
that a reviewer can and should check at source. Bundling all three into one document made each promise
a claim the review could disprove against the actual schema and Gate contract, which is a structural
reject loop, not a drafting problem. Splitting each mechanism to a focused follow-on with its own gate
lets the direction land on requirement-coherence while each mechanism is proven where its proof lives.

**Content-hash message identity.** Rejected as incorrect (D2): it collides identical messages across
tenants and collapses legitimate repeated events. The hash is retained as an integrity/dedup adjunct on
the scoped id instead.

**Gate-decision row isolation / relying on Gate fail-safety.** Rejected as unenforceable (D4): the
ADR-018 Gate is pre-dispatch, sees no rows, and fails open, so row isolation cannot be a property of
its decision. The isolation requirement is a mandatory authenticated-tenant predicate at the handler
seam, fail-closed by requiring a positive scope — carried by ADR-117a.

**A handler-level namespace filter on a caller-supplied namespace.** Rejected as forgeable and as the
prior v1 anti-pattern (ADR-007): the scope must be the authenticated tenant, not a caller argument.

**A separate uploader tool for cross-machine ingestion.** Rejected as redundant (D3): the mirror's
zero-cursor cold start already performs bulk backfill through the same loop as live tailing.

**A time-window retention bound.** Rejected in favor of a storage cap (D5): a fixed window either
discards history search should still reach or grows unbounded; a cap keeps history reachable up to a
declared limit and makes aging an explicit, surfaced event.

**Searching the caller-authored note store and the mirror as one corpus in v1.** Deferred, not
adopted: ADR-083 keeps the two stores distinct by design. v1 searches the transcript text where the
searchable content is, and the continuity bridge (D6) reconnects hits to resume/export; unifying the
corpora is a later option, not a v1 requirement.

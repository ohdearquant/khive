# ADR-112: Outbound GitHub Publish Verbs with a Publication-Hygiene Scan

**Status**: Proposed\
**Date**: 2026-07-13\
**Authors**: khive maintainers\
**Depends on**: ADR-088 (Git-Lifecycle Pack) and its Amendment 1 (`git.digest`), ADR-108
(Git Write Surface Through khive, Phase B), ADR-018 (Authorization Gate), ADR-017 (Pack
Standard), ADR-016 (Request DSL), ADR-004 (Substrate Observables - `Event` store used for
audit), ADR-013 (Note Kind Taxonomy)\
**Related**: ADR-002 (Edge Ontology - `annotates`), ADR-007 Rev 7 (Namespace as
Attribution-Only)

## Context

Callers write GitHub content - issue bodies, PR descriptions, review comments, and
release notes - through the raw `gh` CLI today. That content is free text assembled from
caller prose, relayed messages, file contents, or other generated output, and nothing
inspects it before it reaches a public repository. An incident in which internal-only
vocabulary reached a public issue body (via text relayed from another internal channel,
not authored directly by the publishing agent) showed the failure mode concretely: the
content was cleaned up after the fact, once it was already visible externally.

Two protections exist or are planned for this problem, deliberately layered in order of
arrival:

1. **Deployed today**: a client-side pre-tool-use hook that scans `gh` invocations before
   they run and denies on a pattern match. This is per-agent-process enforcement - it only
   applies where the hook is installed and current.
2. **This ADR**: outbound-publish verbs on the git pack, with the same class of scan
   enforced server-side, inside the verb handler, before any GitHub API call. Once this
   surface exists, the hook narrows from "scan and decide" to "deny raw `gh` content
   writes outright, point the caller at the verb" - the verb becomes the one path a scan
   cannot be bypassed on, regardless of which hook version an agent process happens to be
   running.
3. **Later, not specified here**: `gh` is denied for content writes entirely once the verb
   surface has proven itself; the verbs become the only path. That transition is a
   deployment and hook-configuration change, not a khive code change, and is out of
   this ADR's implementation scope (see Migration).

### Relationship to ADR-108

ADR-108 (Phase B) specs `git.commit`, `git.branch`, and `git.push` - verbs that mutate a
local git repository and its remote refs. Those verbs never construct a GitHub-side
content object; they move commits and branches. This ADR is the complementary surface:
verbs that create GitHub-side content objects (issues, comments, pull requests, releases)
through the GitHub API, authored by an agent, addressed to a public or external audience.
The two compose rather than overlap: an agent typically pushes a branch through ADR-108's
`git.push`, then opens the pull request against it through this ADR's `git.publish_pr`.
Neither verb set performs the other's operation - ADR-108's verbs never call the GitHub
API, and this ADR's verbs never touch git objects or refs.

This ADR does not amend ADR-108. It reuses two of ADR-108's resolved positions as
precedent rather than re-deriving them: the hardened-shell-out execution model (ADR-108
Fork (b), resolved B2 - argv-only construction, no shell interpolation, a fixed
subcommand/flag surface, and a required adversarial security review at implementation
time), and the full-audit-via-Event-substrate rule (ADR-108's hard rule 2, restated below
for this surface's typed audit payload).

## Decision

### Verb table (four verbs)

| Verb                  | Args                                                                 | Returns         |
| --------------------- | -------------------------------------------------------------------- | --------------- |
| `git.publish_issue`   | `repo`, `idempotency_key`, `title`, `body`, `labels?`, `assignees?`  | `{url, number}` |
| `git.publish_comment` | `repo`, `idempotency_key`, `target`, `body`                          | `{url, id}`     |
| `git.publish_pr`      | `repo`, `idempotency_key`, `head`, `base`, `title`, `body`, `draft?` | `{url, number}` |
| `git.publish_release` | `repo`, `idempotency_key`, `tag`, `title?`, `notes`                  | `{url}`         |

Verb naming follows the `git.publish_*` family (resolved fork - see Resolutions). All four
verbs are `git`-pack verbs, `pack.verb` namespaced per ADR-023, dispatched through the
existing `VerbRegistry` / `Gate` seam every other khive verb uses - no new dispatch
mechanism.

`idempotency_key` is required on all four verbs. It is a caller-generated UUID in
canonical lowercase hyphenated form and identifies one logical publication across retries.
The handler applies defaults, preserves array order, sorts object keys lexicographically,
serializes the normalized arguments other than `idempotency_key` as compact UTF-8 JSON,
and stores the BLAKE3 hash of those bytes. The generated reconciliation marker is not part
of the hash. The same key with the same hash resumes that operation; reusing it with a
different hash is rejected before any network call. The implementation stores the key as
`operation_id` in the recovery ledger described below. A successful retry returns the
cached remote result. This explicit key is necessary because neither `gh` nor a GitHub
create operation supplies a transactional boundary shared with khive's graph store.

### Comment target grammar

`git.publish_comment.target` has exactly this ASCII grammar:

```abnf
target           = target-kind "#" positive-integer
target-kind      = "issue" / "pr"
positive-integer = nonzero-digit *DIGIT
nonzero-digit    = %x31-39
```

Valid examples are `issue#42` and `pr#978`. Parsing is case-sensitive and the decimal
number must fit in `u64`. The handler rejects zero, a leading zero, a sign, any whitespace,
overflow, a bare number, a URL, or any string that embeds a repository. Examples of invalid
input include `issue#0`, `issue#042`, `issue #42`, `PR#42`, `42`,
`https://github.com/org/repo/issues/42`, and `org/repo#42`.

The `repo` argument is the only repository authority. After local grammar validation and
the hygiene scan, but before a comment write, the handler performs a read-only lookup in
that exact repository and verifies both that the number exists and that its remote object
kind matches `issue` or `pr`. A missing object, a kind mismatch, or any attempt to encode a
different repository in `target` fails before the comment is published.

Deliberately excluded from v0:

- Edit and delete of already-published content. The same scan applies unchanged when these
  are added; they are not blocked on any design question here, only sequenced later (see
  Migration).
- A dedicated review-verdict comment verb. This is the highest-volume outbound path today,
  and its templates are already sanitized by convention; it is a stronger candidate for
  the second wave once v0's scan module has field evidence behind it, not a v0 requirement.

### Handler pipeline

Each verb executes the following steps in order. No remote write occurs until validation,
authorization, and hygiene scanning have completed:

1. **Argument and repo checks.** Arguments are normalized, `idempotency_key` is validated,
   the comment-target grammar is validated where applicable, and `repo` is checked against
   `[git] publish_repos` (daemon config; see "Repo allowlist" below). An unregistered repo
   fails fast, independent of content - there is no reason to scan text for a repository
   this daemon can never publish into.
2. **Publication-hygiene scan.** Every free-text field submitted to the verb - `title`,
   `body`, `notes`, and every string inside `labels`/`assignees` - is scanned by the three
   layers described in "Scan module" below. The scan is origin-agnostic: it does not
   distinguish an agent's own prose from relayed or pasted text. There is no trusted-source
   bypass and no `force=true` parameter on any verb.
3. **Deny path.** Any hit not covered by an allowlist escape produces a synchronous deny to
   the caller (see "Deny semantics") and an additional typed audit record (see "Audit and
   the event plane"). No GitHub API call is made.
4. **Comment-target read check.** For `git.publish_comment`, the handler performs the
   repository-scoped, kind-aware read described in "Comment target grammar". This is a
   read-only validation call, not a content write.
5. **Claim the durable operation.** The handler inserts the pending-operation row in state
   `unconfirmed_publish`, including the normalized request and its generated reconciliation
   marker. This commit happens before `gh` is spawned. An existing row follows the recovery
   state machine; only a proven `not_published` row may enter the create path.
6. **GitHub API call.** For a newly claimed or proven `not_published` operation only, the
   verb shells the configured GitHub CLI (`gh`) under the daemon's identity - the same
   transport ADR-088's ingester and Amendment 1's `git.digest` already use, argv-only, no
   shell interpolation, matching the discipline
   ADR-108 Fork (b) required for `git`. `gh` is reused rather than a direct REST client for
   the same reason ADR-088 §5 gave: it already handles auth and pagination correctly for
   this environment. The fixed marker described in "Publish recovery state machine" is
   appended after user content has passed the scan.
7. **Persist the remote receipt.** Once GitHub returns, the handler durably stores the
   remote URL and number or id and moves the operation to `published_pending_ingest`
   before attempting any graph write.
8. **Idempotent self-ingest.** The handler reconciles the corresponding graph record and
   `annotates` edge (see "Dual write"), then moves the operation to
   `ingested_pending_audit`.
9. **Typed publication audit and return.** The handler appends the additional
   `EventKind::Audit` publication record. Only after that append is durable does it mark
   the operation `complete` and return the shape in the table. A resumed operation returns
   the same cached remote identity.

Verb dispatch passes through the Gate (ADR-018) exactly as every other khive verb does -
this is inherited from the existing dispatch path, not a new mechanism this ADR
introduces. The publication-hygiene scan is a separate concern from Gate authorization:
Gate answers "may this actor call this verb at all," enforced through pluggable policy;
the scan answers "does this specific content contain a class of string this system must
never let reach GitHub," enforced as fixed pattern matching inside the handler,
deliberately not delegated to Gate policy. The two are independent gates in series, not
alternatives to each other.

### Deny semantics

On a scan hit, the verb returns the standing per-op failure shape (`{ok:false, ...}`,
matching every other khive verb's error contract) with scan-specific fields:

```json
{
  "ok": false,
  "tool": "git.publish_issue",
  "error": "publication-hygiene: denied",
  "hits": [
    { "field": "body", "pattern_id": "actor-token-namespace-prefix", "excerpt": "...te***** the" }
  ]
}
```

- `hits` lists every field/pattern combination that matched (not just the first), so a
  caller can fix everything in one pass instead of retrying repeatedly.
- `excerpt` shows masked context around the match - enough for the caller to locate the
  offending text, never the full matched span unmasked. A deny response must not itself
  become a channel for the content it is denying.
- The batch does not abort on a deny: a failed publish op in a multi-op `request` batch is
  one failed entry among others, per the standing khive batch contract.
- No silent rewrite. The verb never substitutes, truncates, or auto-corrects denied text;
  the caller fixes the text and retries. Silent rewriting was explicitly rejected: it
  teaches the caller nothing and can change the meaning of a message without anyone
  noticing.

### Scan module

Three layers, evaluated in order, all inside the verb handler:

1. **Token denylist.** Pattern matching (regex, fail-closed) against categories described
   in the Pattern File Format section below: actor and deployment identifier tokens, internal
   process and workflow vocabulary that would be meaningless or confusing to an external
   reader, internal filesystem paths, and commercial-strategy vocabulary in an
   OSS-facing context. Patterns are pack data, not code - a versioned file, editable and
   auditable without a binary release. This document deliberately does not enumerate the
   concrete denylist terms; per the Pattern File Format section, those live only in the
   pattern files (a generic in-repo class list plus a private overlay), never in prose
   documentation. An ADR that listed the actual internal tokens would itself be exactly the
   kind of publication-hygiene violation this system exists to prevent.
2. **Secret scan.** Reuses the existing `secret_gate` module's compiled patterns (the same
   ones ADR-088 §5 applies at ingest) against the same fields. Unlike the ingest path,
   which masks a detected secret and keeps the record, outbound publish **denies** on a
   secret-scan hit and masks nothing silently. The directionality is deliberate: inbound
   content is sanitized and kept because the record has independent value once the secret
   is removed; outbound content that would carry a live secret must never leave in any
   form, masked or not, and the caller must know the check fired so they can rotate or
   remove the credential rather than merely lose it from a git message.
3. **Allowlist escapes.** Certain tokens are legitimate in certain repos - a product name
   that also matches an actor-token pattern, for example. Escapes are declared per
   `(repo, pattern_id)` pair in the same config file the patterns live in (see Pattern File
   Format), never as a per-call parameter. There is no `force=true` escape on any verb; an
   operator who needs an exception edits the versioned allowlist file, and that edit is
   itself reviewable through the same process as any other configuration change.

### Repo allowlist

`[git] publish_repos = ["org/repo", ...]` in daemon configuration, read at daemon startup

- resolved as per-daemon config (see Resolutions, F4). A repo not explicitly listed is
  never writable through this surface. The explicit allowlist is authoritative: an operator
  must not add a fork or external repository slug to it, and a slug that is not listed is never
  writable regardless of its origin. This is distinct from, and
  does not replace, Gate-level authorization: the allowlist bounds which repos this daemon
  process may ever publish into, regardless of which actor is calling; Gate policy (if
  configured beyond the permissive default) further bounds which actors may call the verb at
  all. Promoting the allowlist to centrally managed admin-plane data is deferred (Resolutions
  F4) until a multi-daemon deployment needs it enforced consistently across daemons rather
  than per-daemon.

### Audit and the event plane

The existing typed surface is authoritative. Every domain-specific row required here uses
the closed `EventKind::Audit` variant (serialized as `"audit"`), the precise top-level
`verb` (`git.publish_issue`, `git.publish_comment`, `git.publish_pr`, or
`git.publish_release`), an `EventOutcome`, and the storage Event's JSON `payload`. This ADR
does not add `hygiene_deny`, `git.publish`, or any other `EventKind` variant, and does not
use a nonexistent Event `properties` field. The row has `substrate = SubstrateKind::Event`
and `payload_schema_version = 1`. It is appended through a runtime helper that stamps the
same gate-resolved namespace and actor as the automatic audit row.

Outcome mapping is exact:

- A hygiene rejection uses `EventOutcome::Denied` (`"denied"`).
- A completed remote publication whose local ingest and required audit append both
  succeeded uses `EventOutcome::Success` (`"success"`).
- A validation or transport failure, an unconfirmed remote result, or an unfinished
  recovery stage uses `EventOutcome::Error` (`"error"`) for that invocation. A later
  successful recovery emits its own success record; append-only history is not rewritten.

Every additional row carries these required payload keys:

| Key             | Contract                                                                                                                                      |
| --------------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| `audit_type`    | `publication_hygiene` for a scan deny; `github_publish` otherwise                                                                             |
| `verb`          | The same precise publish verb as the Event's top-level `verb`                                                                                 |
| `repo`          | Canonical `owner/name` slug, or `null` if repository validation failed                                                                        |
| `target`        | `issue`, `pr`, the canonical comment target such as `issue#42`, `tag:<tag>` for a release, or `null` if target validation failed              |
| `operation_id`  | Canonical idempotency UUID supplied by the call, or `null` if key validation failed                                                           |
| `state`         | Recovery state after this invocation, or `not_claimed` if the operation ledger was not reached                                                |
| `rule_ids`      | Sorted, unique pattern ids on deny; empty array otherwise                                                                                     |
| `field_count`   | Number of distinct denied fields on deny; zero otherwise                                                                                      |
| `remote_url`    | Published URL on success and whenever already known during recovery; `null` otherwise                                                         |
| `remote_number` | Positive issue/PR number on issue or PR success; `null` for comments, releases, denies, and errors before that identity is known              |
| `remote_id`     | Comment id on comment success; `null` for issues, pull requests, releases, denies, and errors before that identity is known                   |
| `stage`         | `validation`, `scan`, `remote_publish`, `remote_reconcile`, `graph_ingest`, or `audit_append` on denied/error outcomes; `complete` on success |

No title, body, notes, labels, assignee, matched span, or excerpt is stored in this
payload. In particular, `rule_ids` identifies every hygiene rule hit without persisting
the rejected content.

These rows are **additional to**, not replacements for, the dispatch-audit row that
`VerbRegistry` already attempts for every call. The automatic row also has
`EventKind::Audit`, but carries the generic Gate `AuditEvent` payload and the dispatch
outcome. After the Gate allows dispatch, the handler adds one hygiene/publication-domain
row for its invocation, identified by `payload.audit_type`. Thus a normal handler call has
the generic dispatch row plus one extra domain row; a Gate-denied call has only the generic
row because the handler never runs. Retries create their own dispatch history. Event reads
use `list(kind="event", verb="git.publish_issue", ...)` (or another precise verb) and
inspect `payload.audit_type`; ADR-022 still excludes `query()` / GQL / SPARQL for events.

The success-domain row uses UUIDv5 over the operation UUID's canonical string under a fixed,
code-defined git-publish audit namespace UUID. This makes its Event id deterministic
without accepting a caller-selected Event id. A duplicate-key result for the same operation
and payload is treated as already recorded. The operation cannot advance from
`ingested_pending_audit` to `complete` until this row is durable. This makes the
handler-owned success audit recoverable even though the registry's generic dispatch audit
remains best-effort.

The additional row is a synchronous, required write. If a validation or hygiene-deny row
cannot be appended, the handler fails closed and performs no remote write. If an audit
append fails after a remote publication, the operation remains in its recoverable ledger
state; it is never reported complete merely because the registry may have written its
best-effort generic row.

This is the v0 audit surface for hygiene enforcement. There is no per-deny notification to
an actor's inbox or messaging channel in v0 (Resolutions, F3) - the synchronous deny
response to the caller plus this queryable audit row are sufficient for v0. A push-based
feedback loop is deferred until evidence shows that event-plane records alone are
insufficient.

### Dual write: self-ingest via ADR-088 note kinds

A successful publish reconciles a graph record through the existing generic `create`,
`update`, and `link` verbs - no new graph verb, no new edge relation, `annotates` only,
matching ADR-088's own usage. The repo-anchor `project` entity is resolved exactly as
`git.digest` resolves it (ADR-088 Amendment 1): match on `properties.repo_url`, or create
the anchor if none is found and report that creation.

Issue and PR self-ingest uses the same natural key as the current digest implementation:

```text
(kind, namespace, properties.number, properties.project_id)
```

`properties.project_id` is the full UUID string of the resolved repo-anchor project.
Repository-scoped GitHub numbers are never looked up by `kind` and `number` alone. The
handler performs a create-or-update upsert on this key: if no note exists it creates one;
if one exists it updates the governed fields from the remote read-back while preserving
unrelated extension properties. It then ensures one, and only one, `annotates` edge from
that note to the resolved project. Replaying either step is a no-op once the desired note
and edge exist.

The remote read-back after publication (or marker reconciliation) supplies the complete
property shapes used by `git.digest` today:

- An `issue` note has `name = "#<number> <title>"`, the marker-free remote body in
  `content`, and common properties `number`, `title`, `author`, `created_at`, `closed_at`,
  `labels`, and `project_id`. `state_reason` is included when GitHub returns one and must
  satisfy ADR-088's governed lowercase enum.
- A `pull_request` note has `name = "#<number> <title>"`, the marker-free remote body in
  `content`, and common properties `number`, `title`, `author`, `created_at`, `merged_at`,
  `closed_at`, `base_ref`, `head_ref`, and `project_id`.

Nullable remote fields are retained as JSON `null`, matching the current ingest shape;
arrays such as issue `labels` are present even when empty. The namespace in the natural key
is the storage namespace from the handler's `NamespaceToken`, matching `git.digest`.
Self-ingest does not substitute a publication URL for any of these common properties.

- `git.publish_release` and `git.publish_comment` have no dedicated ADR-088 note kind -
  ADR-088's taxonomy covers `commit`, `issue`, and `pull_request` only. Rather than adding
  new pack-owned note kinds for this ADR's own purposes, both use the existing base
  `reference` note kind (ADR-013), with `content` set to the release notes or comment body
  with the khive reconciliation marker removed, `properties.url` set to the published URL,
  and `properties.publish_operation_id` set to the operation UUID. The latter is the
  reference-note upsert key used during recovery. A `git.publish_comment` targeting an
  already-ingested issue or pull request `annotates` that note; if the target was never
  ingested, it `annotates` the repo-anchor `project` entity instead. This mirrors ADR-088
  Amendment 1's best-effort enrichment precedent: no match means a narrower edge, never a
  second remote publish.

This graph reconciliation runs synchronously after a successful GitHub response and is
resumed from durable state after a failure; it is not deferred to the next digest sweep or
to a background job. The required regression is: publish an issue or PR, run `git.digest`
for the same repository until `done`, and assert exactly one note with that natural key and
exactly one `annotates` edge from it to the repo project.

### Publish recovery state machine

GitHub and the graph store cannot share a transaction. The git pack therefore owns a
durable `git_publish_operation` ledger. Its minimum persisted fields are `operation_id`
(the idempotency UUID, primary key), `namespace`, `verb`, `repo`, a canonical request hash,
the normalized request needed for local replay, `marker`, `state`, `remote_url`,
`remote_number`, `remote_id`, `note_id`, `audit_event_id`, `last_error`, `created_at`, and
`updated_at`. The stored request has already passed the hygiene and secret scans, is local
daemon state, and must never be copied into an Event payload or error response.

The closed v0 state set and transitions are:

```text
new -> unconfirmed_publish
unconfirmed_publish -> not_published
not_published -> unconfirmed_publish
unconfirmed_publish -> published_pending_ingest
published_pending_ingest -> ingested_pending_audit
ingested_pending_audit -> complete
```

- **`unconfirmed_publish`** is committed before `gh` is spawned. It means a remote create
  may not have started, may have failed without a response, or may have succeeded without
  its response becoming durable. A retry in this state never issues another create.
- **`not_published`** is reachable only when `std::process::Command::spawn` itself returns
  an error, proving that no child process and therefore no remote request started. A retry
  with the same key may move back to `unconfirmed_publish` and make the first remote
  attempt. No child exit status or output-parse failure is strong enough for this state.
- **`published_pending_ingest`** requires a durably stored remote URL and the applicable
  remote number or id. It means GitHub accepted the object but graph reconciliation is not
  yet complete. Retries perform only the local upsert and edge reconciliation.
- **`ingested_pending_audit`** means the graph note and edge are reconciled but the
  operation-level success audit has not yet been confirmed durable. Retries perform only
  the idempotent audit append.
- **`complete`** requires remote identity, graph reconciliation, and the success-domain
  audit. Retries return the stored result without network or graph mutation.

For remote reconciliation, every create appends this fixed, inert marker after the scanned
user content:

```html
<!-- khive-publish:<operation_id> -->
```

The marker contains only the opaque publication UUID and is generated by the handler after
the caller content passes the scan. It is applied to issue and PR bodies, comment bodies,
and release notes. Graph self-ingest strips exactly this generated trailing marker from
`content`; it does not remove arbitrary HTML comments supplied by the caller.

On a retry from `unconfirmed_publish`, the handler performs a read-only, repo- and
object-kind-scoped search for the exact marker. One match supplies the remote identity and
advances to `published_pending_ingest`; multiple matches are an integrity error; no match
leaves the operation unconfirmed and returns an error carrying `operation_id` and
`state`, but no publish content. It never calls a GitHub create command. An operator may
resolve a persistently unconfirmed operation only after independently establishing whether
the remote object exists; silently changing the idempotency key is not a recovery action.

The crash and failure windows are therefore explicit:

| Window                                                   | Durable state              | Retry behavior                                                                 |
| -------------------------------------------------------- | -------------------------- | ------------------------------------------------------------------------------ |
| Before the operation insert commits                      | No operation               | The request may claim its key; no remote write has occurred                    |
| Spawn fails before a child exists                        | `not_published`            | A same-key retry may safely attempt the first create                           |
| After spawn, during `gh`, before receipt commit          | `unconfirmed_publish`      | Read-only marker reconciliation; never create                                  |
| After receipt commit, before/during note or edge write   | `published_pending_ingest` | Upsert note and ensure edge; never create                                      |
| After graph reconciliation, before/during audit append   | `ingested_pending_audit`   | Append the deterministic success audit idempotently; never create or re-ingest |
| After audit append, before the ledger reaches `complete` | `ingested_pending_audit`   | Duplicate Event id proves audit durability, then mark complete                 |

The ledger update that records remote identity is committed before the first graph write.
The graph upsert and edge ensure are independently idempotent because a crash can occur
between them. The domain-separated deterministic success Event id closes the final
append-versus-ledger window. No retry with a possible or confirmed prior remote attempt
blindly reissues a GitHub create; only the proven pre-spawn `not_published` state permits
another attempt.

### `gh` transport and degradation

Same transport as ADR-088 and Amendment 1: the daemon shells the configured `gh` CLI,
argv-only construction (`std::process::Command`, no shell string interpolation), no new
token storage. Degradation posture is the opposite of the read/ingest path's: where
ADR-088's ingester skips gh-dependent work with a warning when `gh` is unavailable or
unauthenticated, this ADR's publish verbs treat that same condition as a **hard error**.
A publish verb never silently skips and never falls back to an alternate transport. A
failure to spawn `gh` is a confirmed no-publish hard error. Once the child process starts,
an error or missing parseable response is conservatively `unconfirmed_publish`, because
GitHub may have accepted the request before the local failure became visible. The caller
receives `{ok:false, error: "publication state unresolved", operation_id, state}` and must
retry with the same idempotency key; recovery follows the read-only marker path above.
This asymmetry is deliberate: skipped ingest work is recoverable on the next digest pass,
whereas a retried create could duplicate public content.

## Pattern File Format (normative)

The token-denylist (scan layer 1) and allowlist-escape (scan layer 3) patterns are defined
in TOML files, loaded by both the server-side verb handler and the client-side pre-tool-use
hook described in the Context section. Both layers must reach the same allow/deny decision
on the same content - the sections below are the contract that makes that possible across
two independent implementations.

This normative contract covers only scan layers 1 and 3. Scan layer 2 (the secret scan)
reuses the existing, already-deployed `secret_gate` module and its own pattern set; it is
Rust-only today and unaffected by this ADR. A hook implementation that wants secret-scan
parity with the server maintains its own secret-detection mechanism (for example, a
gitleaks-style scanner with a versioned allowlist) rather than consuming this file format
for that layer. Convergence of the secret-scan layer onto a single shared representation is
not required by this ADR.

### Two files, one merged pattern set

1. **In-repo generic pattern file** - versioned in the khive repository, public-visible.
   Contains only generic pattern _classes_: a pattern that matches the _shape_ of an
   actor-namespace token, an internal-path prefix, or org-mechanics phrasing, never a
   concrete internal identifier, alias, or literal internal term. This file must never contain
   concrete internal-identifier tokens; if a pattern would only make sense with a concrete literal
   internal term hardcoded into it, that pattern does not belong in this file - it belongs
   in the overlay.
2. **Local overlay file** - not versioned in the repository, resolved from a
   daemon-configured path (for example, an environment variable or a `[git]` config key
   pointing outside the repository tree, matching the operational shape the git-digest
   scratch cache already uses for daemon-owned paths outside version control). Contains the
   concrete internal tokens: internal identifiers, aliases, literal internal-process phrasing. This
   file is a private, per-installation overlay - it is never committed, never published,
   and is out of scope for this ADR's public-repo artifact.

### Merge semantics

At load time (daemon startup for the verb handler; hook startup or first-invocation for
the client-side scanner), the two files are merged into one pattern set:

- The in-repo file loads first, the overlay file loads second and its patterns are
  appended.
- Every pattern's `id` field must be unique across the merged set. If the overlay defines
  an `id` that already exists in the in-repo file, the loader fails closed at startup - an
  overlay is additive only; it cannot redefine or silently shadow a pattern the in-repo
  file ships. This prevents a misconfigured local overlay from quietly weakening the
  generic pattern set.
- A missing overlay file is not an error - the merged set is simply the in-repo file's
  patterns alone. A malformed overlay file (fails to parse as valid TOML matching the
  schema below) is an error and fails closed: the daemon does not start with a partially
  loaded pattern set, and the hook does not run with a partially loaded pattern set either.

### Pattern entry schema

```toml
[[pattern]]
id = "actor-token-namespace-prefix"
category = "actor_token"
regex = '(?i)\bnamespace:[a-z0-9_-]+\b'
description = "actor-namespace-style token"
severity = "deny"

[[pattern]]
id = "internal-path-worktree"
category = "internal_path"
regex = '/[A-Za-z0-9_./-]+/agent-worktrees/'
description = "local worktree absolute path"
severity = "deny"
```

Fields:

- `id` (required, string, unique across the merged set) - stable identifier referenced in
  deny responses (`hits[].pattern_id`) and in deny-audit `payload.rule_ids`.
- `category` (required, one of `actor_token` | `org_mechanics` | `internal_path` |
  `commercial_strategy`) - the pattern class. `secret` is deliberately not a valid value
  here; secret patterns live in the separate `secret_gate` module (see above).
- `regex` (required, string) - the match pattern. See "Regex portability" below for the
  syntax restriction that keeps two independent implementations in agreement.
- `description` (required, string) - human-readable, shown in review/audit contexts; never
  shown to end users in a deny response beyond the `pattern_id`.
- `severity` (required, string, `"deny"` in v0) - present as a field now so a future
  graduated-severity model (for example, `"warn"`) does not require a schema migration; v0
  recognizes only `"deny"` and treats any other value as a load-time error.

### Allowlist escape entry schema

```toml
[[allow]]
repo = "org/khive"
pattern_id = "actor-token-namespace-prefix"
reason = "the product name matches this pattern's shape; legitimate in this repo only"
```

- `repo` (required, exact `org/name` string) - no wildcard. An escape applies to exactly
  one repository.
- `pattern_id` (required, exact match against a `[[pattern]]` `id`) - no wildcard. An
  escape suppresses exactly one pattern.
- `reason` (required, non-empty string) - the file's own diff is the review surface for
  exceptions; an escape without a stated reason does not load.

An `[[allow]]` entry suppresses a match for the named `(repo, pattern_id)` pair only. It
never suppresses a pattern globally, never suppresses secret-scan hits (layer 2 has no
allowlist - a detected secret is always denied), and there is no equivalent per-call
parameter on any verb.

### Regex portability

Because the same file must be consumed by a Rust implementation (the verb handler) and a
separate implementation in the hook's own language, patterns are restricted to a portable
subset that both a linear-time regex engine (for example, Rust's `regex` crate, which is
RE2-derived) and common scripting-language regex engines can execute identically:

- No lookahead or lookbehind assertions.
- No backreferences.
- No possessive or atomic groups.
- Only character classes, anchors (`^`, `$`, `\b`), quantifiers, alternation, and inline
  case-insensitivity flags (`(?i)`) are permitted.

A pattern file containing a construct outside this subset fails to load in either
implementation - this is intentionally a hard parse-time failure, not a warning, because a
pattern that only one of the two layers can execute is precisely the condition under which
the two layers could disagree about the same content, which this format exists to prevent.

### Match semantics

- Each pattern is applied independently to each candidate string field of a publish
  request (`title`, `body`, `notes`, and each string within `labels`/`assignees`) - never
  to a concatenation of fields, so that a reported hit's `field` value is accurate.
  Whether a candidate scans every field or only free-text fields is a verb-level decision
  (see Handler pipeline step 2); the file format itself does not vary by field.
- A pattern matching anywhere within a field's string is a hit for that field. An
  implementation is not required to find every possible match within a single field for a
  single pattern - one is sufficient to deny - but the reference verb implementation
  reports the first match per `(field, pattern_id)` pair in `hits[]` for completeness of
  the audit record.
- File location, load order relative to other daemon startup steps, and caching/reload
  behavior are implementation and configuration detail, not part of this normative
  section. What is normative - and must not diverge between the two consuming
  implementations - is: the TOML shape above, the cross-file `id`-uniqueness rule, the
  regex-portability subset, and the per-field, per-pattern hit semantics.

## Implementation requirements and verification

The implementation ships the verbs, operation ledger, pattern loader, scan, audit rows,
graph reconciliation, and tests as one change. In addition to unit coverage for the
pattern file, it must include these contract tests:

1. **Typed audit surface.** Allow, hygiene-deny, transport-error, and recovery-error cases
   append additional rows with `EventKind::Audit`, the precise top-level verb, the correct
   `EventOutcome::{Success, Denied, Error}`, and every required JSON payload key. Tests
   assert that no new `EventKind` spelling is introduced and that deny payloads contain
   rule ids but no rejected content or excerpt. They also distinguish the handler-owned
   row from the automatic generic dispatch row by `payload.audit_type`.
2. **Comment target validation.** Table-driven handler tests accept `issue#42` and
   `pr#978`; reject zero, leading zero, case changes, signs, whitespace, overflow, bare
   numbers, URLs, and embedded repositories; and reject a repository-scoped read whose
   remote kind disagrees with the parsed kind. A transport spy proves rejection occurs
   before any comment write.
3. **Digest-compatible idempotency.** Publish an issue and a PR, then run `git.digest` on
   the same repo until complete. For each remote number, assert one note under
   `(kind, namespace, number, project_id)`, the full common property shape, and one
   project `annotates` edge. Repeat self-ingest and digest to prove the counts remain one.
4. **Recovery failure injection.** Inject a crash or store error after each boundary in
   the recovery table: operation insert, remote response, note upsert, edge ensure, audit
   append, and completion update. Resume with the same idempotency key and assert that the
   remote create spy observed exactly one create, unfinished local work completed, and the
   final success Event exists exactly once. The unconfirmed case must exercise marker
   recovery and the no-match error path.
5. **Idempotency-key conflict.** Reusing a key with identical normalized arguments returns
   or resumes the original operation; reusing it with different arguments fails before
   transport.

`tests/smoke_test.py` must cover one allowed publish against a controlled fake `gh`, one
hygiene deny, one comment-target validation failure, and one resumed
`published_pending_ingest` operation. Unit and integration tests use a fake transport; the
test suite must not create real GitHub content.

## Out of Scope (v0)

- **Reads.** `gh pr view`, checks, API reads, and any other read-only GitHub operation are
  untouched by this ADR; it governs content writes only.
- **CI and workflow operations, merges.** Not content-bearing in the sense this ADR
  addresses; they stay on direct `gh`/platform mechanisms.
- **Repository-level git writes** (commit, branch, push) - ADR-108's surface, not
  duplicated or amended here.
- **Edit and delete of already-published content.** Named in Migration as second-wave
  work; the scan applies unchanged when these verbs are added.
- **Semantic scanning.** The scan is pattern matching plus evidence-tuned additions, not
  natural-language understanding. A pattern miss is corrected by adding a pattern, not by
  adding classification logic; this keeps the scan auditable as a diff, at the cost of not
  catching paraphrased violations a semantic classifier might.
- **Per-deny actor notification** (Resolutions, F3) and **admin-plane repo allowlist data**
  (Resolutions, F4) - both explicitly deferred, not rejected.

## Migration

1. This ADR through review and sign-off.
2. **v0**: the four publish verbs, the scan module (pattern file loader, three-layer scan,
   deny path), typed `EventKind::Audit` domain records, the durable operation ledger, the
   idempotent self-ingest, and the repo allowlist ship together, with coverage added to
   `tests/smoke_test.py`.
3. **Deployment notice and hook convergence**: outbound GitHub content publication moves
   to the verb surface. The existing client-side hook converges onto the same pattern file
   this ADR defines (rather than an independently maintained rule set) and narrows its role
   to denying raw `gh` content writes outright, pointing the caller at the equivalent verb
   - it no longer needs to make its own allow/deny judgment once the verb enforces the
     identical scan server-side.
4. **Second wave** (a follow-on ADR or amendment, not specified by this document): the
   review-verdict comment path adopts the verb surface, and edit verbs are added, reusing
   the same scan module.
5. **Eventual**: `gh` denied outright for content writes once the verb surface has proven
   itself in the second wave. This is a hook/process configuration change, not a khive code
   change, and is out of this ADR's implementation scope.

## Resolutions

Four forks were presented for this design; each is resolved in place.

1. **Verb naming (F1)**: `git.publish_issue`, `git.publish_comment`, `git.publish_pr`,
   `git.publish_release` - the `git.publish_*` family, over a bare-noun alternative
   (`git.issue`, `git.comment`, ...). **Resolved**: the `publish_*` family, so the verb
   name itself signals "this leaves the daemon and reaches GitHub," distinct from
   `git.digest`'s read direction and from ADR-108's repo-mutation verbs.
2. **Pattern file location and shape (F2)**: whether the denylist lives entirely in-repo,
   entirely in a private overlay, or split. **Resolved**: split. Generic pattern classes
   ship in-repo, versioned and public-visible; the concrete internal-token list is a
   private local overlay file, merged at load per the rules in "Pattern File Format"
   above. The in-repo file must never contain concrete internal-identifier tokens.
3. **Deny-event notification (F3)**: whether a scan deny also notifies the calling actor's
   own inbox or messaging channel, in addition to the synchronous deny response.
   **Resolved**: no per-deny notification in v0. The synchronous deny to the caller plus
   the additional `EventKind::Audit` row with `payload.audit_type =
   "publication_hygiene"` together are the v0 audit surface. Revisit only if evidence
   shows deny records going unreviewed in practice.
4. **Repo allowlist ownership (F4)**: per-daemon static configuration, or admin-plane data
   shared across daemons. **Resolved**: per-daemon config (`[git] publish_repos`) for v0.
   Centrally managed allowlist data is deferred until a deployment with multiple daemons
   needs consistent enforcement across them rather than per-daemon configuration.

## Consequences

### Positive

- A single, server-side enforcement point for outbound GitHub content hygiene, reachable
  by every caller through the standard verb dispatch path - not contingent on which hook
  version, if any, an individual agent process happens to have installed.
- A durable, queryable audit trail on the existing typed Event surface that closes the
  "cleaned up after the fact" gap the motivating incident exposed, and gives future
  pattern-tuning work an evidence base instead of anecdote.
- Same-call KG visibility of caller-authored GitHub content via recoverable self-ingest,
  without waiting on the next digest sweep.
- Pattern data is versioned and reviewable as an ordinary file diff, rather than scattered
  across independently maintained hook and verb implementations that could silently drift
  apart.

### Negative

- Hard-error degradation on `gh` unavailability means a publish verb can block a caller's
  task on transport flakiness, where the read/ingest path would have degraded gracefully.
  This is a deliberate tradeoff: a silently skipped publish is worse than a blocked one.
- Every remote object carries an HTML-comment reconciliation marker that is observable to
  anyone who inspects the source content.
- A post-spawn failure with no marker match remains `unconfirmed_publish` rather than
  guessing that no object exists. This can require operator reconciliation, deliberately
  preferring a blocked operation over duplicate public content.
- The recovery ledger stores outbound content that has passed the scan so local graph work
  can be replayed. It is daemon-local state and expands the data-retention surface.
- The overlay-file split (F2) is an operational dependency, not a solved problem: a missing
  or stale overlay changes what the scan catches, and if the verb handler and the hook load
  overlays from different paths or versions, the two layers can still drift despite sharing
  one file format. The format in this ADR makes convergence possible; it does not enforce
  that both deployed copies are kept in sync.
- Pattern matching has an inherent false-negative and false-positive profile. It is not
  semantic, so a paraphrased violation is not necessarily caught, and a generic pattern can
  over-match a legitimate use (mitigated, not eliminated, by the allowlist-escape
  mechanism). This ADR requires evidence-driven tuning as an ongoing discipline; it does
  not claim to close the gap once.

## Alternatives Considered

| Alternative                                                      | Why not adopted                                                                                                                                                                                                                                                                           |
| ---------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Enforce hygiene only at the client-side hook, no server verb     | Does not compose with the eventual "raw `gh` denied for content writes" endpoint: any caller not running a current hook has no enforcement at all. A server-side verb is the one chokepoint every caller must pass through regardless of local hook state.                                |
| Semantic or LLM-based content classification instead of patterns | Rejected for v0. Patterns are deterministic, auditable as a file diff, and portable across two independent implementations by construction; a classifier's decisions are neither deterministic nor reviewable in the same way, and out-of-scope per this ADR's scope decision.            |
| A `force=true` per-call escape parameter                         | Rejected. Defeats the review property the allowlist file provides: a legitimate exception must be a versioned, reviewable config edit, never a call-site flag any caller can set for itself.                                                                                              |
| A single merged pattern file, no in-repo/overlay split           | Rejected. The in-repo file must never carry concrete internal-identifier tokens (F2); a single file would either leak internal terms into a public repository or force every installation to maintain a full private copy instead of layering a small overlay onto a shared generic base. |
| Fold outbound publish into `git.digest` with a write mode        | Rejected, matching ADR-108's identical rejection of overloading `git.digest`: it is read/ingest-shaped, and conflating it with a write-and-scan operation mixes two operations with entirely different audit and failure-mode needs.                                                      |

## References

- ADR-088 - Git-Lifecycle Pack; `commit`/`issue`/`pull_request` note kinds and `annotates`
  usage this ADR's dual write reuses unchanged.
- ADR-088 Amendment 1 - `git.digest`; the project-anchor resolution logic and `gh`
  transport conventions this ADR follows for the publish direction.
- ADR-108 - Git Write Surface Through khive (Phase B); the repo-level write surface this
  ADR is scoped alongside, not duplicated with. The scan module described here is a
  candidate for future adoption by ADR-108 surfaces (for example, scanning a `git.commit`
  message) - not specified by this ADR, noted as a natural extension point.
- ADR-018 - Authorization Gate; the dispatch-time authorization seam every verb, including
  this ADR's four, passes through independent of the hygiene scan.
- ADR-017 - Pack Standard; `HandlerDef`, `PackRuntime::dispatch`, the mechanism these verbs
  register through.
- ADR-016 - Request DSL; the wire surface these verbs are reachable through.
- ADR-004 - Substrate Observables; `Event` store, the audit-persistence sink for both the
  deny and allow paths.
- ADR-013 - Note Kind Taxonomy; the base `reference` note kind this ADR's dual write reuses
  for `git.publish_release` and `git.publish_comment`, in place of new pack-owned note
  kinds.
- ADR-007 Rev 7 - Namespace as attribution; publish-verb dispatch stamps namespace/actor
  exactly as every other verb does, no new namespace semantics.
- `crates/khive-types/src/event.rs` and `crates/khive-storage/src/event.rs` - the closed
  `EventKind`, `EventOutcome`, and JSON payload storage contract used by the additional
  audit rows.
- `crates/khive-runtime/src/pack.rs` - the existing generic dispatch-audit path whose row
  remains separate from this ADR's handler-owned domain row.
- `crates/khive-runtime/src/secret_gate.rs` - existing secret-detection module, reused
  unchanged as scan layer 2.
- `crates/khive-pack-git/src/ingest.rs` - the digest natural key, issue/PR property shapes,
  and existing `gh`/`git` shell-out precedent this ADR follows.

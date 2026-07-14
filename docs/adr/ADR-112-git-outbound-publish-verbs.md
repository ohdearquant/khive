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

### Required-argument wire and validation contract

Every required argument is a JSON string on the request wire. Omission, JSON `null`, a
non-string JSON value, and an invalid UTF-8 request are errors; none is coerced to an empty
string or another type. Validation applies to the decoded string, so a prohibited scalar is
still prohibited when written as a JSON escape. The handler validates every required field
before hashing, claiming an operation, or invoking `gh`.

| Argument                                                                    | Wire type | Empty-string rule                   | Size and content contract                                                                                                                                                            |
| --------------------------------------------------------------------------- | --------- | ----------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| All verbs: `repo`                                                           | `string`  | Rejected                            | 3-140 ASCII bytes and exactly one `/`; must satisfy the canonical repository grammar below                                                                                           |
| All verbs: `idempotency_key`                                                | `string`  | Rejected                            | Exactly 36 ASCII bytes; canonical lowercase hyphenated UUID (`8-4-4-4-12` hexadecimal digits)                                                                                        |
| `git.publish_issue.title`, `git.publish_pr.title`                           | `string`  | Rejected                            | 1-256 Unicode scalar values; no Unicode control or format scalar (`General_Category=Cc` or `Cf`)                                                                                     |
| `git.publish_issue.body`, `git.publish_comment.body`, `git.publish_pr.body` | `string`  | Accepted and distinct from omission | 0-65,536 Unicode scalar values; no Unicode format scalar (`Cf`) and no Unicode control scalar except horizontal tab (`U+0009`), line feed (`U+000A`), and carriage return (`U+000D`) |
| `git.publish_comment.target`                                                | `string`  | Rejected                            | 4-26 ASCII bytes and the closed comment-target grammar below                                                                                                                         |
| `git.publish_pr.head`, `git.publish_pr.base`                                | `string`  | Rejected                            | 1-255 ASCII bytes and the GitHub branch-name subset below                                                                                                                            |
| `git.publish_release.tag`                                                   | `string`  | Rejected                            | 1-255 ASCII bytes and the GitHub tag-name subset below                                                                                                                               |
| `git.publish_release.notes`                                                 | `string`  | Accepted and distinct from omission | 0-65,536 Unicode scalar values; no Unicode format scalar (`Cf`) and no Unicode control scalar except horizontal tab (`U+0009`), line feed (`U+000A`), and carriage return (`U+000D`) |

Required strings are otherwise preserved byte-for-byte. The handler does not trim them,
apply Unicode normalization, change line endings, or case-fold them. An accepted empty
`body` or `notes` value therefore remains an empty string in the canonical request; the
generated nonce-bearing reconciliation marker is transport metadata and is not part of that
value.
The `Cc`/`Cf` rejection is performed on decoded Unicode scalars before preservation or
scanning. Only the multiline `body` and `notes` fields permit any `Cc` scalar, and their
closed exception set is HT, LF, and CR. Single-line titles, the optional release title,
and issue labels permit neither `Cc` nor `Cf`. The ASCII grammars for `repo`,
`idempotency_key`, `target`, `head`, `base`, `tag`, and assignees reject both categories
by construction.

The `idempotency_key` syntax accepts every canonical 128-bit UUID spelling except the nil
UUID (`00000000-0000-0000-0000-000000000000`); it imposes no version or variant-bit
restriction. Uppercase hexadecimal, braces, a `urn:uuid:` prefix, missing hyphens, and any
other spelling are rejected rather than normalized.

`repo` is canonical only when both components are already lowercase ASCII and satisfy all
of these rules:

```abnf
repo        = owner "/" repo-name
owner       = lower-alnum [*37owner-char lower-alnum]
owner-char  = lower-alnum / "-"
repo-name   = lower-alnum [*98repo-char lower-alnum]
repo-char   = lower-alnum / "." / "_" / "-"
lower-alnum = %x61-7A / DIGIT
```

- `owner` is 1-39 ASCII lowercase alphanumeric-or-hyphen characters, starts and ends
  alphanumeric, and contains no consecutive hyphens.
- `name` is 1-100 ASCII lowercase alphanumeric, `.`, `_`, or `-` characters, starts and
  ends alphanumeric, and contains no consecutive dots.
- The wire value is exactly `owner/name`: no hostname, scheme, port, leading or trailing
  slash, repeated slash, `.git` transport suffix, query, fragment, or surrounding
  whitespace. Uppercase spellings are rejected rather than silently case-folded.

Every `[git] publish_repos` entry and every pattern-file `[[allow]].repo` value must satisfy
this same grammar at configuration load time. The validated wire `repo` must then equal one
configured `publish_repos` entry byte-for-byte. This exact validated value is the only repo
form stored, hashed, audited, or sent to the transport.

`head`, `base`, and `tag` use this closed ASCII ref subset:

```abnf
ref       = segment *("/" segment)
segment   = ref-first *ref-rest
ref-first = ALPHA / DIGIT / "_"
ref-rest  = ALPHA / DIGIT / "_" / "-" / "."
```

The whole value is 1-255 bytes. In addition to the grammar, it must not equal `HEAD`, start
with `refs/`, contain `..`, have a segment ending in `.` or `.lock`, or have an empty
segment. These checks are case-sensitive except that the reserved name `HEAD` is rejected
in any ASCII case. This subset accepts ordinary names such as `main`, `feature/adr-112`,
and `v1.2.3`, while rejecting whitespace, control characters, backslash, `~`, `^`, `:`,
`?`, `*`, `[`, a leading dash, and fully qualified ref spellings. `head` is a branch in the
same repository named by `repo`; fork-qualified PR heads such as `owner:branch` are not
supported in v0 and are rejected because `:` is outside the grammar. `base` is likewise a
branch name, and `tag` is a tag name; the caller never supplies `refs/heads/` or
`refs/tags/`.

In v0, `tag` must name a pre-existing remote tag in `repo`. The handler verifies existence
with the read-only check in the pipeline below, and the release-create command independently
enforces that boundary with its fixed `--verify-tag` flag. This verb never creates, moves, or
deletes a tag or any other ref.

The `gh` process boundary has a fixed interpretation. Code selects the executable,
subcommand, and complete option-name set for each verb; no caller string can select or add
a command, subcommand, option, environment-variable name, API field name, or output parser.
Except for the sole release-tag exception below, every caller-controlled value emitted in
the `gh` argument vector is a separate value bound to a code-selected, fixed value-taking
option. The builder must not otherwise use a command form that requires an unbound
caller-controlled positional operand: it must instead choose an equivalent fixed-option
form, encode the value through structured stdin under fixed field names, or reject before
spawn. It never concatenates a value into an option name or shell string. Typed booleans may
select only their one documented fixed flag, and arrays expand only into repetitions of
their documented fixed value-taking option. Thus an accepted free-text value such as a
title beginning `--` remains data, while an option-looking identifier such as a `repo`,
`target`, `head`, `base`, or `tag` beginning `-` fails its grammar before transport.

The **sole positional exception** is the tag operand required by `gh release create`. It is
not free caller input at argv-construction time: before the operation is claimed, it must
pass the closed ASCII ref grammar above and the read-only preflight must prove that the exact
`refs/tags/<tag>` already exists in `repo`. Failure of either check rejects the operation
without publishing. The builder then emits the fixed, closed shape
`gh release create --verify-tag <fixed options> -- <validated-tag>`, with the validated tag
as the only operand after the mandatory `--` end-of-options separator. Release assets and
all other positional operands are forbidden. The separator remains mandatory even though
the grammar already rejects a leading `-`, whitespace, and shell or option
metacharacters. Every other caller-controlled value on every verb remains bound to a fixed
value-taking option as specified above.

### Canonical request and optional-argument contract

The optional arguments have one wire shape and one normalized representation:

| Argument                      | Wire type       | Default and normalization                                                                                                                                                   | Validation limit                                                                                                                                |
| ----------------------------- | --------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------- |
| `git.publish_issue.labels`    | `array<string>` | Omitted and `[]` both normalize to `[]`. Otherwise, reject exact duplicates and sort strings by unsigned UTF-8 byte order before hashing and transport.                     | At most 100 entries; each entry is 1-50 Unicode scalar values and contains no Unicode control or format scalar (`General_Category=Cc` or `Cf`). |
| `git.publish_issue.assignees` | `array<string>` | Omitted and `[]` both normalize to `[]`. Normalize each login to ASCII lowercase, reject duplicates after lowercasing, and sort by unsigned UTF-8 byte order.               | At most 10 entries; each entry is 1-39 ASCII alphanumeric-or-hyphen characters, starts and ends alphanumeric, and has no consecutive hyphens.   |
| `git.publish_pr.draft`        | `boolean`       | Omitted and `false` both normalize to `false`; `true` remains `true`.                                                                                                       | Boolean only.                                                                                                                                   |
| `git.publish_release.title`   | `string`        | Omitted, `""`, and a value exactly equal to the normalized `tag` all normalize to the normalized `tag`. The handler always passes the resulting non-empty title explicitly. | The resulting title is 1-256 Unicode scalar values and contains no Unicode control or format scalar (`General_Category=Cc` or `Cf`).            |

For these four optional arguments, JSON `null` is invalid rather than another spelling of
omission. String content is otherwise preserved byte-for-byte: the handler does not trim
whitespace, case-fold labels, or apply Unicode normalization. Array order is not semantic;
the sorted forms above are the only forms stored, hashed, or sent to `gh`. "Normalized
`tag`" in the release-title rule means the exact required `tag` string after validation;
v0 does not trim, case-fold, or otherwise rewrite it.

`idempotency_key` is required on all four verbs. It is a caller-generated UUID in canonical
lowercase hyphenated form. One logical publication across retries is identified by the
three-part operation identity `(namespace, verb, idempotency_key)`, where `namespace` is the
write namespace from the handler's `NamespaceToken` and `verb` is the exact dispatched
publish verb. Every ledger lookup and mutation must constrain all three components.

The handler validates all arguments, applies the defaults and array normalization above,
and constructs a canonical object containing every verb argument other than
`idempotency_key`, including the explicit default values. It sorts object keys
lexicographically by unsigned UTF-8 bytes, serializes the object as compact UTF-8 JSON, and
stores the BLAKE3 hash of those bytes. The generated reconciliation nonce and marker are not
part of the hash. Consequently, omitted and explicit defaults hash identically, as do
permutations of the same labels or assignees. The same three-part operation identity with
the same hash resumes that operation; reusing it with a different hash is rejected before
any network call. Reusing the UUID in a different namespace or for a different verb is a
distinct operation and must never return or mutate the first operation's cached receipt.
The implementation stores the UUID component as `operation_id` in the recovery ledger
described below. A successful same-identity retry returns the cached remote result. This
explicit identity is necessary because neither `gh` nor a GitHub create operation supplies
a transactional boundary shared with khive's graph store.

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

1. **Argument and repo checks.** Required arguments are validated against the normative wire
   contract above, optional arguments are normalized, and `repo` is checked against the
   `[git] publish_repos` daemon config (see "Repo allowlist" below). An unregistered repo fails
   fast, independent of content - there is no reason to scan text for a repository this
   daemon can never publish into.
2. **Publication-hygiene scan.** Every caller-controlled string that can become externally
   visible - `title`, `body`, `notes`, `tag`, `head`, `base`, and every string inside
   `labels`/`assignees` - is scanned by both the token denylist and the secret scan described
   in "Scan module" below, followed by the token allowlist layer. This includes the release
   tag because GitHub exposes it as the release identifier, and includes PR head/base names
   because GitHub exposes them as pull-request identifiers. The scan runs before the
   operation is claimed or `gh` is invoked. It is origin-agnostic: it does not distinguish
   an agent's own prose from relayed or pasted text. There is no trusted-source bypass and
   no `force=true` parameter on any verb.

   The remaining caller-controlled identifiers have narrower validation that prevents this
   content channel: `repo` must be an exact configured allowlist entry; comment `target`
   must match the closed grammar and an existing object in that repo; and
   `idempotency_key` must be a canonical UUID before the handler combines it with the
   daemon-generated reconciliation nonce described below. They are therefore validated, not
   pattern-scanned.
3. **Deny path.** Any hit not covered by an allowlist escape produces a synchronous deny to
   the caller (see "Deny semantics") and an additional typed audit record (see "Audit and
   the event plane"). No GitHub API call is made, and no operation-ledger row is claimed;
   the rejected normalized request therefore has no recovery-ledger representation.
4. **Remote-target read checks.** For `git.publish_comment`, the handler performs the
   repository-scoped, kind-aware read described in "Comment target grammar". For
   `git.publish_release`, it performs a repository-scoped, read-only lookup of the exact
   `refs/tags/<tag>` reference and rejects the request if that tag does not exist. These are
   validation calls, not content writes, and both finish before an operation is claimed.
5. **Claim the durable operation.** For a new operation, the handler obtains a
   cryptographically random reconciliation nonce, generates a standard UUIDv7 for the eventual
   success-domain Event, and atomically inserts the pending-operation row in state
   `unconfirmed_publish`, including the normalized request, nonce, full generated
   reconciliation marker, and `audit_event_id`. This commit happens before `gh` is spawned.
   An existing row reuses its persisted nonce, marker, and audit Event id and follows the
   recovery state machine; only a proven `not_published` row may enter the create path.
6. **GitHub API call.** For a newly claimed or proven `not_published` operation only, the
   verb shells the configured GitHub CLI (`gh`) under the daemon's identity - the same
   transport ADR-088's ingester and Amendment 1's `git.digest` already use, with the fixed
   option/value binding contract above, argv-only construction, and no shell interpolation,
   matching the discipline
   ADR-108 Fork (b) required for `git`. `gh` is reused rather than a direct REST client for
   the same reason ADR-088 §5 gave: it already handles auth and pagination correctly for
   this environment. The operation-bound marker described in "Publish recovery state
   machine" is appended after user content has passed the scan. A release create always
   uses the sole positional exception's fixed argv shape, including the code-selected
   `--verify-tag` flag and mandatory `--` separator, and never includes `--target` or a
   release asset, so a tag removed after the read-only check still causes the create to
   abort rather than create a replacement tag or any other ref.
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
    { "field": "body", "pattern_id": "actor-token-namespace-prefix" }
  ]
}
```

- `hits` lists every field/pattern combination that matched across both the token and
  secret scanners (not just the first), sorted by unsigned UTF-8 byte order on
  `(field, pattern_id)`. A pair appears once even if the same rule matches multiple spans
  or array elements within that field, so a caller can fix everything in one pass instead
  of retrying repeatedly.
- A hit contains only the outward field name and stable pattern id. It contains no matched
  span, excerpt, prefix, suffix, hash, length, or other value derived from the denied field.
  A deny response must not itself become a channel for the content it is denying.
- The batch does not abort on a deny: a failed publish op in a multi-op `request` batch is
  one failed entry among others, per the standing khive batch contract.
- No silent rewrite. The verb never substitutes, truncates, or auto-corrects denied text;
  the caller fixes the text and retries. Silent rewriting was explicitly rejected: it
  teaches the caller nothing and can change the meaning of a message without anyone
  noticing.

### Scan module

Three layers, evaluated in order, all inside the verb handler. The candidate field set is
the normalized outward-facing strings from Handler pipeline step 2, including `tag`,
`head`, and `base`; the deny response and audit identify a hit by that exact field name:

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
2. **Secret scan.** Reuses the existing `secret_gate` module's detector definitions and
   matching semantics (the same ones ADR-088 §5 applies at ingest) against the same
   candidate fields, including `tag`, `head`, and `base`. The module adds a publication
   API with the semantic shape
   `detector_ids(content: &str) -> sorted unique detector ids`. It evaluates every
   canonical detector class against the original content and returns a detector id when
   that class matches any span, including a span that overlaps a match from another
   detector. It never returns a span, masked excerpt, offset, length, candidate fragment,
   or `SecretMatch`; the existing first-match `check` and all-span `mask_secrets` APIs keep
   their current contracts for existing callers. Publication-facing ids are the stable,
   content-free spelling `secret:<secret_gate detector name>`, which cannot collide with
   token-pattern ids because the latter's grammar excludes `:`. A detector name exposed
   through this API is a stable wire identifier: it may not be renamed or reused for a
   different detector semantic. The publish handler must call this all-matches API for
   every candidate field; repeatedly calling the existing first-match `check` API does not
   satisfy this contract. Unlike the ingest path,
   which masks a detected secret and keeps the record, outbound publish **denies** on a
   secret-scan hit and masks nothing silently. The directionality is deliberate: inbound
   content is sanitized and kept because the record has independent value once the secret
   is removed; outbound content that would carry a live secret must never leave in any
   form, masked or not, and the caller must know the check fired so they can rotate or
   remove the credential rather than merely lose it from a git message.
3. **Allowlist escapes.** Certain tokens are legitimate in certain repos - a product name
   that also matches an actor-token pattern, for example. Escapes are declared per
   `(repo, pattern_id)` pair in the same pattern file the token rule lives in (see Pattern File
   Format), never as a per-call parameter. There is no `force=true` escape on any verb; an
   operator who needs an exception edits the applicable generic or private pattern file,
   updates the configured revision, and subjects that change to the installation's config
   review process.

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

| Key             | Contract                                                                                                                                       |
| --------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- |
| `audit_type`    | `publication_hygiene` for a scan deny; `github_publish` otherwise                                                                              |
| `verb`          | The same precise publish verb as the Event's top-level `verb`                                                                                  |
| `repo`          | Canonical, configured `owner/name` slug after grammar and allowlist checks; `null` before both checks succeed                                  |
| `target`        | `issue`, `pr`, or the content-free literal `release`; for a comment, its canonical target only after the kind-aware read succeeds, else `null` |
| `operation_id`  | Canonical idempotency UUID supplied by the call, or `null` if key validation failed                                                            |
| `state`         | Recovery state after this invocation, or `not_claimed` if the operation ledger was not reached                                                 |
| `rule_ids`      | Sorted, unique pattern ids on deny; empty array otherwise                                                                                      |
| `denied_fields` | Sorted, unique outward field names on deny (including `tag`, `head`, or `base` when applicable); empty array otherwise                         |
| `field_count`   | Number of distinct denied fields on deny; zero otherwise                                                                                       |
| `remote_url`    | Published URL on success and whenever already known during recovery; `null` otherwise                                                          |
| `remote_number` | Positive issue/PR number on issue or PR success; `null` for comments, releases, denies, and errors before that identity is known               |
| `remote_id`     | Comment id on comment success; `null` for issues, pull requests, releases, denies, and errors before that identity is known                    |
| `stage`         | `validation`, `scan`, `remote_publish`, `remote_reconcile`, `graph_ingest`, or `audit_append` on denied/error outcomes; `complete` on success  |

For `git.publish_release`, `target` is the literal `release` for every outcome, including
validation failures, scan denials, transport errors, and recovery errors; the tag is never
copied into `target` or another payload key. Before the repository allowlist check or a
comment's kind-aware read succeeds, the corresponding `repo` or comment `target` is `null`.

No title, body, notes, tag, head, base, label, assignee, matched span, excerpt, or value
derived from one is stored in this payload. `rule_ids` and `denied_fields` identify the
rules and field names that caused a hygiene denial without persisting the rejected content.
Normatively, no value from a field that failed validation or caused a hygiene denial may
appear in any audit payload, Event, operation-ledger row, log line, error message, or error
response. The automatic Gate audit row is also content-free: per ADR-018 it records the
verb and decision envelope, not request arguments.

These rows are **additional to**, not replacements for, the dispatch-audit row that
`VerbRegistry` already attempts for every call. The automatic row also has
`EventKind::Audit`, but carries the generic Gate `AuditEvent` payload and the dispatch
outcome. After the Gate allows dispatch, the handler adds one hygiene/publication-domain
row for its invocation, identified by `payload.audit_type`. Thus a normal handler call has
the generic dispatch row plus one extra domain row; a Gate-denied call has only the generic
row because the handler never runs. Retries create their own dispatch history. Event reads
use `list(kind="event", verb="git.publish_issue", ...)` (or another precise verb) and
inspect `payload.audit_type`; ADR-022 still excludes `query()` / GQL / SPARQL for events.

The success-domain row uses the standard UUIDv7 generated once when the operation is first
claimed and persisted in the ledger's `audit_event_id` field. It is neither caller-selected
nor derived from the operation UUID. Recovery reuses that persisted id for the idempotent
append; a duplicate-key result for the same operation and payload is treated as already
recorded. The Event's `created_at` is assigned normally when it is appended, preserving
ADR-004's `(created_at, event_id)` replay order. The operation cannot advance from
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
matching ADR-088's own usage.

For the GitHub-specific identity path defined below, publish self-ingest and `git.digest`
use one shared project mapping. `GitHubRepoIdentity` is the validated lowercase
`owner/name` slug, and its only stored URL is
`https://github.com/<owner>/<name>`. An accepted remote `git.digest` source using
`www.github.com`, an optional trailing slash or `.git` suffix, or ASCII case differences
in the host or owner/name components is an input alias for that identity; it is never a
distinct `properties.repo_url`. A URL with user information, a port, query, fragment,
percent-encoded path separator, or any path component beyond the owner and repository is
not a GitHub identity alias. After alias removal and ASCII lowercasing, the slug must
satisfy this ADR's canonical `repo` grammar.

`git.digest` and publish self-ingest must call the same project-identity resolver in the
handler namespace on the GitHub-specific identity path. For `git.digest`, that path applies
only when `source` is an accepted remote GitHub URL; the identity comes from its accepted URL
alias. Publish self-ingest gets the same identity from its canonical `repo` slug. The resolver
considers only live `project`
entities whose string `properties.repo_url` parses to the same `GitHubRepoIdentity`; it
never matches `project.name`, a path basename, or an unscoped repository name. If no anchor
matches, it creates one with the canonical URL. If exactly one alias-form anchor matches,
it rewrites that anchor's `properties.repo_url` to the canonical URL before ingest. If
more than one live anchor maps to the identity, resolution fails as an integrity error
before any note write; an operator must explicitly merge the duplicate projects and retry,
rather than the resolver selecting an arbitrary row. This enumeration-and-rewrite is the
required alias migration for existing data and runs before either digest or publish can
ingest a GitHub object. A caller-supplied `git.digest project` id is accepted for a GitHub
source only when that project's `properties.repo_url` parses to the same identity; it is
otherwise rejected. Outside this GitHub-specific identity path - including every local path,
whether or not it has a GitHub origin, and every non-GitHub URL - `git.digest` retains
ADR-088 Amendment 1's accepted resolution contract unchanged: an omitted `project` matches
either the exact canonical path/URL in `properties.repo_url` or the name derived from the
source basename, and creates an anchor only when neither matches. This ADR neither reselects
nor migrates those anchors; their project ids, cursors, and issue/PR natural keys remain
unchanged. Such identities cannot be selected by a publish slug.

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
  `properties.publish_operation_id` set to the operation UUID, and
  `properties.publish_verb` set to the exact publish verb.
- A release reconciliation is an upsert with exactly one live `reference` note under
  `(kind=reference, namespace, properties.publish_verb,
  properties.publish_operation_id)`. It creates or updates that note and ensures exactly
  one `annotates` edge from the note to the resolved repo-anchor `project`. Replaying the
  initial self-ingest or recovering after either the note upsert or edge ensure must leave
  one note and one such edge; a release reference without that edge is not ingested and
  cannot advance to `ingested_pending_audit`.
- A comment uses the same reference-note upsert key. A comment targeting an already-ingested
  issue or pull request `annotates` that note; if the target was never ingested, it
  `annotates` the repo-anchor `project` entity instead. This mirrors ADR-088 Amendment 1's
  best-effort enrichment precedent: no match means a narrower edge, never a second remote
  publish.

This graph reconciliation runs synchronously after a successful GitHub response and is
resumed from durable state after a failure; it is not deferred to the next digest sweep or
to a background job. The required regression is: publish an issue or PR, run `git.digest`
for the same repository until `done`, and assert exactly one note with that natural key and
exactly one `annotates` edge from it to the repo project.

### Publish recovery state machine

GitHub and the graph store cannot share a transaction. The git pack therefore owns a
durable `git_publish_operation` ledger. Its minimum persisted fields are `operation_id`
(the idempotency UUID), `namespace`, `verb`, `repo`, a canonical request hash, the normalized
request needed for local replay, `reconciliation_nonce`, `marker`, `state`, `remote_url`,
`remote_number`, `remote_id`, `note_id`, `audit_event_id`, `last_error`, `created_at`, and
`updated_at`. The composite `(namespace, verb, operation_id)` is the ledger's primary key
and unique operation identity; no global uniqueness constraint on `operation_id` may collapse
two supported dispatch scopes. The stored request has already passed the hygiene and secret
scans, is local daemon state, and must never be copied into an Event payload or error
response.
Because the scan and nonce generation precede the ledger claim, a validation failure,
hygiene denial, or secure-random-source failure creates no ledger row. `last_error` is a
code-selected, content-free error class and stage; it never contains transport stdout or
stderr, a command or argv rendering, or any caller-supplied field value. The same restriction
applies to tracing and all other log output.

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
  with the same operation identity may move back to `unconfirmed_publish` and make the first
  remote attempt. No child exit status or output-parse failure is strong enough for this
  state.
- **`published_pending_ingest`** requires a durably stored remote URL and the applicable
  remote number or id. It means GitHub accepted the object but graph reconciliation is not
  yet complete. Retries perform only the local upsert and edge reconciliation.
- **`ingested_pending_audit`** means the graph note and edge are reconciled but the
  operation-level success audit has not yet been confirmed durable. Retries perform only
  the idempotent audit append.
- **`complete`** requires remote identity, graph reconciliation, and the success-domain
  audit. Retries return the stored result without network or graph mutation.

For remote reconciliation, every create appends this inert, operation-bound marker after the
scanned user content:

```html
<!-- khive-publish:<operation_id>:<reconciliation_nonce> -->
```

`reconciliation_nonce` is exactly 32 bytes obtained from the operating system's
cryptographically secure random source and encoded as 64 lowercase hexadecimal characters.
It is generated by the daemon after the caller content passes the scan; it is never accepted
from the caller or derived from the operation UUID, request bytes, time, process state, or
another predictable value. The new-operation claim atomically persists the nonce and full
marker with `unconfirmed_publish` before the child is spawned. Failure to obtain secure
randomness fails the invocation before a ledger row or remote write and is audited as the
content-free `remote_publish` error stage. The nonce and marker are immutable for the
operation, including a `not_published` retry.

The persisted nonce and full marker are local recovery credentials until the marker becomes
visible on a successfully created remote object. Neither appears in receipts, error
responses, audit payloads, graph properties, or logs. The marker is applied uniformly to
issue and PR bodies, comment bodies, and release notes. Graph self-ingest strips exactly the
persisted generated trailing marker from `content`; it does not remove arbitrary HTML
comments supplied by the caller. Caller content may therefore contain operation UUIDs,
legacy operation-id-only marker text, or marker-shaped HTML comments with caller-chosen
nonce text without being rewritten. Such text is not a recovery match for a pending
operation because it does not contain that operation's unpredictable persisted nonce.

On a retry from `unconfirmed_publish`, the handler performs an authoritative, read-only,
repo- and object-kind-scoped enumeration for the exact full persisted marker, including both
the operation UUID and nonce. A GitHub search endpoint, search index, cached digest result,
or first-page-only lookup is not authoritative for this purpose. The per-kind collection is
fixed:

- issues: the repository's `issues` connection, with `states: [OPEN, CLOSED]`,
  `orderBy: {field: CREATED_AT, direction: ASC}`, and the issue `id`, `url`, `number`, and
  `body`;
- pull requests: the repository's `pullRequests` connection, with
  `states: [OPEN, CLOSED, MERGED]`, `orderBy: {field: CREATED_AT, direction: ASC}`, and the
  pull request `id`, `url`, `number`, and `body`;
- comments: the already-validated target returned by the repository's
  `issueOrPullRequest(number:)` field, followed by the concrete `Issue.comments` or
  `PullRequest.comments` connection with
  `orderBy: {field: UPDATED_AT, direction: ASC}`, and the comment `id`, `url`, and `body`;
- releases: the repository's `releases` connection, with
  `orderBy: {field: CREATED_AT, direction: ASC}`, and the release `id`, `url`, and
  `description`.

These are fixed GraphQL object connections, not the GraphQL `search` connection. The handler
invokes them through the code-selected literal `gh api graphql` command shape. The literal
`graphql` endpoint operand contains no caller data and does not add a caller-controlled
positional exception. Canonical owner/name components and subsequent cursors are values of
code-selected `--raw-field` options, and the validated decimal comment target number is the
value of a code-selected `--field` option; every GraphQL variable name is a code constant.
The query document and selected response fields are likewise code constants.

Every connection requests `first: 100` and `pageInfo { hasNextPage endCursor }`. The handler
follows `endCursor` while `hasNextPage` is true, rejects a true `hasNextPage` with a missing
or repeated cursor, deduplicates nodes by immutable GraphQL `id`, and sorts any matching
identities by that id before deciding the result. A no-match conclusion is valid only after
`hasNextPage` is false. Each recovery invocation is bounded by both 1,000 pages and 120
seconds. The traversal returns a content-free unresolved reconciliation error if the
120-second deadline is reached, another page remains after page 1,000, cursor progress is
malformed, or any page read fails. The operation remains `unconfirmed_publish`; the bounded
failure must not be treated as no match and must never enable another create.

This rule applies identically to issue, pull-request, comment, and release reconciliation;
an operation-id-only or nonce-mismatched marker is never accepted on any path. One match
supplies the remote identity and advances to `published_pending_ingest`; multiple matches
are an integrity error; no match after the complete traversal leaves the operation
unconfirmed and returns an error carrying `operation_id` and `state`, but no publish
content. It never calls a GitHub create command. An operator may resolve a persistently
unconfirmed operation only after independently establishing whether the remote object
exists; silently changing the idempotency key is not a recovery action.

The crash and failure windows are therefore explicit:

| Window                                                   | Durable state              | Retry behavior                                                                    |
| -------------------------------------------------------- | -------------------------- | --------------------------------------------------------------------------------- |
| Before the operation insert commits                      | No operation               | The request may claim its key; no remote write has occurred                       |
| Spawn fails before a child exists                        | `not_published`            | A same-identity retry may safely attempt the first create                         |
| After spawn, during `gh`, before receipt commit          | `unconfirmed_publish`      | Read-only marker reconciliation; never create                                     |
| After receipt commit, before/during note or edge write   | `published_pending_ingest` | Upsert note and ensure edge; never create                                         |
| After graph reconciliation, before/during audit append   | `ingested_pending_audit`   | Append idempotently with the persisted UUIDv7 Event id; never create or re-ingest |
| After audit append, before the ledger reaches `complete` | `ingested_pending_audit`   | Duplicate Event id proves audit durability, then mark complete                    |

The ledger update that records remote identity is committed before the first graph write.
The graph upsert and edge ensure are independently idempotent because a crash can occur
between them. The persisted claim-time UUIDv7 success Event id closes the final
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
retry with the same operation identity; recovery follows the read-only marker path above.
This asymmetry is deliberate: skipped ingest work is recoverable on the next digest pass,
whereas a retried create could duplicate public content.

## Pattern File Format (normative)

The token-denylist (scan layer 1) and allowlist-escape (scan layer 3) patterns are defined
in TOML files, loaded by both the server-side verb handler and the client-side pre-tool-use
hook described in the Context section. Both implementations must reach the same allow/deny
decision on the same content. The authoritative paths, overlay selection, content revision,
reload behavior, and ASCII byte-pattern grammar below are the executable contract for that
property. The shared corpus is regression coverage for this contract, not its definition.

This normative contract covers only scan layers 1 and 3. Scan layer 2 (the secret scan)
reuses the existing, already-deployed `secret_gate` module and its own pattern set; it is
Rust-only today and unaffected by this ADR. A hook implementation that wants secret-scan
parity with the server maintains its own secret-detection mechanism (for example, a
gitleaks-style scanner with a versioned allowlist) rather than consuming this file format
for that layer. Convergence of the secret-scan layer onto a single shared representation is
not required by this ADR.

### Two files, one merged pattern set

1. **In-repo generic pattern file** - exactly
   `crates/khive-pack-git/patterns/publication-hygiene.toml`, versioned in the khive
   repository and public-visible. This path is the sole source of generic pattern bytes.
   The Rust pack may embed those bytes at build time, and hook packaging may copy them, but
   generated or installed copies must be byte-identical to this file. It contains only
   generic pattern _classes_: a pattern that matches the _shape_ of an actor-namespace
   token, an internal-path prefix, or org-mechanics phrasing, never a concrete internal
   identifier, alias, or literal internal term. If a pattern would only make sense with a
   concrete literal internal term hardcoded into it, that pattern does not belong in this
   file - it belongs in the overlay.
2. **Local overlay file** - not versioned in the repository. Its only selection mechanism
   is the optional absolute path at `[git] publication_hygiene_overlay` in the resolved
   khive config file. There is no second environment variable or alternate config key for
   the overlay. The server uses its normal `--config` / `KHIVE_CONFIG` resolution; a paired
   hook must receive that same resolved absolute config path through `KHIVE_CONFIG` and
   must not perform an independent current-directory search. The overlay contains concrete
   internal tokens: internal identifiers, aliases, and literal internal-process phrasing.
   It is private per installation, never committed, and never published.

`[git] publication_hygiene_revision` is required whenever `publish_repos` is non-empty. It
is the lowercase 64-hex-character BLAKE3 digest of this unambiguous byte sequence:

```text
"khive-publication-hygiene-v1\0"
|| u64be(generic_byte_length) || generic_bytes
|| u64be(overlay_byte_length) || overlay_bytes
```

For an absent overlay key, `overlay_byte_length` is zero and `overlay_bytes` is empty. Both
consumers compare their computed digest with the configured revision and fail closed on a
mismatch. Thus an old embedded generic file, a stale installed hook copy, or a different
overlay cannot silently produce a second effective rule set.

### Merge semantics

Before each scan, both consumers validate the configured revision and merge the two files
into one pattern set. The generic bytes may remain embedded or cached, but the overlay is
read and the combined revision is recomputed for every scan; v0 does not cache overlay
contents across scans. This makes an in-place overlay change fail closed until the
configured revision is updated, and makes the new revision effective without one consumer
continuing to use stale overlay bytes.

- The in-repo file loads first, the overlay file loads second and its patterns are
  appended.
- Every pattern's `id` field must be unique across the merged set. If the overlay defines
  an `id` that already exists in the in-repo file, that scan fails closed - an
  overlay is additive only; it cannot redefine or silently shadow a pattern the in-repo
  file ships. This prevents a misconfigured local overlay from quietly weakening the
  generic pattern set.
- An absent overlay config key selects the generic file alone. If the key is present, its
  value must be an absolute UTF-8 path. A relative path, missing or unreadable file, malformed
  TOML, schema violation, or revision mismatch fails closed before any external write. The
  server returns a configuration error and the hook denies the raw content-write command;
  neither continues with a partially loaded pattern set.

### Pattern entry schema

```toml
[[pattern]]
id = "actor-token-namespace-prefix"
category = "actor_token"
regex = '\bnamespace:[a-z0-9_-]+\b'
case = "ascii_insensitive"
description = "actor-namespace-style token"
severity = "deny"

[[pattern]]
id = "internal-path-worktree"
category = "internal_path"
regex = '/[A-Za-z0-9_./-]+/agent-worktrees/'
case = "sensitive"
description = "local worktree absolute path"
severity = "deny"
```

Fields:

- `id` (required, string, unique across the merged set) - a content-free stable identifier
  matching `[a-z][a-z0-9-]{0,63}`. Because it is exposed in deny responses
  (`hits[].pattern_id`) and deny-audit `payload.rule_ids`, it must describe a generic rule
  class and must not contain a matched literal, credential fragment, or deployment-specific
  term; either loader rejects a nonconforming id.
- `category` (required, one of `actor_token` | `org_mechanics` | `internal_path` |
  `commercial_strategy`) - the pattern class. `secret` is deliberately not a valid value
  here; secret patterns live in the separate `secret_gate` module (see above).
- `regex` (required, string) - the match pattern. See "ASCII byte-pattern grammar and
  portability" below for the closed syntax and matching semantics.
- `case` (required, exactly `"sensitive"` or `"ascii_insensitive"`) - selects the ASCII
  comparison rule below. There is no inline flag syntax.
- `description` (required, string) - human-readable configuration-review context. It is
  never copied into an audit payload, log, or deny response; those expose only the
  `pattern_id`.
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

### ASCII byte-pattern grammar and portability

The `regex` field is not an arbitrary host-language regular expression. It is an ASCII-only
byte-pattern language with this grammar; both consumers parse this language before compiling
or evaluating it:

```ebnf
pattern      = [ "^" ], alternation, [ "$" ] ;
alternation  = sequence, { "|", sequence } ;
sequence     = piece, { piece } ;
piece        = atom, [ quantifier ] ;
atom         = literal | class | "(" , alternation , ")" | "\\b" ;
class        = "[", class_item, { class_item }, "]" ;
class_item   = class_literal | class_literal, "-", class_literal ;
quantifier   = "?" | "*" | "+" | "{", count, [ ",", [ count ] ], "}" ;
count        = "0" | nonzero_digit, { digit } ;
digit        = "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" ;
nonzero_digit = "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" ;
```

The lexical and semantic rules are closed:

- Pattern source must be 1-4,096 bytes long and contain only printable ASCII bytes `0x20`
  through `0x7e`. A literal is any such byte except `\`,
  `.`, `^`, `$`, `|`, `(`, `)`, `[`, `]`, `{`, `}`, `*`, `+`, or `?`. A
  metacharacter is literal only when escaped with `\`; `\b` alone denotes the boundary
  atom. A bare `.` wildcard and every other escape are rejected.
- A class is positive, ASCII-only, and non-empty. `]` and `\` must be escaped; `-` is
  literal only when escaped or first or last, and `^` as the first class byte is rejected.
  Inside a class, the only accepted escapes are `\]`, `\\`, and `\-`; every other
  printable ASCII byte is a class literal. Range endpoints must be ASCII bytes in ascending
  order. Negated classes, POSIX classes, Unicode properties, and shorthand classes such as
  `\d`, `\s`, and `\w` are rejected.
- Alternation branches, sequences, and groups are non-empty. `^` and `$` are permitted
  only once in the outer positions shown. Each atom has at most one quantifier. Counts are
  canonical decimal integers from 0 through 65,535; for `{m,n}`, `m` must not exceed `n`.
- The candidate is matched as its exact UTF-8 byte sequence, with no Unicode normalization
  and with multiline and dot-all modes disabled. `^` means before byte zero and `$` means
  after the final byte, including when the final byte is a line feed.
- ASCII word bytes are exactly `[A-Za-z0-9_]`. `\b` matches only at the start or end of the
  candidate, or between an ASCII word byte and any other byte. Every byte of a non-ASCII
  UTF-8 encoding is a non-word byte.
- With `case = "sensitive"`, byte comparison is exact. With
  `case = "ascii_insensitive"`, and only then, ASCII `A` through `Z` compare equal to the
  corresponding `a` through `z`; all other bytes compare exactly. No Unicode case folding
  is performed.

Each loader must consume the entire pattern into this grammar and reject any other syntax
before scan service becomes available. A host engine accepting a pattern does not make it
valid. Both the server publish path and the paired hook **must** evaluate the parsed grammar
with a bounded, worst-case linear-time byte matcher, such as a Thompson-automaton or
RE2-style engine, with no backtracking execution path. Translation to a host regex engine is
permitted only when that engine guarantees linear-time evaluation for every accepted
pattern while preserving these exact anchor, boundary, byte-mode, and ASCII-case rules; a
backtracking engine is prohibited even when a particular pattern appears benign. Matcher
work must be bounded linearly in candidate byte length for the already bounded pattern
source, and exhaustion of an implementation resource bound fails closed rather than falling
back to a backtracking matcher. This grammar and byte semantics, rather than the behavior of
either host regex engine or a finite fixture set, define cross-client equivalence.

### Match semantics

- Each pattern is applied independently to each candidate string field of a publish
  request (`title`, `body`, `notes`, `tag`, `head`, `base`, and each string within
  `labels`/`assignees`) - never to a concatenation of fields, so that a reported hit's
  `field` value is accurate. The candidate set is fixed by Handler pipeline step 2; neither
  scanner may silently omit an identifier field.
- A pattern matching anywhere within a field's byte sequence is a hit for that field. An
  implementation is not required to find every possible match within a single field for a
  single pattern - one is sufficient to deny. It reports each matching
  `(field, pattern_id)` pair once and reports no match span or derived content.
- A repo-specific `[[allow]]` entry is applied only after the implementation has collected
  the token-pattern hit. The shared corpus therefore observes the same hit before the same
  `(repo, pattern_id)` suppression in both implementations.

### Shared conformance corpus

The authoritative cross-implementation corpus is exactly
`crates/khive-pack-git/tests/fixtures/publication-hygiene-conformance.toml`. A companion
test overlay, when needed by a case, is
`crates/khive-pack-git/tests/fixtures/publication-hygiene-overlay.toml`. These are generic
test fixtures and contain no deployment-specific terms.

The corpus has two closed case tables:

```toml
[[scan_case]]
id = "word-boundary-deny"
repo = "org/repo"
field = "body"
text = "generic fixture text"
expected_rule_ids = ["fixture-word-boundary"]

[[load_case]]
id = "reject-lookbehind"
regex = '(?<=x)y'
case = "sensitive"
expected = "reject"
```

For `scan_case`, `id`, `repo`, `field`, `text`, and the sorted unique
`expected_rule_ids` are required; an empty expected list means allow. For `load_case`,
`id`, `regex`, `case`, and `expected = "accept" | "reject"` are required. Unknown keys or
duplicate case ids fail the corpus loader. Both implementations execute every case and must
produce the exact expected result; neither maintains a private copy or language-specific
expected file. Passing these cases detects regressions but cannot admit syntax or define
semantics beyond the normative grammar above.

The committed corpus must cover at least: allow and deny for every pattern category;
matches and near-misses for strict anchors, ASCII `\b`, alternation, quantifiers, positive
character classes, and both case modes; non-ASCII adjacent text that exercises the specified
byte boundary and absence of Unicode case folding; rejection of non-ASCII pattern bytes,
wildcards, negated classes, shorthand or Unicode classes, inline flags, lookaround,
backreferences, possessive quantifiers, and atomic groups; multiple rule hits; all outward
field names including `tag`, `head`, and `base`; a repo-specific allowlist hit that is
allowed in exactly one repo and denied in another; duplicate ids across generic and overlay
files; an absent overlay; and malformed or revision-mismatched overlays. Passing this corpus
in both the Rust handler suite and the hook suite is a regression gate for any pattern-format,
loader, or matcher change; it is not the definition of cross-client equivalence.

The shared corpus must also include an ambiguous-repetition near-miss evaluated against a
maximum-length 65,536-scalar candidate. Both consumers must prove the linear bound with an
instrumented matcher-step or automaton-transition budget proportional to candidate byte
length; a wall-clock-only assertion is insufficient. The case must complete with the same
no-match result in both suites and must not permit a backtracking fallback.

## Implementation requirements and verification

The implementation ships the verbs, operation ledger, pattern loader, scan, audit rows,
graph reconciliation, and tests as one change. In addition to unit coverage for the
pattern file, it must include these contract tests:

1. **Required-argument and argv safety.** Table-driven tests cover omission, JSON `null`,
   wrong JSON types, the specified empty-string behavior, every accepted control-character
   exception, every rejected control-character class, and each required text field's
   minimum, maximum, and maximum-plus-one boundary. Repository component lengths, UUID shape
   and nil rejection, the target's `u64` boundary, and whole-ref length receive their own
   boundary cases. Grammar tests also include malformed values, fully qualified refs, a
   fork-qualified `owner:branch` head, and option-looking identifiers beginning with `-`.
   Free-text `title`, `body`, and `notes` values and an issue label beginning with `--` are
   accepted when otherwise valid and arrive byte-for-byte in fixed value slots; a transport
   spy asserts that they add no option, change no subcommand, and cannot consume a following
   fixed option. For each externally visible Unicode-capable field - issue title, PR title,
   each of the three body fields, release notes, an explicit release title, and an issue
   label element - the table inserts a `Cf` scalar such as U+200B into (a) an otherwise
   denied token fixture and (b) an otherwise credential-shaped secret fixture. Every case
   is rejected during argument validation before a ledger claim or transport call, and the
   serialized Event, response, error, and captured logs contain neither source fixture.
   Rejected cases reach neither the operation ledger nor transport.
2. **Typed audit surface.** Allow, hygiene-deny, transport-error, and recovery-error cases
   append additional rows with `EventKind::Audit`, the precise top-level verb, the correct
   `EventOutcome::{Success, Denied, Error}`, and every required JSON payload key. Tests
   assert that no new `EventKind` spelling is introduced and that deny payloads contain
   rule ids and field names but no rejected or derived content. Validation and deny tests
   inspect serialized Events, captured logs, error messages, responses, and the operation
   ledger for absence of every rejected field value. They also distinguish the
   handler-owned row from the automatic generic dispatch row by `payload.audit_type`.
3. **Comment target validation.** Table-driven handler tests accept `issue#42` and
   `pr#978`; reject zero, leading zero, case changes, signs, whitespace, overflow, bare
   numbers, URLs, and embedded repositories; and reject a repository-scoped read whose
   remote kind disagrees with the parsed kind. A transport spy proves rejection occurs
   before any comment write.
4. **Canonical project identity and digest-compatible idempotency.** Digest an accepted
   GitHub URL alias such as `https://www.github.com/Org/Repo.git`, publish an issue and a PR
   through canonical slug `org/repo`, then run `git.digest` again through the canonical URL
   until complete. Assert one project whose `properties.repo_url` is
   `https://github.com/org/repo`; for each remote number, assert one note under
   `(kind, namespace, number, project_id)`, the full common property shape, and one project
   `annotates` edge. Repeat self-ingest and both digest URL forms to prove the project,
   natural-key, and edge counts remain one. Separate cases pre-seed one alias-form project
   and prove it is canonicalized in place, pre-seed two aliases and prove resolution fails
   without choosing either, prove an unrelated same-basename project is ignored, and prove
   a mismatched explicit `git.digest project` id is rejected. An initial release publish
   also asserts exactly one reference note under its verb-qualified operation-identity upsert
   key and exactly one `annotates` edge to this canonical project.
5. **Recovery failure injection.** Inject a crash or store error after each boundary in
   the recovery table: operation insert, remote response, note upsert, edge ensure, audit
   append, and completion update. Resume with the same operation identity and assert that the
   remote create spy observed exactly one create, unfinished local work completed, and the
   final success Event exists exactly once. Assert that `audit_event_id` is a UUIDv7 in the
   initial claim and that every recovery attempt and the final Event reuse that exact id. The
   unconfirmed case must exercise exact
   nonce-bearing marker recovery and the no-match error path. A parameterized forgery
   regression covers issue, pull-request, comment, and release reconciliation: operation A
   publishes a same-repo, same-kind object whose caller content contains operation B's UUID
   in both legacy operation-id-only marker text and a full marker-shaped comment with a
   nonmatching nonce; A still receives its own generated trailing marker. B's child then
   starts, its response is lost without creating B's remote object, and B remains
   `unconfirmed_publish`. A same-identity retry of B must not bind A's remote identity, must
   find no exact full-marker match, must issue no second create, and must return the
   unresolved error. For each of issue, pull-request, comment, and release recovery, place
   the exact marker on an object beyond the first 100-item page and prove that the
   authoritative per-kind traversal follows pagination, finds it, and issues no second
   create. Bound, malformed-pagination, and mid-pagination failure cases must leave the
   operation `unconfirmed_publish` rather than reporting no match. The recovery transport
   spy also proves that `graphql` is the only endpoint operand and that owner, name, target
   number, and cursor values cannot add or alter positional operands. Release cases also
   inject failures immediately after the reference-note upsert and immediately after its
   project-edge ensure; each recovery must finish with exactly one reference note and
   exactly one `annotates` edge to the resolved repo-anchor project.
6. **Idempotency scope and request conflict.** Reusing a key with identical normalized
   arguments returns or resumes the original operation within the same namespace and verb;
   reusing it with different arguments in that same scope fails before transport. Reuse the
   same UUID in two explicit namespaces and prove that two ledger identities and
   namespace-specific graph records are created and neither invocation returns the other's
   cached receipt. Reuse the same UUID across two publish verbs and prove that their ledger
   identities remain distinct; a comment/release pair must also produce distinct reference
   notes under `properties.publish_verb` rather than collide on the UUID.
7. **External identifier hygiene.** Table-driven fake-transport tests put a token-denylist
   match and a `secret_gate` match in each of `tag`, `head`, and `base`. Every case returns
   a hygiene denial before an operation-ledger claim or any `gh` invocation, the transport
   spy observes zero GitHub writes, and the deny response plus audit identify the exact
   field. The release cases must include both a denied generic-pattern tag and a
   credential-shaped secret-pattern tag, proving that a tag cannot reach reference lookup or
   release creation without passing both scan layers. For each release-tag denial, the
   stored Event has `target = "release"`;
   a serialized-Event assertion proves the raw tag is absent from the complete payload, and
   the operation ledger remains empty for that idempotency key. Secret-scan regressions put
   at least two detector classes in one field and distribute multiple detector classes
   across multiple fields. They assert the exact sorted set of `secret:<detector>` ids and
   `(field, pattern_id)` pairs, including overlapping detector-class matches, and assert
   that the response, audit Event, error, and logs contain no source text, masked excerpt,
   span, offset, or length.
8. **Release-tag boundary.** A missing-tag case is rejected by the read-only preflight before
   an operation-ledger claim or remote write. An existing-tag case reaches release creation,
   and the transport spy proves the fixed argv includes `--verify-tag`, excludes `--target`
   and release assets, contains the mandatory `--` separator immediately before the sole
   validated-tag positional, and performs no tag/ref write. A tag beginning `-` or containing
   whitespace or a shell/option metacharacter is rejected before argv construction and cannot
   alter option parsing. A race case removes the tag after preflight and proves the release
   create fails without recreating it.
9. **Optional-argument normalization.** For labels and assignees, compare omission with
   `[]`, compare at least two permutations of the same non-empty array, and retry each form
   with one idempotency key. For draft, compare omission with `false`. For release title,
   compare omission, `""`, and the normalized tag. Each equivalence case must produce the
   same canonical JSON and request hash and must resume or return the same operation on a
   same-identity retry; each genuinely different normalized value must conflict on that
   identity.
   Limit, element-shape, duplicate, and JSON-`null` failures occur before transport.
10. **Hook/server scan conformance.** The Rust handler suite and the hook implementation's
    suite both execute every case in
    `crates/khive-pack-git/tests/fixtures/publication-hygiene-conformance.toml` against the
    authoritative generic file and test overlay. CI fails on a skipped case, divergent
    allow/deny result, field or rule-id mismatch, byte-grammar loader mismatch, allowlist
    mismatch, or revision mismatch. Separate loader tests prove that both implementations
    reject every source outside the closed grammar even when their host regex engine would
    accept it. Both suites also run the shared ambiguous-repetition near-miss against a
    65,536-scalar field under the deterministic linear matcher-work bound; neither may invoke
    or fall back to a backtracking engine.

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
- **Tag/ref creation.** `git.publish_release` requires a pre-existing remote tag and creates
  only the release. Automatic tag creation, if ever added, is a separate explicit Git-write
  capability requiring an ADR-108 amendment that defines the target ref and commit,
  authorization, audit, and recovery contracts; it is outside this ADR and v0.
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
   to the verb surface only after the Rust handler and hook both pass the shared conformance
   corpus, implement the normative ASCII byte-pattern grammar, consume the authoritative
   generic file, resolve the same absolute config path, and validate the same configured
   pattern-set revision. The hook then narrows its role to denying raw `gh` content writes
   outright and pointing the caller at the equivalent verb; it no longer makes an
   independent content allow/deny judgment once the verb enforces the scan server-side.
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
   ship at the single authoritative in-repo path, versioned and public-visible; the
   concrete internal-token list is selected only by the resolved config's absolute overlay
   path. The ASCII byte-pattern grammar defines common scanner semantics, the merged-content
   revision prevents file-version drift, and the shared hook/server corpus detects
   regressions in both implementations. The in-repo file must never contain concrete
   internal-identifier tokens.
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
- A daemon-generated nonce binds reconciliation to the operation's actual remote object
  without reserving or rewriting legitimate caller-authored HTML comments.
- Pattern data is versioned and reviewable as an ordinary file diff, rather than scattered
  across independently maintained hook and verb implementations. The byte-pattern grammar
  defines common semantics; the shared corpus and configured merged-content revision make
  implementation regressions or file-version drift a failing check instead of a silent
  behavior change.

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
- The overlay-file split (F2) remains an operational dependency. Updating generic or
  private patterns now also requires updating the configured merged-content revision and
  keeping the paired hook on the same resolved config path. A bad rollout fails closed and
  can temporarily block publication until both consumers receive the matching bytes and
  revision.
- Pattern matching has an inherent false-negative and false-positive profile. It is not
  semantic, so a paraphrased violation is not necessarily caught, and a generic pattern can
  over-match a legitimate use (mitigated, not eliminated, by the allowlist-escape
  mechanism). This ADR requires evidence-driven tuning as an ongoing discipline; it does
  not claim to close the gap once.

## Alternatives Considered

| Alternative                                                      | Why not adopted                                                                                                                                                                                                                                                                                              |
| ---------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Enforce hygiene only at the client-side hook, no server verb     | Does not compose with the eventual "raw `gh` denied for content writes" endpoint: any caller not running a current hook has no enforcement at all. A server-side verb is the one chokepoint every caller must pass through regardless of local hook state.                                                   |
| Semantic or LLM-based content classification instead of patterns | Rejected for v0. Patterns are deterministic, auditable as a file diff, and portable across two independent implementations by construction; a classifier's decisions are neither deterministic nor reviewable in the same way, and out-of-scope per this ADR's scope decision.                               |
| A `force=true` per-call escape parameter                         | Rejected. Defeats the review property the allowlist file provides: a legitimate exception must be a versioned, reviewable config edit, never a call-site flag any caller can set for itself.                                                                                                                 |
| A single merged pattern file, no in-repo/overlay split           | Rejected. The in-repo file must never carry concrete internal-identifier tokens (F2); a single file would either leak internal terms into a public repository or force every installation to maintain a full private copy instead of layering a small overlay onto a shared generic base.                    |
| Fold outbound publish into `git.digest` with a write mode        | Rejected, matching ADR-108's identical rejection of overloading `git.digest`: it is read/ingest-shaped, and conflating it with a write-and-scan operation mixes two operations with entirely different audit and failure-mode needs.                                                                         |
| Derive the recovery marker from `operation_id` alone             | Rejected. The id is caller supplied and predictable before the operation is claimed, so another publication could pre-plant that marker and be mistaken for the pending operation. A persisted daemon-generated nonce preserves caller content while making a pending operation's full marker unpredictable. |

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
- `crates/khive-runtime/src/secret_gate.rs` - existing secret detector definitions and
  matching semantics, extended with the content-free all-detector-id API required by scan
  layer 2; existing first-match and masking callers retain their contracts.
- `crates/khive-pack-git/src/ingest.rs` - the digest natural key, issue/PR property shapes,
  and existing `gh`/`git` shell-out precedent this ADR follows.

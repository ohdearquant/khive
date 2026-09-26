# `git.digest` handler design notes

Extracted from `crates/khive-pack-git/src/handlers.rs` doc-comments.

For the separate read of persisted ingest progress, see
[`git.ingest_cursor`](ingest_cursor.md). It does not run this digest handler.

## Argument boundary

Every registered `git.*` verb accepts a named JSON object. Each handler
deserializes it through that verb's closed argument struct at its existing
validation seam. An unknown field returns its name and the accepted names
before repository access, process execution or tool-policy calls. Local and
remote operation refusals still persist exactly one `not_committed` receipt
with reason `invalid_params`; legacy commit preserves its denial audit, and
the existing local/remote supplementary audits remain in place. Receipt
storage failures keep their existing uncertainty behavior. Read and ingest
validation has no operation receipt. Dispatcher audit is independent of
these handler records. Unknown values are not copied to safe receipt inputs
or echoed in the error, and never become Git flags or executable selectors.

The boundary preserves each known field's JSON value and whether it was
supplied. Existing handlers still decide value types, ranges, policy and
cross-field rules: explicit null is not rewritten as omission, `git.commit`
still selects its tree form by the presence of `tree`, and digest's signed
`max_items` keeps its default and clamp behavior. This does not strengthen
or weaken the existing argv, allowlist, credential, or ref-comparison rules.

## `RemoteRecoveryStage` / `RemoteCommitRecovery`

Issue #765 remote-only repair policy: at most one `git fetch --refetch`,
then at most one owned-cache reclone, bounded by `stage` so a persistent or
recurring classified failure surfaces as a terminal error rather than
looping. Local-path sources never construct this — they call public
`run_ingest` directly, which never repairs anything (ADR-088 Amendment 1:
the disposable scratch cache is remote-URL-mode-only).

## `repair`

Advances the bounded repair state machine by one step in response to a
classified `GitLogError` (the caller has already verified
`is_missing_promisor_object()`). Ignores `_repo` — both repair primitives
operate on the cache slot for `canonical_url`, which is the same path
`_repo` already names (`crate::cache`'s slot layout is keyed by URL, not
passed through).

## `handle_digest`

The handler preserves the caller's `include` bits when it enters
`run_ingest`; it never masks issues/pull requests merely because the parsed
source URL is not on GitHub. A remote source acquires a scratch clone only
when commits are requested; issues/pull-requests-only passes supply the
canonical source's GitHub slug directly and execute source-bound `gh` calls
from a neutral working directory. If that remote URL has no GitHub slug, the
core does not derive an unrelated `origin` from the neutral directory and
records requested remote sources as `Skipped`. Local-path and administrative
callers may still derive the slug from their checkout's `origin`. This
truthful probe behavior is required for accurate `history_exhausted`
reporting.

The handler serializes `IngestReport` with no `receipt_id`. Both normal and
multi-backend runtime dispatch paths then persist the complete successful
report and inject the durable audit event id before returning. Direct handler
or administrative-ingester callers do not fabricate a receipt. Runtime
presentation classifies `git.digest` as `AlwaysVerbose`, preserving the strict
identity contract: the default MCP result is exactly the stored
`payload.result`, including the full `receipt_id` UUID.

### Repo-anchor resolution and conflict warning

An explicit `project` UUID is used as supplied, and an 8-or-more-hex-digit
prefix is resolved without a namespace filter. Either form can select an
anchor in a different namespace from the digest request. This is the handler's
current ID-resolution behavior: ADR-007 Rule 9 describes unfiltered UUID and
prefix resolution for parameters declaring that contract, but does not
expressly list `git.digest(project)` among them. When `project` is absent,
anchor discovery is restricted to the request namespace.

When `project` is absent, `git.digest` resolves a canonical `repo_slug` and
uses the tier order defined by ADR-088 Amendment 2: exact canonical slug,
exact legacy `repo_url`, normalized `repo_url`, then create. The normalized
route considers live anchors whose slug is absent or differs from the
canonical slug. A URL-equivalent anchor with a present non-canonical slug is
therefore repaired and reused rather than excluded and duplicated (#1708);
the selected anchor receives the canonical slug and a credential-redacted
`repo_url` in the same update. This includes a remote-less local repository's
canonical `local:<canonical-path>` identity. Redaction removes userinfo from
both scheme URLs and SCP-style shorthand, plus query and fragment material.
An accepted HTTPS source that does not satisfy the host-plus-two-segments slug
grammar uses a shared credential-redacted, query-free, trailing-slash/`.git`-
normalized URL as the identity while retaining its clone URL; reconciliation
calls that same fallback canonicalizer only through the accepted HTTPS parser.

An exact canonical-slug winner retains precedence over an older
URL-equivalent anchor with a conflicting slug. The conflicting row is left
unchanged and named in `warnings` for deliberate curation. The public warning
uses this form:

```text
multiple live project anchors resolve to the same repo identity; selected <id> by canonical resolution order; duplicate or conflicting anchors: <ids>
```

If ingest fails after duplicate anchors have been identified, the handler
emits that same sentence at WARN level, including the selected and duplicate
anchor ids. It returns the original error unchanged, preserving its kind,
message, and any typed storage retry semantics.

Candidate queries use `created_at ASC, id ASC`; tier precedence, selection,
and warning-id order are deterministic. An anchor id appears at most once in
the warning.

For the orphaned-corpus signal, matching soft-deleted anchors are considered
by `deleted_at DESC, id ASC` across the exact and normalized identity routes.
The first anchor in that order with live annotating notes supplies the signal;
an anchor without live notes is skipped.

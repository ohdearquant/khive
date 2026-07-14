# Implementation notes

- Added strict optional-string parsing for `git.commit.author`, `git.branch.from`, and
  `git.push.remote`. Explicit non-string values now return `RuntimeError::InvalidInput`
  naming the argument and expected string type instead of changing the requested Git
  operation through omission/default behavior.
- Routed missing and wrong-typed `repo` parameters through a shared audited parse path.
  Each rejected request emits one denied supplementary write audit with the safe
  `<invalid-repo>` marker before returning, without invoking Git.
- Added handler regressions proving malformed optionals cannot create commits, branches,
  or remote refs, and proving missing/non-string repo values are audited exactly once for
  all three write verbs without a Git process invocation.

## Verification

- `cargo fmt --all` — passed
- `cargo fmt --all -- --check` — passed
- `cargo test -p khive-pack-git` — passed (169 unit tests, 52 acceptance tests)
- `cargo clippy -p khive-pack-git --all-targets -- -D warnings` — passed
- `git diff --check` — passed

Domain utility: low — this fix was governed by repository-local ADR contracts, handler
implementation, and regression behavior; no external domain composition was needed.

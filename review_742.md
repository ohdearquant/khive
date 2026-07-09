Verdict: APPROVE-WITH-FIXES
Findings: 0 Blocker, 0 High, 1 Medium, 0 Low

### [Medium] Anonymous resolve test misses the `local` id leak regression

Evidence: `crates/khive-pack-brain/tests/resolve_actor.rs:128` creates the anonymous runtime for the new `brain.resolve` regression test, but `crates/khive-pack-brain/tests/resolve_actor.rs:130` binds the profile to `actor="lambda:test-seat"`. The dangerous anonymous path is specifically that `ActorRef::anonymous()` carries raw id `"local"` (`crates/khive-gate/src/actor.rs:65`) and must be converted through `binding_id()` (`crates/khive-gate/src/actor.rs:85`) so it becomes `None`.

Why this matters: If `handle_resolve` later regresses from `token.actor().binding_id()` to a raw `Some(token.actor().id.as_str())`, an anonymous caller would match an explicit `actor="local"` binding, but this new test would still pass because `"local"` does not match `"lambda:test-seat"`. That leaves the exact #708/#741 anonymous-boundary regression uncovered for the `brain.resolve` introspection path.

Suggested fix: Add a second anonymous `brain.resolve` test, or change this one, to bind the test profile with `actor="local"` and assert no-arg anonymous resolve does not return that profile and reports `matched_binding:false`.

## Looks Right

- `handle_resolve` honors an explicit `actor` verbatim and defaults omitted actor through `token.actor().binding_id()` (`crates/khive-pack-brain/src/handlers.rs:965`), so anonymous callers become `None` instead of leaking raw `"local"`.
- The only `handle_resolve` dispatch site now passes the token (`crates/khive-pack-brain/src/handlers.rs:2508`), and `rg -n "handle_resolve\\(" .` found no other callers.
- `resolve_with_match` behavior is unchanged in this diff; its existing matching still treats `None` as wildcard-only and exact-matches explicit actor rows (`crates/khive-brain-core/src/brain_state.rs:154`, `crates/khive-brain-core/src/brain_state.rs:161`).
- Test 1 is non-vacuous for the pre-fix bug: with old `p.actor.as_deref()`, the omitted actor would not match the bound `lambda:test-seat` row and would fail the `matched_binding:true` / profile-id assertions.
- Test 2 covers explicit actor override through the dispatch path.
- The rider descriptions match the handlers: `brain.activate` delegates only to lifecycle transition (`crates/khive-pack-brain/src/handlers.rs:991`), and `brain.reset` increments the profile reset epoch through the reset paths (`crates/khive-brain-core/src/profile.rs:185`, `crates/khive-pack-brain/src/handlers.rs:1162`).
- Publication hygiene checks over the diff and commit message body found no `ocean`, `lambda:leo`, or `lambda:khive`; the only actor strings added in the diff are synthetic `lambda:test-seat` / `lambda:other-seat` tests.

## Commands Run

- `date -Iseconds`: confirmed review started at `2026-07-08T22:27:05-04:00`.
- `git status --short --branch`: branch `fix/brain-resolve-actor-default`, ahead of `origin/main` by 1.
- `gh pr view 742 --json number,title,state,baseRefName,headRefName,url,headRefOid,updatedAt`: PR #742 is open; local HEAD matches `headRefOid` `aadb56b97e6b2fd5c76b2aa8f3483e095667fe9d`.
- `git diff --name-status origin/main...HEAD`: reviewed all three changed files.
- `git diff --check origin/main...HEAD`: no whitespace errors.
- `rg -n "handle_resolve\\(" .`: only definition and dispatch site found.
- `git diff origin/main...HEAD | rg -n -i "ocean|lambda:leo|lambda:khive"`: no matches.
- `git log --format=%B origin/main..HEAD | rg -n -i "ocean|lambda:leo|lambda:khive"`: no matches.
- Static source reads of changed files plus `crates/khive-gate/src/actor.rs`, `crates/khive-runtime/src/runtime.rs`, `crates/khive-runtime/src/config.rs`, `crates/khive-runtime/src/actor_identity.rs`, and resolver/binding handler sections.

## What I Did Not Check

- I did not run cargo fmt, clippy, or tests per the static-only instruction. The reported cargo gate results are treated as author claims, not observed results.

## Re-Review Guidance

- Narrow re-review only: verify the anonymous `brain.resolve` test binds `actor="local"` and fails if `handle_resolve` uses the raw anonymous actor id instead of `binding_id()`.

Domain utility: MEDIUM - khive memory recall was directly useful for the anonymous `binding_id()` review invariant; composed Rust typestate lore was mostly general boundary-check framing.

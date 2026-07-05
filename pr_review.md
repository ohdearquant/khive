Verdict: APPROVE-WITH-FIXES
Findings: 0 Blocker, 0 High, 3 Medium, 1 Low

### [Medium] Changed README tables still make the default pack total add to 73

Evidence: `README.md:66` says the default load gives 74 verbs, but `README.md:76` still lists the `comm` pack as 5 verbs. `npm/README.md:3` says "74 verbs", but `npm/README.md:32` also still lists `comm` as 5. The merged source exposes `comm.health` as a public verb with `Visibility::Verb` and no params at `crates/khive-pack-comm/src/vocab.rs:290`.

Why this matters: The PR updates the top-line total but leaves two user-facing pack tables summing to 73. Readers using the tables to audit the 74 total will conclude the docs are inconsistent.

Suggested fix: Change the `comm` row to 6 in both `README.md` and `npm/README.md`.

### [Medium] AGENTS.md still has the stale 73/5 verb-count contract

Evidence: `AGENTS.md:18` still says "**73 public verbs**"; `AGENTS.md:120` still says "Comm pack - 5 verbs", and the table at `AGENTS.md:124` through `AGENTS.md:128` omits `comm.health`. The actual handler definition at `crates/khive-pack-comm/src/vocab.rs:290` through `crates/khive-pack-comm/src/vocab.rs:303` documents `comm.health` as an Assertive public verb with an empty parameter list.

Why this matters: This is not historical ADR narrative; it is the current agent usage guide. The PR's stated stale-count sweep missed one of the main docs agents read before using the runtime.

Suggested fix: Update `AGENTS.md` to 74 public verbs, change the comm heading to 6 verbs, and add a `comm.health` row matching the no-argument health snapshot contract.

### [Medium] The smoke-test verb-count tripwire still expects 73

Evidence: `tests/smoke_test.py:194` through `tests/smoke_test.py:205` says the default 8-pack registry returns exactly 73 user-facing verbs and asserts `verbs_result["total"] == 73`. The new public `comm.health` handler is present at `crates/khive-pack-comm/src/vocab.rs:290` through `crates/khive-pack-comm/src/vocab.rs:303`.

Why this matters: The test is explicitly a drift tripwire for this surface. With the registry now at 74, leaving this assertion stale means the next smoke run will report the intended new verb as a regression.

Suggested fix: Update the comment and assertion to 74, and mention `comm.health` as the new comm-pack verb that changes the count.

### [Low] The literal no-new-em-dash gate is not clean

Evidence: `docs/guide/api-reference.md:909` adds the new heading `comm.health — Assertive` using the same em-dash delimiter as the rest of the API reference headings. `git diff --unified=0 origin/main...HEAD | rg '^\+.*—'` also shows this as a newly added heading line.

Why this matters: The heading is consistent with the surrounding API-reference style, but the review instruction asked to verify that no em dashes were introduced. If that gate is literal, the new heading violates it.

Suggested fix: Either explicitly exempt API-reference heading delimiters from the gate, or change the new heading punctuation in whatever broader style migration is intended for this document.

## Looks Right

- The new API reference count is internally consistent: `docs/guide/api-reference.md:3` through `docs/guide/api-reference.md:11` say 74, `docs/guide/api-reference.md:28` lists `comm` as 6, and `docs/guide/api-reference.md:33` through `docs/guide/api-reference.md:34` sum to 74.
- The `comm.health` entry matches the merged source: no params at `crates/khive-pack-comm/src/vocab.rs:302`, Assertive at `crates/khive-pack-comm/src/vocab.rs:301`, argument rejection at `crates/khive-pack-comm/src/handlers.rs:1102` through `crates/khive-pack-comm/src/handlers.rs:1109`, pinned local namespace read at `crates/khive-pack-comm/src/handlers.rs:1118` through `crates/khive-pack-comm/src/handlers.rs:1121`, timestamp/count-only projection at `crates/khive-pack-comm/src/handlers.rs:1060` through `crates/khive-pack-comm/src/handlers.rs:1070`, and empty-channel ambiguity at `crates/khive-pack-comm/src/handlers.rs:1088` through `crates/khive-pack-comm/src/handlers.rs:1093`.
- The relative link `docs/guide/api-reference.md:917` points to existing `docs/guide/communication.md`, whose health section starts at `docs/guide/communication.md:82`.
- `git diff --check origin/main...HEAD` reported no whitespace errors.

## Commands Run

- `date -Iseconds`: confirmed review started before the deadline.
- `git status --short --branch`: local checkout is `docs/verb-count-74...origin/docs/verb-count-74` with no pre-existing worktree changes.
- `git rev-parse HEAD`: `7968d2b1f56b21f9e04ac441e3d3a05232790d02`.
- `gh pr view 619 --json ...`: PR #619 head is the same `7968d2b1f56b21f9e04ac441e3d3a05232790d02`; changed files are the six markdown files listed in the task.
- `git diff --stat origin/main...HEAD` and `git diff --numstat origin/main...HEAD`: confirmed 6 changed markdown files, 30 insertions, 14 deletions.
- `rg -n '73 (user-facing|public|verbs)|expected 73|73 verbs|73-verb|Comm pack ...' --glob '!docs/adr/**' ... .`: found the stale non-ADR count references listed above.
- `rg -n 'comm.health|health' crates/khive-pack-comm docs/guide -S`: located the source handler contract and communication guide.
- `git diff --unified=0 origin/main...HEAD | rg '^\\+.*—'`: checked added punctuation lines for the no-new-em-dash gate.
- `git diff --check origin/main...HEAD`: passed.

## What I Did Not Check

- Static review only, per instruction. I did not run Rust builds, tests, smoke tests, docs generation, or the live `request(ops="verbs()")` registry.
- I excluded `docs/adr/**` from stale-count findings because the instruction allowed historical ADR narrative to remain as it was.

## Re-Review Guidance

Narrow re-review is enough after fixes: rerun the stale-count grep, confirm `README.md`, `npm/README.md`, `AGENTS.md`, and `tests/smoke_test.py` are consistent with 74 total verbs and `comm` equal to 6, then recheck the API-reference health entry still matches the source.

Domain utility: LOW - khive knowledge suggest returned no domains; role memory recall was useful for choosing a broad stale-count grep.

VERDICT: APPROVE-WITH-FIXES

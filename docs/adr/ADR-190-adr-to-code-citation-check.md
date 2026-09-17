# ADR-190: One code-keyed check for ADR-to-code citations

- **Status**: Proposed
- **Date**: 2026-09-17
- **Relates to**: [ADR-137](ADR-137-tailnet-wire-transport.md) — the guard over its Amendment 1 is
  one of the per-document checks this consolidates

## Context

`docs/adr/README.md` opens with: "These are **desired-state specifications** — the contract that
code must implement." ADRs make that contract concrete by citing code: a path, usually with the
symbol it holds. Those citations go stale, and nothing in the repository checks them.

### What was measured

At `405739349`, unless a line says otherwise.

**The corpus.** 192 `ADR-*.md` files under `docs/adr/`. Cited code paths extracted from them and
resolved against `git ls-files`, with a must-resolve and a must-not-resolve control in the same
pass: **291 distinct paths cited, 240 resolve, 51 do not.** Of the 51, **31 were never in the tree**
— the desired-state class a proposed ADR is entitled to — and **20 were in the tree and are gone**.
Of those 20, **14 are cited by accepted ADRs**, 1 by a withdrawn one, and 5 only by a non-ADR
document in the same directory.

**What breaks them.** Every one of the 14 was broken by a change to code, not to a document. Five
were module splits (`crates/khive-pack-kg/src/handlers.rs` into `handlers/`, and the same shape for
`apply_worker.rs`, `projection_worker.rs`, `crates/khive-pack-memory/src/handlers.rs`,
`crates/khive-storage/src/types.rs`), one a cross-crate move in `6d92490a`, one a deletion when the
binaries were unified in `2f2f3ca7`, and one path that never landed on `main` at all.

**What exists today.** Four committed instruments read the ADR corpus, all wired into
`scripts/ci.sh`:

| instrument                                               | what it checks                                                                    | scope        |
| -------------------------------------------------------- | --------------------------------------------------------------------------------- | ------------ |
| `scripts/lint-adr-refs.sh` (1,142 lines)                 | ADR→ADR integrity: catalog rows, link targets, titled references                  | whole corpus |
| `scripts/lint-adr-status.py` (1,024 lines)               | exactly one header status per ADR, from a known vocabulary, in the header's scope | whole corpus |
| `scripts/lint-adr-137-displacements.py` (504 lines)      | every passage its table quotes still exists in the file the row cites             | one document |
| `python/tests/test_response_wire_contract.py` (66 lines) | every cited source line exists in the named source                                | one document |

The last two are the same check, written twice, each after its own document broke. Neither is
reachable from the other, and a third copy — the titled-citation arm of `lint-adr-refs.sh` — checks
a related property for a different citation shape. Three implementations of doc-cites-code against
291 citations across 192 documents is what a missing general check looks like from the inside.

**The trigger is the half that is easy to get wrong.** `lint-adr-refs.sh` is registered in
pre-commit with `files: ^(docs/.*\.md|crates/(.*/docs/.*\.md|.*/design[^/]*\.md))$` — keyed on the
_document_ changing. `lint-adr-137-displacements.py` is keyed on ADR-137, its JSON table, or itself
changing. The event that breaks a code citation is a change to the _code_: the split of
`handlers.rs` into `handlers/` touched no `.md` file, so no document-keyed hook fired.
`scripts/ci.sh` runs both unconditionally, so CI would catch a code-keyed check and pre-commit
would not. That asymmetry is the argument for this ADR being about the trigger at least as much as
about the rule.

### The contradiction this has to settle

Two documents in this repository state opposite things about what a merged ADR is.

`docs/adr/README.md`, line 3: ADRs are "desired-state specifications — the contract that code must
implement."

`tests/documented_verb_counts.py`, its opening docstring: "Merged ADRs are historical records and
intentionally excluded."

Both are defensible and they imply opposite gate behaviour. Under the first, a dead citation in an
accepted ADR is a broken contract clause and must fail. Under the second, an accepted ADR is a
record of what was decided then, and rewriting its sentences to match today's tree falsifies the
record. A check cannot be written until this is decided, and deciding it by picking one document
over the other would strand the other.

### An instrument that was wrong, twice, by one mechanism

The inventory above is the corrected one. The first pass at "who reads the ADR corpus" grepped
`scripts/`, `.github/` and `cli/` for the literal `docs/adr` and missed
`scripts/lint-adr-137-displacements.py` and `scripts/lint-adr-status.py` — two of the four
instruments, one of them 1,024 lines and running in CI on every commit.

The mechanism is the same in both misses and is worth stating as a rule, because it will defeat the
check proposed here if the check is built the same way: **a path-literal search cannot find a
consumer that builds the path from components.** `lint-adr-status.py:56-57` reads

```python
ROOT = Path(__file__).resolve().parents[1]
ADR_DIR = ROOT / "docs" / "adr"
```

so the string `docs/adr` appears nowhere in it. `lint-adr-137-displacements.py` names its subject
the same way. Grep found neither, and the absence read as "no such instrument exists" rather than
as "this instrument cannot see that shape".

## Decision

### 1. A citation has a role, and the role decides the rule

Split citations by what the sentence is doing, not by the status of the document it sits in.

- A **pointer** says where something is: "Verb handlers: `crates/khive-pack-kg/src/handlers/`",
  "`resolve_uuid_async` (`crates/khive-pack-kg/src/handlers/common.rs`)". A pointer that does not
  resolve is a broken clause. It must fail.
- A **record** says what was true at a point in time: a file-change table listing what an
  implementation touched, or a sentence describing a move out of a path
  ("`DrainSummary` moves from `crates/kkernel/src/pending_events.rs` into ..."). Rewriting a record
  to match today's tree falsifies it. It must not fail, and it must not be silently edited.

This resolves the contradiction above without overruling either document: `README.md` is right
about pointers, `documented_verb_counts.py` is right about records. Status still gates, but only
the pointers: a pointer in a `Proposed` ADR is desired state and is reported, never failed.

### 2. A record is marked, and the mark is verified

A record is exempt only when it carries a marker, and the marker is checked like any other claim —
an unverified exemption is a hole with a comment on it.

Two forms, one inline and one section-scoped:

```
`crates/khive-mcp/src/main.rs` (removed in `2f2f3ca7`)
`crates/kkernel/src/pending_events.rs` (moved to `crates/khive-mcp/src/pending_events.rs` in `6d92490a`)
`crates/khive-capability/src/boot.rs` (never landed on `main`)

<!-- adr-citations: record, moved to crates/khive-mcp/src/pending_events.rs in 6d92490a -->
```

The section-scoped comment applies from its position to the next heading of the same or higher
level. Each form is verified:

- `removed in <sha>`: the sha must be reachable from `HEAD` and must delete that path.
- `moved to <path> in <sha>`: the sha must be reachable from `HEAD`, and `<path>` must resolve.
- `never landed on main`: the path must appear in no commit reachable from `HEAD`.

History queries here take `--full-history`. Without it, `git log <rev> -- <path>` reports zero
commits for a path whose history lies on the side of a merge that simplification drops: at
`405739349` the bare form returns 0 for `crates/khive-pack-kg/src/handlers.rs` and
`--full-history` returns 119. A verifier built on the bare form would confirm every
`never landed on main` marker it was handed.

### 3. Two rules, one check, one trigger

One instrument, `scripts/lint-adr-citations.py`, enforcing:

- **R1 — the path resolves.** Every pointer citation in an `accepted` ADR resolves against
  `git ls-files`. A trailing `/` denotes a directory and resolves if any tracked file sits under
  it. Pointers in `Proposed` ADRs are listed as desired state and do not fail. A `Superseded` or
  `Deprecated` ADR is a record **in its entirety**: its pointers are neither failed nor reported,
  because the decision it describes is no longer the one in force and the tree it pointed at is no
  longer the tree it was written against. That is the status half of the role split, and stating it
  here is what keeps a future reader from narrowing R1 to "accepted fails, everything else warns".
- **R2 — the quoted passage is present.** A citation that quotes source text must find that text
  exactly once in the file it cites. Zero occurrences fails; more than one fails asking for a
  longer quote, because a quote that matches twice keeps matching after the passage it was written
  for is deleted. This is `lint-adr-137-displacements.py`'s `check_citations` generalised, its
  ambiguity rule included.

Trigger:

- `scripts/ci.sh` runs it unconditionally, beside the three linters already there.
- Pre-commit registers it with a `files:` pattern covering **code** as well as documents:
  `^(docs/.*\.md|crates/.*|cli/.*|python/.*|scripts/.*)$`. Document-keyed is the defect; a hook
  that only fires when the document changes cannot see the commit that breaks the citation.

Both the status vocabulary and the markdown masking it needs — fenced blocks, inline code spans,
link destinations, autolinks — are already implemented and self-tested in `lint-adr-status.py`. The
new check imports them rather than re-deriving them; a second markdown parser in the same directory
is a second set of edge cases to get wrong.

### 4. What retires, and what does not

`python/tests/test_response_wire_contract.py` and the citation arm of
`scripts/lint-adr-137-displacements.py` retire into R1 and R2 in the same change that lands the
check. A third copy surviving is a fork nobody will find.

What does **not** retire, stated explicitly because "consolidate the three" would otherwise delete
it: `lint-adr-137-displacements.py` also checks that its decision table's labels match the ADR's own
decision set (`check_labels`, `check_scope_kinds`) and that the ADR contains its generated passages
verbatim (`precedence_passage`, `fence_passage`). Those are properties of one document's structure,
not citations, and no general citation check expresses them. They stay, in a script reduced to
them. The same applies to the wire-contract test's `R1..R17` inventory assertion.

## Consequences

**Positive.** The 291 citations get the check the three hand-built guards were each written to
provide for one document. A rename that breaks a citation fails at the commit that renames, not at
the next time someone reads the ADR. The record-versus-pointer split makes the ADR corpus honest
about which of its sentences are contracts and which are history, which is a property the corpus
does not currently declare anywhere.

**Negative.** Every future ADR that cites code has a new way to fail CI, and the failure will most
often land on a commit that has nothing to do with the ADR. That is the point, and it is still a
cost: the author of a rename now owns a documentation fix. The marker grammar is a small language,
and small languages accrete.

**Neutral.** The 31 never-existed citations under proposed ADRs stay exactly as they are and are
reported, not failed. They stop counting as breaks, which is the only thing wrong with them today.

**Carried into the implementation.** PR #2917 fixed the 14 dead citations using this ADR's
record/pointer split by hand, but wrote its three record annotations as prose rather than in the
marker grammar above. Those three become markers in the same change that lands the check, and that
change does not wire the check into `scripts/ci.sh` until the marker verifier's three arms each have
a must-fail control: a `removed in <sha>` whose sha does not delete the path, a
`moved to <path> in <sha>` whose destination does not resolve, and a `never landed on main` naming a
path that did land. An exemption grammar whose verifier is untested is a skip list with better
manners, and the arm most likely to be silently dead is the last one, because its query is the one
that returns empty for two different reasons.

## Alternatives considered

**Key the check on the document, like the existing hooks.** Rejected: it is the current behaviour
and it is what let all 14 through. A document-keyed hook cannot fire on the commit that breaks a
citation, because that commit touches no document.

**Fail every dead citation regardless of the sentence.** Rejected: it forces file-change tables and
"moves from X" sentences to be rewritten into falsehoods, and the first person to hit it will add a
skip list, which is the marker grammar with none of the verification.

**Extend `lint-adr-refs.sh` instead of adding a script.** Rejected: R2 needs quote normalisation and
occurrence counting over file bodies, which that script's `#!/bin/sh` cannot do without growing a
second implementation of what `lint-adr-137-displacements.py` already does in Python. The two
scripts stay separate because they check different axes, not because of their languages.

**Generate citations from code instead of checking them.** Rejected for now: it inverts the
contract. An ADR is written before the code it specifies, so a generated citation cannot exist at
the moment the ADR is written, which is the moment the citation is worth the most.

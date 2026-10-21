# ADR-208: Charter Workflows — Declared Topologies over Registered Phrases

- **Status:** Proposed
- **Date:** 2026-10-21
- **Relates to:** ADR-193 (charter runs), ADR-127 and ADR-143 (authenticated grant seams), ADR-182 (git merge verb).
- **Supersedes, when accepted:** ADR-193 D2 (definitions are bounded data for one closed template), the ADR-193 Context
  sentence "There is no language", and the ADR-193 deferred items "charter topologies beyond the template", "any
  definition language" and "cross-charter cascades". Every other ADR-193 decision and Amendments 1 to 4 stand
  unchanged.

## Context

This record serves the maintainer's decision of 2026-10-21 that the charter pack should track many procedures, each
declared in a small language, rather than one built-in procedure.

ADR-193 makes one action procedural: merging a pull request, through the closed template `pr_merge/v1`, whose phase
topology is Rust code. The work a long-running group of agents repeats is not one procedure. Review rounds, fix and
re-review loops, splitting an oversized change, release cuts and inbox triage all have the parts ADR-193 already
models (phases that wait for a trigger, requirements over recorded evidence, an action, an evidence record) and differ
only in topology. Encoding each topology in Rust makes every new procedure a code change, so in practice the rest go
untracked and their open loops live in the actors' memory.

ADR-193 D2 protects one property: a definition cannot express a predicate. Gates are compiled code with fixed
parameter schemas and security floors. This record keeps that property and drops the restriction to a single
topology.

## Decision

### D1. A charter is authored in a small declarative language, compiled by the charter pack

Keywords: `charter`, `roles`, `triggers`, `workflow`, `phase`, `await`, `or`, `require`, `when ... is`, `action`,
`perform`, `grants for`, `evidence`, `max_rounds ... then`, `complete`, `terminate`, `break_glass`. Blocks are indented.
`charter.compile(source)` parses and validates and returns the canonical definition with diagnostics, writing nothing.
`charter.publish(source)` stores that canonical definition immutably, with the source bytes kept beside it for display.
ADR-193 D10 pinning applies to the canonical definition, never to the source text.

### D2. The language names phrases; it never expresses predicates

`require <phrase>(<args>) <level>` references an entry in the compiled phrase registry. The language has no operators,
comparisons, boolean composition, variables, templating or user functions. Branching is `when <phrase> is <label>`,
where the label is a member of that phrase's declared, closed outcome set. A loop is bounded on its phase by
`max_rounds N then <phase>`. Publishing refuses an unknown phrase, an argument outside the phrase's schema, a label
outside its outcome set, a level above the phrase's ceiling, an unreachable phase, a cycle without a `max_rounds`
bound, and a workflow from which `complete` or `terminate` is unreachable.

### D3. The phrase registry is a compiled table

Each entry carries a name, an observation kind, an argument schema, an outcome set, a maximum enforcement level and a
producer class. The registry's canonical digest is the definition's `gate_registry_digest`. Every phrase evaluates by
the one ADR-193 D3 rule: the newest complete observation of its kind for the subject at the current generation, from a
registered producer; an unknown, missing, partial or conflicting observation blocks. A phase's first visit reads every
observation at the current generation, including those recorded while the run was in an earlier phase; a revisit reads
only observations recorded after its previous visit ended, so a bounded loop never reuses the evidence that sent it
round. Phrases over the store's own state
(a task's status, a message present, a graph edge, a schedule fire) are produced by a built-in instrument that reads the
store and records an observation, so evaluation stays pure. A phrase whose truth cannot be established mechanically,
or that only the checked actor can report about itself, has an advisory ceiling.

### D4. A trigger is an observation kind

`await <trigger> [or <trigger> ...]` holds the phase in `waiting_trigger` until an observation of one of those kinds is
recorded for the subject at the current generation. There is no separate trigger queue. The ADR-193 D5 waiting view
covers every charter, so the open loops of every declared procedure are one query.

### D5. Subjects are generic

A subject is a registered subject kind plus a reference under the policy domain. A pull request (forge, repository id,
pull request id, target) is one kind. ADR-193 D8's generation and run-key rules apply per kind, and each kind declares
what starts a new generation: the triggers whose observation moves the subject to the generation it names. Any other
observation is recorded at the generation it names and never moves the subject, so a late result for an older candidate
cannot roll the subject back.

### D6. Levels and milestones are unchanged

Levels are hard, soft and advisory as in ADR-193 D6. The recording milestone (ADR-193 D11, M1) applies to every
charter: levels are recorded, nothing blocks and `charter.claim` refuses. An action becomes enforceable only under the
M2 terms, and only where every hard phrase it depends on has a producer independent of the actor it checks.

### D7. Actions are registered adapters

An `action` names an entry in an action registry beside the phrase registry. In a phase it records the intended effect
and its designated actor; the charter itself performs no external I/O (ADR-193 D7).

`perform <charter>` is the one built-in action kind: it starts a child run of the named published charter on the same
subject, and the phase waits for that run to end. The child's terminal state is recorded in the parent run as an
observation of kind `<charter>` with the closed outcome set `completed`, `terminated`, `cancelled`, so the parent routes
on it with `when <charter> is <label>`. A procedure such as "review; if the review fails perform the fix procedure; if
the fix fails perform the split procedure and terminate" is therefore three charters composed, not one. `terminate`
ends the run and cancels every open run it started, transitively.

## `pr_merge/v1`

The template is re-expressed as a charter source. A fixture asserts that its compiled definition carries the same
gates, floors and levels as the template, so the existing acceptance cases keep their meaning.

## Acceptance (recording slice)

1. Three fixture charters compile: a pull-request review workflow with review, fix and split phases, a single review
   round, and an inbox triage.
2. Every D2 refusal has a must-fail fixture that names the refused construct.
3. Changing any registry entry changes `gate_registry_digest`.
4. An `await` is released by a matching observation and by no other write.
5. The waiting view lists a phase that waits on a role the caller holds.
6. A `perform` step starts one child run, its terminal state releases the parent phase as an observation, and
   terminating the parent cancels the child.

## Alternatives considered

- **Amend ADR-193 D2.** Rejected: reversing D2 inside that record leaves its Context paragraph contradicting it.
- **An authoring tool outside the pack that emits definition data.** Rejected: it needs a second copy of the phrase
  registry, which drifts from the one evaluation uses.
- **An expression language for gates.** Rejected: that is the property D2 protected.
- **One enum variant per phrase.** Rejected: every new procedure becomes a type change across the pack.

## Known limits

- Phrase coverage grows by code change. A charter that names a phrase not yet registered cannot publish.
- Soft overrides wait for the grant service, as in ADR-193 D6.

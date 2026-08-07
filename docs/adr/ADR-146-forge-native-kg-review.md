# ADR-146: Forge-Native KG Review — Pull Requests and Issues as the Collaboration Surface

**Status**: proposed\
**Date**: 2026-08-07\
**Authors**: khive maintainers

## Context

[ADR-010](ADR-010-kg-versioning.md) fixed the strategic position: "GitHub for Knowledge
Graphs." KG state is serialized as sorted NDJSON in a git repository, git provides
versioning, and the forge provides the social layer. [ADR-020](ADR-020-git-native-kg-implementation.md)
is the implementation contract for that position: directory layout, NDJSON format,
two-layer storage, CLI verbs, merge semantics. [ADR-101](ADR-101-kg-changeset-model.md)
defines the semantic change-set (producer envelope plus ordered operations), and
[ADR-102](ADR-102-tiered-validate-and-merge.md) defines tiered validation and the
independent-review gate over it. [ADR-145](ADR-145-local-first-kg-workbench.md) added the
local-first review workbench and the `khive.review.v1` read contract.

What no ADR yet states is how these compose into one review experience. ADR-145's first
slice renders fixtures, and its UI vocabulary (review pages, gates, approvals) is not yet
bound to the forge objects that ADR-010 named as the social layer. Left unstated, the
workbench drifts toward a parallel review system: its own notion of a review, its own
identifiers, its own lifecycle, none of which a collaborator's existing tooling
understands. ADR-020 already rejected a custom VCS layer for exactly this reason; the
same argument applies one level up, at the review layer.

This ADR closes that gap. It is an alignment contract, not a new subsystem.

## Decision

### D1 — A KG review IS a pull request; a curation ask IS an issue

There is no khive-native review object. The unit of proposed KG change is a pull request
on a repository whose `.khive/kg/` NDJSON changed, exactly as ADR-020 §10 defines
branching. The unit of requested-but-unimplemented curation ("this entity should be
split," "this edge looks wrong") is a forge issue on the same repository.

The workbench, the CLI, and any future server surface are presentation and composition
layers over those two objects. They never mint a second system of record. A review that
exists only inside a khive store, invisible to `git log` and the forge, is a design
error under this ADR.

The `khive.review.v1` contract already anticipates this: its `pull_request` review kind
carries full repository identities (owner, name, base and head SHAs) and pull-request
identities (number, title, state, author, head SHA). This ADR promotes those fields from
fixture decoration to the binding: a `pull_request` bundle describes a real PR, and a
bundle whose head SHA no longer matches the live PR head is stale and blocks approval,
as ADR-145 already requires.

### D2 — The UX contract is the forge PR page, with a semantic diff pane

The workbench mirrors the structure every collaborator already knows:

| Forge concept                       | KG review surface                                                    |
| ----------------------------------- | -------------------------------------------------------------------- |
| Conversation tab                    | Review conversation (ingested forge comments, read-only first)       |
| Files-changed tab                   | Semantic diff: ADR-101 operations rendered per entity/edge/note      |
| Checks tab                          | ADR-102 validation findings, one check row per rule class            |
| Required reviews / protected branch | Tier routing and the independent-review gate, shown as review policy |
| Approve / Request changes / Comment | The same three verbs, no invented review states                      |
| Merge box                           | Merge state readout; merging happens forge-side                      |

KG-specific intelligence (tier summaries, affected-subgraph projection, evidence
anchors) appears as additional panels inside this structure, never as a replacement for
it. Where the first slice invented layout that conflicts with this mapping, the mapping
wins in the next slice.

### D3 — One composition path from repository state to rendered review

`khive kg review` composes a `khive.review.v1` bundle from three sources, each already
specified elsewhere:

1. **The change set**: ADR-101 operations derived from the base-to-head difference of
   the committed NDJSON state (ADR-020 layout, ADR-020 §7 diff semantics).
2. **Forge metadata**: PR number, title, body, state, author, comment stream, fetched
   through a thin adapter. GitHub is the first adapter; the bundle schema stays
   forge-neutral.
3. **Validation**: ADR-102 tier classification and findings, computed at the head state.

The workbench consumes the composed bundle unchanged. The CLI, CI, and the browser
render the same bytes, which is the property ADR-145's golden vector already pins.

### D4 — Issues close the loop through the graph

Curation asks are forge issues. The existing git pack already ingests commits, issues,
and pull requests into the graph as note kinds; this ADR makes that ingestion the
designated memory of the review process. A merged KG PR and its review conversation
become graph notes linked to the entities they touched, so the graph carries its own
curation history and retrieval can surface "this entity was disputed in review" as
context. No new note kinds are introduced.

### D5 — Write-back ladder

Review actions gain capability in slices, and each capability is declared in the
bundle's capability block rather than assumed by the UI:

| Slice | Capability                                                           | Persistence              |
| ----- | -------------------------------------------------------------------- | ------------------------ |
| 1     | Local-only decisions (ADR-145 first slice, shipped)                  | Browser session only     |
| 2     | Forge read: live PR metadata, comments, check states via adapter     | Read-only                |
| 3     | Forge write-back: approve, request changes, comment, behind explicit | Forge is the record      |
|       | per-action capability and never on by default                        |                          |
| 4     | Merge remains forge-side under branch protection; post-merge         | Repository is the record |
|       | `kg sync` rebuilds the working database (ADR-020 §11 hooks)          |                          |

Slice numbering here is a dependency order, not a schedule. Skipping a slice is not
permitted: write-back without live read has no stale-detection substrate.

### D6 — Gates are checks, not UI decorations

The tier-routing requirement and the independent-review gate (ADR-102) are enforced
where the forge enforces everything else: as required status checks and review policy on
the protected branch. `khive kg validate` and a gate check run in CI on every PR
touching `.khive/kg/**` (ADR-020 §12 already generates this workflow). The workbench
renders the same gate state it fetches; a gate that exists only in the browser is
advisory, and this ADR treats advisory gates as absent.

## Non-Goals

- No custom VCS or sync server (ADR-010, ADR-020 already rejected these; unchanged).
- No CRDT or automatic semantic merge (ADR-010 rejection stands).
- No expansion of the `live_proposal` variant; the event-sourced proposal lane
  (ADR-046) remains a later, explicitly typed link from a `pull_request` bundle.
- No forge-agnostic abstraction beyond the thin adapter seam: the adapter trait exists
  so the bundle schema never embeds GitHub-specific field shapes, not to promise
  day-one support for other forges.

## Alternatives Considered

| Alternative                                              | Why rejected                                                                                                                                             |
| -------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Native khive review objects (PR-shaped, stored in khive) | Recreates the custom-VCS mistake one layer up: a second lifecycle, invisible to git/forge tooling, with sync burden against the real one.                |
| Serialize reviews as files in the repo                   | Review state is social, not graph state; committing it pollutes KG history and conflicts with forge conversation as the record.                          |
| Forge-first UI with no local workbench                   | Loses the local-first property ADR-145 established: semantic diff, offline review of a bundle, and agent-produced changesets without a forge round-trip. |
| UI-enforced gates only                                   | Advisory by construction; any client that skips the UI skips the gate. CI-enforced checks are the only gate the merge path actually sees.                |

## Consequences

### Positive

- Zero new review concepts: collaborators and agents reuse PR and issue muscle memory,
  and every forge tool (notifications, CLI, mobile, CI) works on KG reviews unchanged.
- The graph learns from its own review history through existing git-pack ingestion.
- The workbench's mock surfaces get an unambiguous functional target: each panel maps
  to a named source (change set, forge adapter, validation) rather than to invented
  data.

### Negative

- Full review flow requires a forge remote; purely local repositories get the changeset
  and validation panels but no conversation or write-back. This is the ADR-010 trade
  accepted again, not a new cost.
- Forge adapter surface area (auth, rate limits, pagination) becomes part of the review
  path's operational envelope at slice 2, ahead of any server deployment.

### Neutral

- ADR-145 is unchanged in substance; its first slice is re-scoped as slice 1 of the D5
  ladder, and its UI vocabulary is bound to the D2 mapping for subsequent slices.
- The remaining unimplemented ADR-020 verbs (`kg diff`, `kg resolve`, `kg update`,
  `kg migrate`, and `kg sync` as a CLI verb) are unblocked prerequisites for slices 2+
  and tracked as implementation work, not redesigned here.

## Implementation Status

Current CLI surface (`kkernel kg`): `init`, `validate`, `fetch`, `export`, `import`,
`status`, `hook`, `commit`, `review` — present. ADR-020 verbs not yet surfaced:
`diff`, `resolve`, `update`, `migrate`; `sync` exists as a library operation
(`khive-vcs`) without a CLI verb. The workbench ships slice 1 (fixture and import,
local-only decisions). Slice 2 begins with the forge read adapter and live PR binding.

## References

- [ADR-010](ADR-010-kg-versioning.md): KG versioning strategy — strategic root
- [ADR-020](ADR-020-git-native-kg-implementation.md): Git-native implementation contract
- [ADR-046](ADR-046-event-sourced-proposals.md): Event-sourced proposals — later variant
- [ADR-101](ADR-101-kg-changeset-model.md): Change-set model
- [ADR-102](ADR-102-tiered-validate-and-merge.md): Tiered validation and merge gate
- [ADR-145](ADR-145-local-first-kg-workbench.md): Local-first KG workbench and
  `khive.review.v1`

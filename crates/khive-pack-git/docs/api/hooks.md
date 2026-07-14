# Git pack `KindHook` design notes

Extracted from `crates/khive-pack-git/src/hook.rs` doc-comments.

## Module overview

Validation only — this pack introduces no new edges at `after_create` time.
Provenance edges (`annotates` -> project / document / merging PR) are
supplied by the caller (the ingester, see `src/ingest.rs`) as part of the
generic `create(kind=..., annotates=[...])` call; the runtime's own
`create_note` path validates and links them atomically, so no
`after_create` edge-creation logic is needed here (unlike gtd's
`TaskHook::after_create`).

## `IssueLikeHook`

GitHub issue/PR numbers are repository-scoped, not globally unique — two
different `project` entities in the same namespace can each have a `#1`.
`properties.project_id` is the natural-key scoping field the ingester's
`find_by_number` lookup filters on, so it is required and validated as a
UUID here rather than left to the caller's discipline.

`ISSUE_STATE_REASONS` is `pub(crate)` so `ingest::MaskedIssueFields` can
classify against the same set at the masking boundary, before an ungoverned
(possibly credential-shaped) raw value can reach this hook's error path.

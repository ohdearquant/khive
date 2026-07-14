# Schedule replay validation

Internal notes for the write-time validators in `src/handlers.rs` that guarantee a
scheduled `schedule.schedule` action can be replayed exactly as stored (issue #461).
None of these functions are public API — this is background for maintainers reading
`src/handlers.rs`.

## `validate_at` — future-timestamp check

Accepts any RFC 3339 string chrono can parse as `DateTime<Utc>` (e.g.
`"2027-01-01T00:00:00Z"` or with a numeric offset). Returns the parsed UTC instant so
callers can compare without re-parsing; the original string is preserved by callers
who want to store it as-is. Rejects unparseable strings and timestamps in the past
relative to `Utc::now()`.

## `validate_repeat` — cron-lite repeat spec

Accepts the literals `daily`, `weekly`, `monthly`, and a limited five-field form
`MIN HOUR DOM MON DOW` where each field is `*` or one non-negative integer within:
MIN 0–59, HOUR 0–23, DOM 1–31, MON 1–12, DOW 0–7.

Standard cron operators (steps `*/15`, ranges `9-17`, lists `0,30`) are NOT accepted
(issue #481): `kkernel`'s pending-events runner does not yet compute next-fire times
for cron-form repeats (it fires them one-shot), so accepting full cron syntax would
imply recurrence semantics that don't exist yet. Use `daily`/`weekly`/`monthly` for
recurring runtime advancement until cron next-fire support lands. Malformed fields
(non-numeric, out-of-range, or a cron operator) are rejected rather than silently
accepted.

## `validate_action` — DSL parseability

Validates that `action` parses via `khive_request::parse_request`, catching garbage
like `"x"` or `"bogus-not-a-valid-verb()"` at write time rather than at trigger time,
when nobody is watching.

## `validate_replayable_single_action` — replay-safety gate (issue #461)

The pending-events runner reparses the stored DSL at trigger time and dispatches it
through the normal request surface. For that replay to succeed, the stored action
must be:

- A single op against an exactly-registered handler name (not a bare shorthand
  resolved via a `schedule.{tool}` fallback).
- Built from literal argument values only (no `$prev` references — those are only
  meaningful inside a chain the replay path does not reconstruct).
- Complete with respect to all required handler parameters.

Rejecting anything else at write time prevents storing an action that is guaranteed
to fail (and be silently marked "fired") when it comes due.

It also rejects any handler whose schema declares `namespace` as a business param
(issue #461/#462): `dispatch_action` in `pending_events.rs` unconditionally injects
the firing event's routing namespace into every op's args, and the registry passes
it through unchanged whenever the handler declares `namespace`
(`khive-runtime/src/pack.rs`). For handlers that treat `namespace` as a business
param (e.g. `brain.bind`, `brain.resolve`), that silently changes the business value
on replay — even when the *stored* action omitted `namespace` entirely (e.g.
`brain.bind` defaults an omitted `namespace` to the wildcard `"*"` at write time;
replay would instead bind it to whatever namespace the event happens to fire from).
Replay cannot yet carry routing-namespace and arg-namespace as separate concepts, so
this is rejected at write time based on the handler's schema alone, regardless of
whether the stored args happen to include `namespace`.

## `validate_conditional_requirements` — hard-coded conditional-required-param cases (issue #461)

`validate_args_against_help` only enforces metadata-declared `required:true` params.
Some handlers accept one of several alternative arg sets (e.g. `create` requires
`kind` unless bulk `items` is given), so neither alternative is marked required in
metadata and both can be omitted at write time — then fail at trigger-time replay.
This function hard-codes the known cases; it is not a general
conditional-requirements mechanism (there is no metadata surface for that yet), so
it does not guarantee every handler-internal semantic precondition is caught.

For `tool == "create"`, this mirrors the singleton branches of the KG pack's own
`handle_create` (`khive-pack-kg/src/handlers/create.rs`): entity/granular-entity
creates require `name`, note/granular-note creates require `content`, and a bare
`kind="entity"` requires an `entity_kind` (or a granular entity kind) to resolve a
concrete kind. It also validates `entity_type` against the KG entity-type/subtype
registry when present — see `validate_entity_type_for_replay` below.
`khive-pack-schedule` does not depend on `khive-pack-kg` in production (only as a
dev-dependency for tests), so this reimplements the classification using
`VerbRegistry::all_entity_kinds`/`all_note_kinds` — the same data `resolve_kind_spec`
consults — rather than importing the KG pack's private helpers.

## `classify_create_kind` — mirrors `khive-pack-kg::handlers::common::resolve_kind_spec`

Classification order: literal substrate keywords first, then base-8-kind aliases
(`khive_types::EntityKind`, e.g. `"paper"` -> `document` — a real, non-dev dependency
already shared with khive-pack-kg, so this is genuine reuse, not a hand-copy), then
the pack-local `resource`-kind alias set (`"atom"`, `"runbook"`, etc. -> `resource`,
ADR-048; hand-copied via `resource_alias_for_replay` since
`khive-pack-kg::vocab::EntityKind` is pack-private — see below), then the registry's
merged entity/note-kind vocabulary (the same final fallback `resolve_kind_spec`
uses). Returns an error for any `kind` guaranteed to fail replay outright: `edge`
(create edges via `link`), `event` (immutable), `proposal` (create via `propose`),
or an unrecognized kind string.

The pre-fix version skipped alias resolution entirely, causing schedule-time false
rejections (not a security hole) for legitimate KG-accepted spellings like `"paper"`
and `"atom"`.

## `canonical_entity_kind_for_replay` / `canonical_note_kind_for_replay`

Mirror `khive-pack-kg::handlers::common::canonical_entity_kind`/
`canonical_note_kind`. The entity variant tries the base `khive_types::EntityKind`
parser (8 base kinds + common aliases, e.g. `"paper"` -> `document`), then the
pack-local `resource` alias set, then the registry's merged entity-kind vocabulary
(covers further pack-declared additions). The pre-fix version resolved neither
alias set, causing the same class of false rejections noted above. Note kinds carry
no alias set beyond their 5 canonical names (ADR-013), so the note variant is
exactly the registry's merged note-kind vocabulary check.

## `resource_alias_for_replay` — hand-copied ADR-048 alias set

Mirrors `khive-pack-kg::vocab::EntityKind`'s `FromStr` arm `"resource" | "atom" |
"runbook" | "template" | "prompt" | "skill" | "tool"` (`khive-pack-kg/src/vocab.rs`).
That type is `pub(crate)` to `khive-pack-kg`, and `khive-pack-schedule` does not
depend on `khive-pack-kg` in production (dev-dependency only, for tests), so this
hand-copies just the alias set — six short strings — rather than the type.
`normalized` must already be trimmed + lowercased. Kept in sync by
`entity_kind_resource_aliases_match_real_vocab` in `create_validation.rs`, which
asserts this list against the live `khive-pack-kg` vocab (via the dev-dependency) so
drift is caught in CI rather than silently reproducing a similar false rejection.

## `validate_entity_type_for_replay`

Mirrors `khive-pack-kg::handlers::common::validate_entity_type`: parses
`canonical_kind_name` into the base `khive_types::EntityKind` first, then resolves
the subtype against the boot-time composed registry (builtin subtypes plus every
loaded pack's `ENTITY_TYPES`, `VerbRegistry::all_entity_types`) — NOT the
builtin-only `EntityTypeRegistry::global()`. KG create validation and schedule
replay validation resolve against the exact same composed set, so this cannot drift
from the real handler's vocabulary, and a scheduled `create` naming a pack-declared
subtype (e.g. git's `adr` Document subtype) is accepted at schedule time exactly
when the real handler would accept it at trigger time.

The kind-parse step is exactly what makes a pack-owned kind like `"resource"` reject
*any* non-`None` `entity_type` outright: `resource` has no variant in the base
8-kind enum, so parsing `canonical_kind_name` fails before the subtype table is even
consulted — the real handler has this same short-circuit
(`khive-pack-kg/src/handlers/common.rs`, `validate_entity_type`), verified live via
`kkernel exec` against a scratch DB. This mirrors that behavior rather than
"fixing" it: the contract here is bit-for-bit replay parity with the real handler,
not what the real handler arguably should do.

## `reconcile_specific_for_replay`

Reconciles a granular `kind`'s resolved `specific` value against a legacy
`entity_kind`/`note_kind` argument, mirroring
`khive-pack-kg::handlers::common::reconcile_specific` exactly (including the
contradiction error shape) so a scheduled action that the real KG `create` handler
would reject for a kind/legacy-kind contradiction is rejected at schedule time too,
not only discovered at trigger-time replay. `context` prefixes error messages (e.g.
`"items[3] "` for a bulk entry, `""` for the singleton path).

## `ScheduleBulkCreateEntryCheck` / `validate_create_bulk_items`

`ScheduleBulkCreateEntryCheck` mirrors
`khive-pack-kg::handlers::params::BulkCreateEntry`'s exact field set (including
`#[serde(deny_unknown_fields)]`) so schedule-time validation rejects the same
malformed entries the real bulk handler would. `validate_create_bulk_items`
validates a `create(items=[...])` bulk payload the way `handle_create`'s bulk path
would: `items` must parse into that shape (required `kind` + `name`,
deny-unknown-fields), and bulk create only supports entity kinds (never note kinds).

Source: `crates/khive-pack-schedule/src/handlers.rs`.

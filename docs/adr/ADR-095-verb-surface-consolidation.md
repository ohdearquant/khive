# ADR-095: Verb-Surface Consolidation and Field-Validation Governance

**Status**: Accepted
**Date**: 2026-07-05
**Depends on**: [ADR-016](./ADR-016-request-dsl.md),
[ADR-017](./ADR-017-pack-standard.md),
[ADR-023](./ADR-023-declarative-pack-format.md),
[ADR-084](./ADR-084-verb-surface-consistency.md)

## Context

Pack verbs and generic substrate verbs can sometimes perform the same storage operation.
Removing a named verb merely because it eventually calls `create`, `search`, or
`update` would reduce discoverability and break callers without necessarily removing
implementation duplication.

The actual maintenance problem is duplicate internal paths: a named verb may implement its
own storage write and field checks while `create(kind=...)` reaches a `KindHook` for the
same kind. Those paths can diverge even when both public operations are intentional.

The runtime already provides two relevant composition seams:

- declarative `EDGE_RULES` for endpoint validation; and
- `KindHook::prepare_create` for per-kind create preparation and validation.

This ADR governs how those seams are used. It does not redefine the public catalog.

## Decision

Keep the public verb surface unchanged. Consolidate duplicate implementations internally
and establish one validation owner per field and lifecycle phase.

### 1. Preserve intentional public verbs

No verb is added, removed, renamed, deprecated, or aliased by this ADR. A named verb may
remain when its name communicates a domain action, even if its implementation ultimately
uses a generic substrate operation.

Surface-size reduction is not an independent objective. A proposal to retire a verb must
show that the semantic operation and its discoverability are genuinely redundant, and must
provide an explicit wire migration.

### 2. Unify internal write paths

When a named verb creates a registered entity or note kind, its implementation routes
through the same substrate create path and `KindHook` used by
`create(kind=...)`. It does not maintain a parallel call to storage.

The named handler may still:

- parse domain-oriented parameters;
- resolve references;
- construct the canonical create input;
- choose its response presentation; and
- run lifecycle behavior that is not part of creation.

Storage mutation, create-time defaults, and create-time kind validation occur once, in the
shared path.

### 3. Create-time field validation belongs to the kind hook

For a registered kind, all rules that can reject or normalize a create request live in
`KindHook::prepare_create`. A bespoke verb calling that kind must invoke the same hook.

Examples include:

- closed-enum membership;
- required and mutually exclusive fields;
- reference resolution needed before persistence;
- default values; and
- pre-write dependency existence checks.

Handlers may share parsing helpers with hooks, but helper reuse alone is not conformance if
the handler can bypass the hook.

Validation rejects invalid values. It does not silently coerce them. Help and schema output
must describe the accepted field names and constraints consistently with ADR-084.

### 4. Edge validation remains declarative

`EDGE_RULES` remains the authority for pack-declared endpoint rules. This ADR does not add
a second edge validator or reopen the closed relation taxonomy.

### 5. Update-time rules remain at their existing owner

There is no generic `KindHook::prepare_update` contract in this decision. Existing update
handlers retain their current validation and atomic lifecycle guards.

If independent kinds require the same update-time composition seam, a later ADR may define
`prepare_update` or `validate_update`. One implementation must not pre-emptively create a
generic registry without multiple concrete consumers and compatibility tests.

### 6. No generic field-rule language

Per-kind checks remain ordinary Rust validation in `KindHook`. A declarative JSON Schema
or separate field-rule registry is not introduced. The current seam already composes pack
behavior and is sufficient for the public validation volume.

### 7. Wire compatibility

This ADR has zero wire delta:

- verb discovery is unchanged;
- request names and parameter shapes are unchanged;
- response envelopes are unchanged;
- no deprecation aliases are introduced; and
- no taxonomy or storage migration is required.

Internal-path unification must preserve error categories, atomicity, authorization checks,
event emission, and presentation behavior.

## Conformance rules

A new or modified pack conforms when:

1. each registered entity or note kind has one create-time validation owner;
2. every public path creating that kind reaches the owner;
3. direct storage writes cannot bypass the owner;
4. help/schema output uses the same field names and enum values;
5. generic and named entry points produce equivalent stored records for equivalent input;
6. authorization and atomicity are checked at the same shared seam; and
7. validation failures occur before persistence.

## Verification

For each internally unified path, tests must compare the generic and named entry points:

- valid input produces equivalent stored records;
- invalid enum and missing-field errors match;
- dependency checks reject before any write;
- authorization denial occurs at the same point;
- emitted events do not duplicate;
- atomic compound behavior is preserved; and
- wire discovery and response shapes remain unchanged.

A repository-level conformance test should enumerate registered kinds and assert that each
declared hook is reachable through generic creation. Pack-specific tests remain responsible
for domain rules.

## Alternatives considered

| Alternative                                               | Reason rejected                                                                                       |
| --------------------------------------------------------- | ----------------------------------------------------------------------------------------------------- |
| Retire every named verb that wraps a substrate operation  | Breaks callers and reduces semantic discoverability without being necessary to remove duplicate code. |
| Keep permanent aliases while planning eventual retirement | Creates two public names and an indefinite migration state.                                           |
| Build a composed field-validation registry                | Duplicates the existing kind-hook seam.                                                               |
| Use per-kind JSON Schema as the runtime validator         | Adds a second schema language and validator for checks already expressed locally.                     |
| Leave named handlers on direct storage paths              | Preserves the divergence this ADR addresses.                                                          |

## Consequences

### Positive

- Equivalent writes share one storage and validation path.
- Named domain operations remain discoverable.
- Create-time rules have a clear owner without a new rule engine.
- The public surface and closed taxonomies remain stable.

### Negative

- The verb catalog does not become smaller.
- Unification can require careful refactoring even though the wire is unchanged.
- Update-time validation remains distributed until multiple consumers justify a shared
  seam.

## Non-goals

- No changes to entity kinds, note kinds, edge relations, or storage substrates.
- No redesign of the schema-introspection surface.
- No new update-time hook.
- No retirement plan for any specific public verb.

## References

- [ADR-016](./ADR-016-request-dsl.md): request and response grammar
- [ADR-017](./ADR-017-pack-standard.md): pack and kind-hook contracts
- [ADR-023](./ADR-023-declarative-pack-format.md): verb visibility and composition
- [ADR-084](./ADR-084-verb-surface-consistency.md): schema and validation consistency

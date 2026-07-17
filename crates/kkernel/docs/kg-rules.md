# KG Rule Configuration

**ADRs**: ADR-034 (KG validation), ADR-035 (kg init and vcs status)
**Last reviewed**: 2026-06-06

## Overview

`kkernel kg validate` applies built-in structural checks plus configurable lint rules loaded from
`.khive/kg/rules.toml`. Rules are evaluated against entities and edges in the NDJSON repository.

## Rule Format

Rules are defined in `rules.toml` as TOML array-of-tables:

```toml
[[rules]]
id = "concept-must-have-description"
severity = "warning"
kind = "entity"
# condition: field=value equality predicate; records must match to be checked
condition = "kind=concept"
# require_field: the record must have a non-empty value for this field
require_field = "description"
message = "Concept entities must have a description"

[[rules]]
id = "no-self-loops"
severity = "error"
kind = "edge"
# special sentinel: literal string "source_id=target_id" means source == target
condition = "source_id=target_id"
message = "Edges must not be self-loops"
```

## Fields

| Field           | Type   | Required | Description                                                                       |
| --------------- | ------ | -------- | --------------------------------------------------------------------------------- |
| `id`            | string | yes      | Unique rule identifier (appears in `RuleResult.id`)                               |
| `severity`      | string | no       | `"error"`, `"warning"`, or `"info"` (default: `"warning"`)                        |
| `kind`          | string | yes      | `"entity"` or `"edge"` — substrate the rule applies to                            |
| `condition`     | string | no       | `field=value` equality predicate; `source_id=target_id` is the self-loop sentinel |
| `require_field` | string | no       | Rule fails if this field is absent or empty on matching records                   |
| `message`       | string | no       | Human-readable violation message (`{id}` is replaced with the record ID)          |

## Built-in Checks

The following structural checks run before configurable rules and cannot be disabled:

- **Duplicate UUIDs** — each entity and edge ID must be unique within its NDJSON file.
- **Referential integrity** — every edge `source_id` and `target_id` must reference a known entity.
- **Valid entity kinds** — entity `kind` must be one of the 8 closed kinds (ADR-001).
- **Valid edge relations** — edge `relation` must be one of the 15 closed relations (ADR-002).

## CLI Options

```
kkernel kg validate [--repo <path>] [--rules <path>] [--fix] [--strict] [--format text|json|github]
```

- `--no-rules` — run built-in checks only; skip `rules.toml`
- `--strict` — treat warnings as errors; exit 1 when warnings > 0
- `--fix` — apply fixable rules and report what changed
- `--verbose` — show all violations (default: cap at 2 then `+ N more`)
- `--quiet` — print summary line only

## Failure Modes

- Missing `rules.toml` — not an error; built-in checks still run.
- Malformed `rules.toml` — returns a parse error; no checks are applied.
- Unknown `kind` in a rule — the rule is skipped with a warning.
- A rule with neither `condition` nor `require_field` matches nothing.

## `build_taxonomy` — strict-actor-mode exemption

`build_taxonomy` (`src/kg/validate.rs`) merges the entity-kind/note-kind sets declared by
every registered pack, mirroring the `build_registry()` pattern in `pack_introspect`; no DB
is opened, only pack metadata is read. It deliberately does NOT call
`enforce_strict_actor_mode`: that enforcement seam protects the **comm dispatch boundary** —
it prevents a server from accepting comm operations without a configured actor identity.
`build_taxonomy` is metadata/introspection-only and never dispatches a verb or reads
comm/tenant data, so there is no tenant-isolation risk here — requiring an actor identity
would make `kkernel kg validate` fail under `KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1` with no
security benefit, and an operator must be able to run taxonomy validation against a
strict-mode deployment. See `enforce_strict_actor_mode` in `crates/khive-mcp/src/serve.rs`
for the authoritative boundary definition. `KgTaxonomy` itself is `pub(super)` so
`kg::commit` (ADR-102) can reuse the same taxonomy sets rather than re-deriving them.

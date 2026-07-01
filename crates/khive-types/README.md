# khive-types

Core type primitives — `Id128`, `Timestamp`, `Namespace`, `Header` — and the
substrate data types (`Note`, `Entity`, `Link`, `Event`) that the rest of khive
operates on. `#![no_std]` (requires `alloc`), `#![forbid(unsafe_code)]`, and
depends on nothing beyond `alloc` by default.

## Features

- `std` (default) — enables `std::error::Error` impls for the crate's error types
- `serde` — `Serialize`/`Deserialize` for every type in the crate
- `blake3` — content hashing support used by downstream crates (e.g. `khive-fold` checkpoints)

## Usage

```rust
use khive_types::{Header, Id128, Namespace, Note, NoteStatus, Timestamp};

let header = Header::new(
    Id128::from_u128(1),
    Namespace::local(),
    Timestamp::from_secs(1_700_000_000),
);

let note = Note {
    header,
    kind: "observation".into(),
    status: NoteStatus::Active,
    content: "khive ships as a single binary".into(),
    properties: Default::default(),
    tags: vec!["release".into()],
    salience: Some(0.8),
    decay_factor: Some(0.01),
    expires_at: None,
    deleted_at: None,
};
assert!(note.is_valid());
```

`Id128` formats as a hyphenated UUID string and round-trips through `FromStr`.
`Namespace::parse` rejects empty, over-length, or malformed (`::`, trailing
separator) input; `Namespace::local()` returns the `"local"` namespace used by
default across the runtime.

## Substrate types

- **`Note`** — temporal-referential record (`kind`, `content`, `salience`,
  `decay_factor`, `tags`, `expires_at`); `NoteStatus` is `Active | Archived | Deleted`.
- **`Entity`** — graph node (`kind: EntityKind`, `entity_type: Option<String>`,
  `name`, `description`, `properties`, `tags`); `EntityKind` is the closed
  8-kind taxonomy (`Concept`, `Document`, `Dataset`, `Project`, `Person`, `Org`,
  `Artifact`, `Service`).
- **`Link`** — a directed, typed edge between two nodes (`source`, `target`,
  `relation: EdgeRelation`, `weight: f64` in `[0.0, 1.0]`).
  `EdgeRelation` is the closed 17-relation ontology, grouped into 9
  `EdgeCategory` values (Structure, Derivation, Provenance, Temporal,
  Dependency, Implementation, Lateral, Annotation, Epistemic).
- **`Event`** — append-only log entry (`verb`, `substrate: SubstrateKind`,
  `kind: EventKind`, `payload: EventPayload`) produced by every verb execution;
  `EventOutcome` is `Success | Denied | Error`.
- **`KhiveError`** — shared error envelope (`ErrorDomain`, `ErrorKind`,
  `ErrorCode`, `RetryHint`, `Details`) used across packs for structured errors.
- **`Pack`** trait plus `HandlerDef`, `EdgeEndpointRule`, `NoteKindSpec` — the
  pack registration contract that lets a pack declare verbs, note kinds, and
  additional edge endpoint rules.

## Where this sits

`khive-types` is the base of khive's dependency chain — every other crate
(`khive-score`, `khive-storage`, `khive-db`, every pack) depends on it, directly
or transitively. It owns the entity kind taxonomy
([ADR-001](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-001-entity-kind-taxonomy.md)),
the edge ontology
([ADR-002](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-002-edge-ontology.md)),
and the note kind taxonomy
([ADR-013](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-013-note-kind-taxonomy.md)).

## License

Apache-2.0.

# Lean mini corpus

This is a small Lake project pinned to Lean `v4.34.1`; that toolchain bundles
Lake `5.0.0`. `fixture-manifest.json` records both versions. The corpus uses
Lean's six source declaration shapes that the formal ontology must distinguish.
`example` is an anonymous goal-shaped statement, so the expected sample has a
null name and a source location. The `sorry` belongs to a **theorem**, which
stays a theorem and carries `incomplete: true`; the separately declared axiom
has `incomplete: false`, meaning that no proof is missing from an axiom
declaration. `refs` records selected source-level semantic dependencies, not
every constant introduced by elaboration.

`expected-extraction.json` is hand-authored contract data. There is no Lean
extractor or runtime verb in this fixture package. A build-host check can run
`lake build` from this directory and `cargo test -p khive-pack-formal --test
lean_mini_fixture` from the Rust workspace. The Rust test validates the sample
against its JSON Schema and matches each expected subtype to the source line.

<!--
  Canonical README template for a khive workspace crate.

  Copy this file to `crates/<crate-name>/README.md` and fill each section.
  cargo auto-detects `README.md` in a crate root, so no `readme = ` field is
  needed in Cargo.toml; the file becomes the crate's crates.io landing page.

  House style (see crates/khive-bm25/README.md and crates/khive-gate-rego/README.md
  for filled exemplars):
    - Neutral, technical, English. No marketing ("blazingly fast"), no emojis,
      no first-person, no AI attribution.
    - Ground every code example in a real public signature from the crate's
      src/lib.rs. If an exact API is uncertain, describe it in prose and name the
      core type rather than invent a code block — README snippets are NOT compiled.
    - Length is proportional to the crate: a thin trait/type crate is ~30-50
      lines; an engine or server crate is ~80-130.
    - Link sibling crates to their crates.io page and ADRs via full GitHub URLs:
      https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-NNN-<slug>.md
  Delete these comments in the filled README.
-->

# <crate-name>

<!-- One or two sentences: what the crate is and what it does. Expand the
     Cargo.toml `description` into a complete sentence. -->

<!-- OPTIONAL — a `## Features` bullet list for feature-rich crates (indexes,
     engines, servers). Omit for thin trait/type crates. -->

## Usage

<!-- A minimal, accurate example grounded in a real public signature. Keep it
     short and focused on the crate's primary entry point. -->

```rust
// grounded example — real types and methods from this crate
```

<!-- OPTIONAL domain sections as the crate warrants — match the depth of the
     exemplars: `## Configuration`, `## Semantics`, `## Architecture`,
     `## Policy contract`, etc. -->

## Where this sits

<!-- One short paragraph or a bullet list placing the crate in khive's storage
     dependency chain:

       types -> score -> storage -> db -> query -> runtime -> pack-* -> mcp

     Name the crates it builds on and the ones that consume it, and link the
     governing ADR(s) by full GitHub URL. -->

## License

Business Source License 1.1. See the repository
[LICENSE](https://github.com/ohdearquant/khive/blob/main/LICENSE).

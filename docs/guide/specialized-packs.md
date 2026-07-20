# Specialized Packs

khive's default install loads eleven production packs
(`kg, gtd, memory, comm, schedule, session, git, workspace, blob`, per
`RuntimeConfig::default()` in `crates/khive-runtime/src/config.rs`). `workspace`
registers the `workspace` entity kind and five `contains` endpoint rules only,
with no verbs; `blob` contributes content-addressed `blob.put`/`blob.get`/`blob.stat`
verbs over the `BlobStore` trait ([ADR-111](../adr/ADR-111-blob-store.md)).

Beyond the default set, khive supports niche packs that extend the graph for
a specific domain — including packs that declare zero verbs and contribute
purely to the edge ontology. Domain-specific packs of that shape (for
example, a pure-ontology extension targeting formal mathematics or source
code) are commercially licensed extensions, not part of this OSS
distribution. This guide covers how pack loading and composition work in
general, so you can write your own.

## Pack composition model

Every pack implements the `Pack` trait (`crates/khive-types/`) and declares,
additively, what it contributes: note kinds, entity kinds, verb handlers,
and edge endpoint rules. A pack can declare zero verbs and still be useful,
contributing purely to the edge ontology. Packs declare a `REQUIRES` list of
other packs that must already be loaded; the runtime resolves this at
startup. See [ADR-017](../adr/ADR-017-pack-standard.md) for the full
standard, including how pack-declared edge endpoint rules combine with the
base ADR-002 contract: rules are additive only, never tightening what the
base contract already allows.

### Loading a pack

Packs are selected via the `--pack` CLI flag (repeatable) or the
`KHIVE_PACKS` environment variable (comma- or whitespace-separated):

```bash
kkernel mcp --pack kg --pack gtd
# or
KHIVE_PACKS="kg,gtd" kkernel mcp
```

A pack that declares `REQUIRES = &["kg"]` needs `kg` in the same load set.

### Writing your own pack

A reference scaffold lives at `crates/khive-pack-template/` in the workspace
— start there when building a new pack. See
[ADR-017](../adr/ADR-017-pack-standard.md) for the `Pack` trait contract and
[ADR-023](../adr/ADR-023-declarative-pack-format.md) for the declarative
verb/note/entity/edge-rule format.

## See also

- [Knowledge Graph Modeling](knowledge-graph.md): the base entity kind and
  edge relation taxonomy that specialized packs extend.
- [Agent Sessions and Data Ingest](sessions-and-ingest.md): another optional
  pack (`session`), included in the default set but with its own opt-in
  background service.

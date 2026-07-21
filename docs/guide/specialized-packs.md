# Specialized Packs

This open-source distribution ships one production pack, `kg`, loaded by default
(per `RuntimeConfig::default()` in `crates/khive-runtime/src/config.rs`). It is the
base substrate: entities, edges, notes, graph queries, and reference resolution.

Task management, memory recall, inter-agent communication, scheduling, session
continuity, workspace linking, and content-addressed blob storage are provided by
commercially licensed extensions and are not part of this distribution. Domain-specific
ontology packs (for example, a pure-ontology extension targeting formal mathematics or
source code, which can declare zero verbs and contribute purely to the edge ontology)
are likewise commercially licensed extensions. This guide covers how pack loading and
composition work in general, so you can write your own — the `KHIVE_PACKS` mechanism
described below applies equally to the `kg` pack and to any extension pack you add.

## Pack composition model

Every pack implements the `Pack` trait (`crates/khive-types/`) and declares,
additively, what it contributes: note kinds, entity kinds, verb handlers,
and edge endpoint rules. A pack can declare zero verbs and still be useful,
contributing purely to the edge ontology. Packs declare a `REQUIRES` list of
other packs that must already be loaded; the runtime resolves this at
startup. See ADR-017 for the full
standard, including how pack-declared edge endpoint rules combine with the
base ADR-002 contract: rules are additive only, never tightening what the
base contract already allows.

### Loading a pack

Packs are selected via the `--pack` CLI flag (repeatable) or the
`KHIVE_PACKS` environment variable (comma- or whitespace-separated):

```bash
kkernel mcp --pack kg --pack <extension-name>
# or
KHIVE_PACKS="kg,<extension-name>" kkernel mcp
```

A pack that declares `REQUIRES = &["kg"]` needs `kg` in the same load set.

### Writing your own pack

A reference scaffold lives at `crates/khive-pack-template/` in the workspace
— start there when building a new pack. See
ADR-017 for the `Pack` trait contract and
ADR-023 for the declarative
verb/note/entity/edge-rule format.

## See also

- [Knowledge Graph Modeling](knowledge-graph.md): the base entity kind and
  edge relation taxonomy that specialized packs extend.
- [Agent Sessions and Data Ingest](sessions-and-ingest.md): the `session` pack, a
  commercially licensed extension with its own opt-in background service.

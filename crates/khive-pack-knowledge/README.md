# khive-pack-knowledge

Knowledge verb pack for khive: a corpus of `atoms` (documented techniques, one per
concept) grouped into `domains`, retrieved by TF-IDF search with optional embedding
rerank, and composed into markdown briefings under a token budget.

## Features

- **TF-IDF corpus search** (`knowledge.search`) over atom name/tags/content, with
  score bands (`>=0.46` reliable, `0.42-0.46` mixed, `<0.42` mostly off-target),
  query decomposition, and RRF fusion against an ANN pass when an embedder is
  configured
- **Section-level records** (`knowledge.edit`) — a closed 10-value `section_type`
  enum (`overview`, `core_model`, `formalism`, `failure_modes`, ... `other`) per
  atom, each independently disputable and adjudicable (ADR-051)
- **Budget-constrained fold** (`knowledge.fold`) — knapsack selection of
  caller-scored candidates against a token/size budget
- **Domain suggestion + compose** (`knowledge.suggest`, `knowledge.compose`) — find
  relevant domains for a query, then assemble a reranked markdown briefing from
  their member atoms
- **Concept sugar over the KG** (`knowledge.learn`, `knowledge.cite`,
  `knowledge.topic`) — register a `concept` entity and link it to its introducing
  `document`/`person`/`org` without hand-rolling `create`/`link` calls
- **Section feedback** (`knowledge.feedback`) — per-section `useful`/`not_useful`/
  `wrong` signals update posterior weights, optionally forwarded to a configured
  brain profile (ADR-032)

## Usage

This crate is not called directly as a Rust library — it registers `KnowledgePack`
with the runtime's `inventory`-based pack registry and dispatches its 19 verbs
through the MCP `request` DSL (or `kkernel exec`). A caller issues:

```text
request(ops="knowledge.search(query=\"block-max wand posting list pruning\", limit=10)")
```

The same DSL runs from the shell without an MCP client via `kkernel exec`:

```bash
kkernel exec 'knowledge.search(query="block-max wand posting list pruning", limit=10)'
```

or, to build a fresh corpus and immediately reference it:

```text
request(ops="[knowledge.upsert_atoms(atoms=[{\"slug\":\"bm25-wand\",\"name\":\"BM25 WAND\",\"content\":\"...\"}]), knowledge.compose(atom_ids=[\"bm25-wand\"], query=\"keyword search pruning strategies\")]")
```

Programmatic embedding is exposed via a small Rust API for the `kkernel reindex`
binary, independent of the MCP surface:

```rust
use khive_pack_knowledge::{reindex_knowledge, KnowledgeReindexOptions};

let opts = KnowledgeReindexOptions {
    atoms: true,
    sections: true,
    drop_existing: false,
    rebuild_ann: true,
    batch_size: None,
};
let report = reindex_knowledge(&runtime, &token, opts, None, None).await?;
```

## Verbs

| Verb                                                                              | What it does                                               |
| --------------------------------------------------------------------------------- | ---------------------------------------------------------- |
| `knowledge.upsert_atoms` / `knowledge.upsert_domains`                             | Bulk insert or update atoms / domains                      |
| `knowledge.get` / `knowledge.list` / `knowledge.delete_atoms` / `knowledge.stats` | Corpus CRUD and aggregate counts                           |
| `knowledge.index`                                                                 | Backfill embeddings + FTS for atoms/domains                |
| `knowledge.search` / `knowledge.suggest` / `knowledge.compose`                    | TF-IDF search, domain suggestion, briefing assembly        |
| `knowledge.fold`                                                                  | Knapsack selection of scored candidates against a budget   |
| `knowledge.edit`                                                                  | Upsert one atom's sections without wiping the rest         |
| `knowledge.import`                                                                | Ingest atlas-format markdown as atoms with parsed sections |
| `knowledge.challenge` / `knowledge.adjudicate`                                    | Dispute and resolve a section's content                    |
| `knowledge.learn` / `knowledge.cite` / `knowledge.topic`                          | Register/link/browse `concept` entities                    |
| `knowledge.feedback`                                                              | Apply per-section signals to posterior weights             |

All 19 verbs are `Visibility::Verb` (exposed on the agent-facing MCP surface).

## Where this sits

`khive-pack-knowledge` sits in the pack tier, above `khive-runtime` /
`khive-storage` / `khive-score` / `khive-fusion` / `khive-vamana` /
`khive-fold`, alongside sibling packs such as
[`khive-pack-kg`](https://crates.io/crates/khive-pack-kg) (a hard `REQUIRES`
dependency for the underlying `concept`/`document` entity substrate) and
[`khive-pack-brain`](https://crates.io/crates/khive-pack-brain) (feedback
target). It is one of the ten packs loaded by default in `khive-mcp`. Governing
ADRs:
[ADR-017 (Pack Standard)](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-017-pack-standard.md),
[ADR-048 (Knowledge Section Profiles)](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-048-knowledge-section-profiles.md),
[ADR-051 (Section-level Embeddings and Hybrid Compose Scoring)](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-051-section-embeddings-hybrid-compose.md).

## License

Apache-2.0.

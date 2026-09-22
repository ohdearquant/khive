# khive-pack-knowledge

Knowledge verb pack for khive: a corpus of `atoms` (documented techniques, one per
concept) grouped into `domains`, retrieved by TF-IDF search with optional embedding
rerank, and composed into markdown briefings under a token budget.

## Features

- **TF-IDF corpus search** (`knowledge.search`) over atom name/tags/content, with
  request-relative scores, per-result lexical/ANN score provenance, query
  decomposition, and RRF fusion against an ANN pass when an embedder is configured.
  Scores are not calibrated probabilities; interpret rank together with
  `score_provenance` and the response's `candidate_provenance`
- **Bounded lexical fan-out** — one 32-term allowance covers the full query and
  decomposed passes, including rarity and eligibility probes. Search reports
  `candidate_provenance.terms_truncated` when it omits terms.
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
- **Knowledge feedback** (`knowledge.feedback`) — scalar judgments on live atoms
  and domains, plus optional per-section `useful`/`not_useful`/`wrong` signals
  that update a configured/bound brain profile or namespace-local prior.

## Usage

This crate is not called directly as a Rust library — it registers `KnowledgePack`
with the runtime's `inventory`-based pack registry and dispatches its 19 verbs
through the MCP `request` DSL (or `kkernel exec`). A caller issues:

```text
request(ops="knowledge.search(query=\"block-max wand posting list pruning\", limit=10)")
```

Each `knowledge.search` result includes `score_provenance`: the contributing
`sources` (`lexical`, `ann`, or both), whether `embedding_rerank` ran successfully,
`normalization: "s_over_s_plus_1"`, and `calibrated: false`. The response's
`candidate_provenance.lexical` distinguishes `matched`, `exact_name`, `no_match`, `filtered`,
`partial_timeout`, and `timed_out`. Its `fallback` is `ann` only when returned
results have ANN evidence and none has lexical evidence; otherwise it is `none`.
A genuine lexical miss contributes no candidates from unrelated recent rows.
Queries with no scoreable terms, such as `AI`, can recover an atom through an
indexed lookup of the query's normalized slug when FTS finds no match. The probe
shares the lexical pass's remaining deadline and eligibility rules; custom slugs
outside the pack's import convention are outside this recovery guarantee.

The same DSL runs from the shell without an MCP client via `kkernel exec`:

```bash
kkernel exec 'knowledge.search(query="block-max wand posting list pruning", limit=10)'
```

or, to build a fresh corpus and immediately reference it:

```text
request(ops="[knowledge.upsert_atoms(atoms=[{\"slug\":\"bm25-wand\",\"name\":\"BM25 WAND\",\"content\":\"...\"}]), knowledge.compose(atom_ids=[\"bm25-wand\"], query=\"keyword search pruning strategies\")]")
```

To replace only an existing atom's properties, supply its complete UUID and the
new JSON value:

```text
request(ops='knowledge.upsert_atoms(atoms=[{"id":"4bed83db-6e6e-4db0-844f-00f6273035bf","properties":{"reviewed":true}}])')
```

This row form accepts exactly `id` and `properties`. It preserves stored content,
including short or empty legacy content, and every other field except `updated_at`.
Properties are replaced completely; `{}` replaces with an empty object and `null`
clears the value. The UUID must identify a live ordinary atom; missing or deleted
IDs return `NotFound`, and domains or their mirrors are refused. By-ID lookup is
namespace-agnostic and retains the atom's namespace. Ordinary slug rows still
require at least 20 words of content. The two forms can share an atomic batch:
all inputs and targets are checked before any write, and writes retain input order.

Validation and secret-gate refusals reject the entire batch without writing any
atom, including otherwise valid siblings. A secret refusal retains its typed
`SecretDetected` error and identifies the zero-based `atoms[index].field` in the
submitted payload. Slugs are not echoed because a slug can itself contain the
refused text, and multiple input rows can share a slug. Correct the identified row
and retry the batch, or submit selected rows separately; the batch verb does not
return per-record partial commits.

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

| Verb                                                                              | What it does                                                     |
| --------------------------------------------------------------------------------- | ---------------------------------------------------------------- |
| `knowledge.upsert_atoms` / `knowledge.upsert_domains`                             | Bulk insert or update atoms / domains                            |
| `knowledge.get` / `knowledge.list` / `knowledge.delete_atoms` / `knowledge.stats` | Corpus CRUD and aggregate counts                                 |
| `knowledge.index`                                                                 | Backfill embeddings (FTS rebuild is `kkernel reindex`-only)      |
| `knowledge.search` / `knowledge.suggest` / `knowledge.compose`                    | TF-IDF search, domain suggestion, briefing assembly              |
| `knowledge.fold`                                                                  | Knapsack selection of scored candidates against a budget         |
| `knowledge.edit`                                                                  | Upsert one atom's sections without wiping the rest               |
| `knowledge.import`                                                                | Validate/import atlas markdown with frontmatter or path identity |
| `knowledge.challenge` / `knowledge.adjudicate`                                    | Dispute and resolve a section's content                          |
| `knowledge.learn` / `knowledge.cite` / `knowledge.topic`                          | Register/link/browse `concept` entities                          |
| `knowledge.feedback`                                                              | Apply per-section signals to posterior weights                   |

All 19 verbs are `Visibility::Verb` (exposed on the agent-facing MCP surface).

For a cheap, stable inventory walk, call
`knowledge.list(fields=["id","slug"], after="", limit=500)` and round-trip each
non-null `next_after`. The projection is pushed into SQL, so atom `content` is
not hydrated. Cursor pages use `created_at ASC, id ASC`; legacy offset pages
retain `created_at DESC, id DESC`. The cursor is a live traversal: inserts
behind an issued boundary wait for a fresh walk, while inserts ahead may extend
the current walk without shifting or duplicating pre-existing rows. Stop when
`next_after` is null; cursor pages carry no `total`, because counting the
namespace is a full scan per page.

## Where this sits

`khive-pack-knowledge` sits in the pack tier, above `khive-runtime` /
`khive-storage` / `khive-score` / `khive-fusion` / `khive-vamana` /
`khive-fold`, alongside sibling packs such as
[`khive-pack-kg`](https://crates.io/crates/khive-pack-kg) (a hard `REQUIRES`
dependency for the underlying `concept`/`document` entity substrate) and
[`khive-pack-brain`](https://crates.io/crates/khive-pack-brain) (feedback
target). It is one of the fourteen packs loaded by default in `khive-mcp`. Governing
ADRs:
[ADR-017 (Pack Standard)](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-017-pack-standard.md),
[ADR-048 (Knowledge Section Profiles)](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-048-knowledge-section-profiles.md),
[ADR-051 (Section-level Embeddings and Hybrid Compose Scoring)](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-051-section-embeddings-hybrid-compose.md).

## License

Apache-2.0.

### Feedback targets and commit order

`knowledge.feedback` resolves `target_id` in knowledge atoms/domains using a full
UUID or unique undashed hex prefix of at least 8 characters. KG entity/note IDs
and slugs are refused. Supply `signal`, `section_signals`, or both; `target_id`
is required with a scalar signal and optional for section-only feedback. Scalar
judgments are preserved verbatim and scalar-only calls do not train sections.

Section learning and explicit/bound profile feedback require an attributed
caller. The serving profile resolves from `served_by_profile_id`, then pack
configuration, then the actor/namespace `knowledge_compose` binding. A trusted
in-process brain hook updates section weights without resolving a KG target or
inventing a scalar signal; regular `brain.feedback` retains its KG-only target
contract. Without a profile, section learning remains namespace-local.

The knowledge event commits first. Profile section learning then commits its
own public event, private learning log and snapshot atomically. These steps may
use different databases, so there is no cross-database transaction: a later hook
failure returns the committed knowledge event ID and reports the profile
outcome as unconfirmed. Inspect that event before retrying; an error does not
mean the knowledge judgment was rolled back.

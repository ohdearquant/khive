# khive knowledge plugin

Structured knowledge management on top of [khive-mcp](https://github.com/ohdearquant/khive).

The knowledge pack provides verbs for managing research concepts and knowledge atoms — all built
on the kg substrate without duplicating storage. This README covers the 10 most commonly used
verbs. Run `verbs(pack="knowledge")` for the full surface (18 verbs total).

## Why this pack exists

The `kg` pack gives you direct CRUD for any entity kind (`create`, `link`, `search`). The knowledge
pack adds **opinionated sugar** for two distinct patterns:

**Concept management** (built on the kg substrate):

- `learn` = `create(kind="concept")` with automatic `domain` → tag promotion (makes domain
  filterable via FTS and the `domain=` parameter on `topic`).
- `cite` = `link(relation="introduced_by")` with weight clamped to [0, 1] and cleaner parameter
  names (`concept_id` / `source_id` instead of `source_id` / `target_id`).
- `topic` = `search(kind="concept")` with optional post-filter on the domain tag.

**Atom management** (lore/knowledge atoms, distinct from the KG entity store):

- `upsert_atoms`, `edit`, `list`, `get`, `delete_atoms` — CRUD for structured knowledge atoms
  (markdown-chunked documents with sections).
- `search` — hybrid FTS + embedding search with optional reranking.
- `stats` — counts across atoms, domains, and embeddings.

Use the knowledge pack when you want auto-promotion for concepts or atom-based document storage.
Use `kg` verbs directly when you need other entity kinds, relations, or full parameter control.

## Verbs

All verbs are dispatched through the single MCP `request` tool.

### Concept management

| Verb              | Params                                                                                                                    | What it does                                                                                                                    |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| `knowledge.learn` | `name` (required), `description?`, `domain?`, `tags?`                                                                     | Register a concept entity. `domain` is stored in `properties.domain` and automatically added to `tags` for FTS discoverability. |
| `knowledge.cite`  | `concept_id` (required UUID), `source_id` (required UUID, must be `document` or `person`), `weight?` (float, default 1.0) | Create an `introduced_by` edge from a concept to its source. `weight` clamped to [0.0, 1.0].                                    |
| `knowledge.topic` | `domain?`, `query?`, `limit?` (default 20, max 100)                                                                       | List or search concept entities, optionally filtered by domain tag.                                                             |

### Atom management

| Verb                     | Params                                                                                                            | What it does                                                                                                                  |
| ------------------------ | ----------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| `knowledge.upsert_atoms` | `atoms` (required, array of `{slug, name, content, description?, tags?, properties?, finalized?}`), `chunk_size?` | Create or update knowledge atoms. Field is `content` (not `body`).                                                            |
| `knowledge.edit`         | `id` (required, UUID or slug), `sections` (required, array of `{section_type, content, heading?, sort_order?}`)   | Upsert sections within an atom. Sections are identified by `section_type`; existing sections with matching type are replaced. |
| `knowledge.list`         | `type?` (`"atom"` or `"domain"`, default `"atom"`), `limit?` (default 20, max 500), `offset?`                     | Paginate atoms or domains.                                                                                                    |
| `knowledge.get`          | `id` (required, UUID or slug)                                                                                     | Fetch a single atom or domain by UUID or slug. Short UUID prefix is NOT supported — use full UUID or slug.                    |
| `knowledge.delete_atoms` | `ids` (required, array of slugs or UUIDs)                                                                         | Delete atoms by slug or UUID. Param is `ids` (not `slugs`).                                                                   |

### Search and retrieval

| Verb               | Params                                                                                                                                                                                                                                                  | What it does                                                                                                                                                 |
| ------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `knowledge.search` | `query` (required), `type?` (`"atom"` or `"domain"`), `role?`, `limit?` (default 10, max 100), `min_score?`, `weights?`, `decompose?` (boolean), `decompose_threshold?`, `intersection_bonus?`, `rerank?` (default true), `rerank_alpha?` (default 0.7) | Hybrid FTS + embedding search over atoms/domains. `rerank=true` blends TF-IDF and embedding scores via `rerank_alpha` (0 = pure embedding, 1 = pure TF-IDF). |
| `knowledge.stats`  | (none)                                                                                                                                                                                                                                                  | Atom, domain, and embedding counts.                                                                                                                          |

### Suggest and compose (ADR-051)

| Verb                | Params                                                                                                  | What it does                                                                                                                                                                                                                                  |
| ------------------- | ------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `knowledge.suggest` | `query` (required, **5+ words**), `role?`, `limit?` (default 8)                                         | Domain suggestion via FTS + Vamana ANN + embedding rerank. Returns domain IDs, names, and scores. Short queries cause disambiguation — use 5–12 words for best results.                                                                       |
| `knowledge.compose` | `query` (required), `domain_ids?`, `atom_ids?`, `max_tokens?` (default 8000), `auto_limit?` (default 5) | Compose a knowledge briefing from domains/atoms with section-level hybrid scoring (ADR-051). **Auto-compose**: omit `domain_ids`/`atom_ids` to run suggest internally (query must be **10+ words**). Explicit IDs accept any non-empty query. |
| `knowledge.index`   | `ids?` (slugs/UUIDs), `batch_size?` (default 500), `rebuild_ann?`, `insert_only?`                       | Embed atoms + build Vamana ANN. Without `ids`, indexes the full corpus.                                                                                                                                                                       |
| `knowledge.fold`    | `candidates` (array of `{id, score, size}`), `budget` (tokens)                                          | Knapsack selection — pack candidates into a token budget by score/size ratio.                                                                                                                                                                 |

#### Compose scoring formula

```
0.55 · cosine(query, section_embedding)
0.20 · bm25(query, heading + content)
0.10 · cosine(query, atom_embedding)
0.10 · domain_membership
0.05 · section_type_weight
```

Sections without embeddings score via BM25 + atom cosine + domain + type (section_cosine = 0).

#### Query length guidelines

| Verb                     | Minimum  | Sweet spot  | Rationale                                                                       |
| ------------------------ | -------- | ----------- | ------------------------------------------------------------------------------- |
| `suggest`                | 5 words  | 5–12 words  | Short queries cause disambiguation (e.g. "attention" matches ML and psychology) |
| `compose` (auto)         | 10 words | 15–30 words | Longer queries improve section ranking score spread                             |
| `compose` (explicit IDs) | 1 word   | any         | IDs already select the scope; query only ranks sections                         |

## Skills

- **learn** — register a concept with domain and tags.
- **cite** — create a provenance-tracked citation from a concept to its source.
- **topic** — browse concepts by domain or free-text query.

## Install

`kkernel` ships with the knowledge pack bundled. The knowledge pack requires the `kg` pack as a
dependency — both must be listed explicitly when launching the server:

```bash
cargo install kkernel

# Claude Code
claude mcp add --transport stdio khive -- kkernel mcp --pack kg --pack knowledge
```

Or using the `KHIVE_PACKS` environment variable:

```bash
claude mcp add --transport stdio khive -- env KHIVE_PACKS=kg,knowledge kkernel mcp
```

Or add to `.mcp.json`:

```json
{
  "mcpServers": {
    "khive": {
      "command": "kkernel",
      "args": ["mcp", "--pack", "kg", "--pack", "knowledge"]
    }
  }
}
```

## Plugin install

```bash
/plugin marketplace add ohdearquant/khive
/plugin install knowledge
```

## Presentation modes (ADR-045)

The `request` tool accepts an optional `presentation` field per op: `agent` (default,
token-efficient), `verbose` (canonical full JSON), or `human` (same as `verbose` over MCP). Agents
should use the default `agent` mode.

## License

Apache-2.0

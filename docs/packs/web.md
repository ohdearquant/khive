# Web pack

The optional `web` pack maps a local site's ARW manifest and markdown machine views to a dedicated
knowledge graph database. It reads declarations as supplied; it does not fetch URLs, parse HTML,
infer protocols, or calculate quality scores.

Load it with `KHIVE_PACKS=kg,web` or add `--pack web` when starting the MCP server. The pack requires
`kg` and is outside the default pack set. Its only verb is `web.ingest`.

## Vocabulary

| Entity type    | Base kind  | Alias            | Represents                                  |
| -------------- | ---------- | ---------------- | ------------------------------------------- |
| `site`         | `service`  | `origin`         | An origin and its declared site metadata    |
| `page`         | `document` | `web_page`       | A declared content entry                    |
| `machine_view` | `document` | `view`           | A page's markdown rendering and frontmatter |
| `agent_tool`   | `service`  | `mcp_tool`       | A declared callable tool                    |
| `agent_skill`  | `document` | `skill_manifest` | A declared skill and its required tools     |

These subtype tokens validate through `create`, including aliases, even when the web pack is not
loaded. For example, `create(kind="service", entity_type="site", name="Meadow Archive")` creates a
site. The existing `tool` subtype belongs to `project`; it is not a service subtype.

| Source                  | Relation       | Target                 |
| ----------------------- | -------------- | ---------------------- |
| `service/site`          | `contains`     | `document/page`        |
| `service/site`          | `contains`     | `service/agent_tool`   |
| `service/site`          | `contains`     | `document/agent_skill` |
| `document/machine_view` | `derived_from` | `document/page`        |
| `document/agent_skill`  | `depends_on`   | `service/agent_tool`   |
| `service/site`          | `implements`   | `concept/interface`    |

The rules appear in `link(help=true)` when the web pack is loaded. Derivation between documents and
implementation from services to concepts are already permitted by the base contract; these two rows
also describe the web vocabulary. Pack rules add permissions and cannot narrow the base contract.
Consequently, a caller can link a page as derived from a view, while ingestion emits only the
view-to-page direction. `site contains site` has no matching rule.

## Ingest a site tree

```text
web.ingest(source="./sites/meadow")
web.ingest(source="./sites/meadow", db="./maps/meadow.db", include_views=false)
web.ingest(help=true)
```

| Parameter       | Required | Default                      | Meaning                                                    |
| --------------- | -------- | ---------------------------- | ---------------------------------------------------------- |
| `source`        | Yes      | —                            | Local directory containing one origin's served tree        |
| `db`            | No       | `<source>/.khive/web-map.db` | Dedicated target map database                              |
| `include_views` | No       | `true`                       | Read the declared markdown views and create their entities |

Unknown parameters are rejected. The target cannot be the shared production database or the
calling runtime's database, including a configured non-default production location. There is no
override for this refusal.
Database targets use filesystem paths; SQLite `file:` URI spellings are rejected before opening.

The scanner reads `.well-known/arw.json` first. An absent manifest returns `manifest_missing`;
malformed manifest JSON returns `manifest_malformed`. Both refuse the whole site before writing map
records. Version, profile, site, content signals, content entries, tools, and skills are read from the
manifest's `version`, `profile`, `site`, `content_signals`, `content`, `tools`, and `skills` keys.
Other top-level keys are ignored and counted in `ignored_keys`. In particular, `integrations` does
not create protocol entities or `implements` edges in v0. The scanner reads the supported fields
without running full JSON Schema validation; unreadable declarations are quarantined with a reason.

If `llms.txt` exists, its embedded YAML declarations are checked against the manifest. Each
disagreement records the affected field and a reason in `quarantined`; the manifest value wins.
Each content entry's optional `markdown_url` names a local markdown view. Its frontmatter supplies
properties such as `page_type`, `schema_org_type`, and `aeo`. A missing view leaves its page in the
map and increments `views_missing`, without quarantine. Setting `include_views=false` skips view
files entirely and produces no machine-view entities or derivation edges for that ingest.

Ingestion preserves declared descriptions, tags, chunks, content signals, and frontmatter values.
The view's declared `aeo.domain` supplies its discipline and page tags supply subdiscipline.
Scores and reading-ease values remain publisher declarations. Tool and skill declarations come from
the manifest; endpoint URLs and skill documents are not fetched.

Identifiers use UUIDv5 with separate keys for each subtype. Origin identity is the lowercased host
from `site.homepage`, with scheme and port removed. Page and view paths have one leading slash and
no trailing slash. Tool and skill names remain as declared. Re-ingesting unchanged declarations
preserves the map rows; changed descriptions update the existing identities.
An ingest accepts at most 10,000 entities and 50,000 edges. Validation and statement preparation
finish before the atomic map write begins.

## Result

The verb returns a report, without writing a report side file. For a site with two pages, two views,
two tools, and one skill requiring both tools, the counts are:

```json
{
  "entity_counts": {
    "site": 1,
    "page": 2,
    "machine_view": 2,
    "agent_tool": 2,
    "agent_skill": 1
  },
  "relation_counts": {
    "contains": 5,
    "derived_from": 2,
    "depends_on": 2,
    "implements": 0
  },
  "views_missing": 0,
  "quarantined": [],
  "manifest_digest": "<64 lowercase hexadecimal characters>",
  "source": "/srv/sites/meadow",
  "db_path": "/srv/sites/meadow/.khive/web-map.db",
  "include_views": true,
  "ignored_keys": 0
}
```

All five subtype keys and all four relation keys are present, including zero counts.
`manifest_digest` is BLAKE3 over the raw manifest file bytes, rendered as lowercase hexadecimal;
the placeholder above represents that value. `quarantined` entries have the shape
`{"field":"site.name","reason":"…"}`. `ignored_keys` counts unconsumed top-level manifest keys,
not their nested declarations.

## Multiple sites

Place the served trees at `./sites/meadow` and `./sites/fern`. This ten-line shell example ingests
each into its own default map database. Python encodes the operation so paths remain correctly
quoted.

```sh
set -eu
command -v kkernel >/dev/null
command -v python3 >/dev/null
export KHIVE_PACKS=kg,web
site_root=./sites
for site_source in "$site_root/meadow" "$site_root/fern"; do
  test -d "$site_source"
  ops=$(python3 -c 'import json,sys; print(json.dumps([{"tool":"web.ingest","args":{"source":sys.argv[1]}}]))' "$site_source")
  kkernel exec "$ops"
done
```

The domain vocabulary and ingest contract are described in
[ADR-175](../adr/ADR-175-web-pack.md).

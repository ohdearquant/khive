# Web pack

The optional `web` pack fetches, extracts, ingests, searches, and refreshes web content into the
knowledge graph under an egress policy (address-class checks, an optional host allowlist,
scoped credentials, a bounded request-header set, and byte/time/result-count ceilings). It never
depends on a manifest declared by the target site — anything a plain HTTP(S) fetch can read is
in scope.

Load it with `KHIVE_PACKS=kg,web` or pass `--pack kg --pack web` when starting the MCP server.
The pack requires `kg` and is outside the default pack set.

## Ontology

| Entity type | Base kind  | Identity                                        | Represents                                       |
| ----------- | ---------- | ----------------------------------------------- | ------------------------------------------------ |
| `site`      | `service`  | `(scheme, host, port)`                          | An origin — the alias `origin` also validates    |
| `page`      | `document` | `(site, canonical path+query)`                  | A fetched document whose body is HTML/XHTML      |
| `resource`  | `document` | `(site, canonical path+query)` — same as `page` | A fetched, or discovered-but-unfetched, document |

`page` and `resource` share one identity formula on purpose: a `resource` discovered as a link
target (`status: null`, unfetched) re-types to `page` **in place**, at the same id, the moment
`fetch`/`ingest`/`refresh` retrieves it and finds an HTML body. Identity is by address, never by
subtype — nothing about a document's id encodes whether it has been fetched yet or what kind of
body it turned out to have.

Canonicalization (applied before any identity computation): scheme and host are lowercased, the
default port for the scheme is dropped, the path is percent-normalized, raw query pairs are
stably sorted by their raw key bytes, and the fragment is dropped entirely. Query pairs are
never form-decoded or re-encoded: `%FF` and `%FE`, `a+b` and `a%20b`, and `flag` and `flag=`
remain distinct. Sorting still makes `https://Example.com/a?b=1&a=2#x` and
`https://example.com/a?a=2&b=1` the same resource. Equal-key pairs retain their order.

The document's `url` property keeps the actual parsed request address, with its fragment
removed, independently of the sorted identity key. Refresh uses that address with its
original query spelling and order. Newly fetched representations update this property;
existing rows that only retain a previously rewritten address cannot recover its original
spelling without being fetched again.

### Edge rules

Two pack-declared rows — everything else this pack's verbs produce is already legal under the
base 17-relation contract and needs no addition:

| Source         | Relation   | Target              | Written by                                             |
| -------------- | ---------- | ------------------- | ------------------------------------------------------ |
| `service/site` | `contains` | `document/page`     | fetch, ingest, refresh                                 |
| `service/site` | `contains` | `document/resource` | fetch, ingest, refresh, extract (sitemap/feed entries) |

Base rows this pack's verbs also produce, needing no pack declaration:

| Source               | Relation       | Target                    | Meaning                                                                   |
| -------------------- | -------------- | ------------------------- | ------------------------------------------------------------------------- |
| `document/page`      | `links_to`     | `document/page\|resource` | a hyperlink found by `extract`                                            |
| `document/resource`  | `derived_from` | `document/page`           | text extracted from a page (`extract`'s `text` kind)                      |
| `document`           | `supersedes`   | `document`                | a permanent redirect (301/308): the old address stops being authoritative |
| `note` (observation) | `annotates`    | any                       | a fetch/search/refresh receipt                                            |
| `note` (observation) | `supersedes`   | `note`                    | the receipt chain: the history of one resource's fetches                  |

## Verbs

### `web.fetch(url, accept?, persist?, max_bytes?, timeout_s?, method?, headers?, credential?, namespace?)`

Fetch one URL under egress policy. Follows up to 5 redirects; a 301/308 hop mints both ends of
the hop and links `new supersedes old`, a 302/307 hop mints nothing beyond a receipt entry naming
it. The terminal hop's body (if `GET`; `HEAD` carries none) is stored via the runtime's blob
store, content-addressed; storing byte-identical content again is a no-op. `persist` defaults to
`true`; `false` stores no body or entities and returns the exact body as a standard-alphabet,
padded base64 string in `body` (null for HEAD or a persisted fetch). It still writes a standalone receipt recording
`final_url`, the BLAKE3 `content_digest`, `size` and RFC 3339 `fetched_at`; `content_ref` is null.
A persisted body has one `content` attachment on its entity, on the main backend even when web
records use a separate backend. Receipts never carry body attachments.
The egress `max_bytes` ceiling bounds raw bytes; base64 uses `4 * ceil(bytes / 3)` characters.
For a transient GET, the effective `max_bytes` (including an omitted argument's configured
default) must be at most **6,288,384 raw bytes**. A higher value is refused as invalid input
before DNS or network access, with no receipt; lower `max_bytes` or use `persist=true`.
HEAD has no inline body and is exempt from this extra ceiling; persisted fetches retain their
configured egress ceiling.

The limit leaves 4 KiB below the 8 MiB daemon frame cap for base64 body encoding. Settlement
checks both that encoded-body limit and the complete serialized verb result, including the
actual URL and allowed response headers. The result may use up to 8 MiB minus 1 KiB, leaving
3 KiB beyond the body budget for verb metadata and 1 KiB for the outer transport envelope.
Large or heavily escaped metadata can therefore refuse an otherwise valid body before its
receipt is written. The transport still checks the final frame, including responses that
aggregate multiple operations.
`entity_type` is decided from the response `content-type`: `text/html`/`application/xhtml+xml`
(ignoring `; charset=...` and case) is `page`, everything else is `resource`.

### `web.extract(id | url, kinds?, namespace?, link_limit?)`

Parse an already-fetched body — never fetches one itself. `kinds` is a subset of
`{text, links, sitemap, feed}`, defaulting to whatever applies to the stored content-type.
`links` yields `page links_to page|resource` edges to targets minted (if absent) as unfetched
`resource` rows. `link_limit` caps link targets processed from one page. It defaults to 100 and accepts integers
from 0 through 1,000. Once the ceiling is reached, remaining matched `<a href>` attributes are
left unresolved and their count is returned as `result.links.skipped`;
`result.links.edges_created` reports the processed targets. `sitemap`/`feed` yield
`site contains resource` edges for each entry under the _publishing_ site. `text` mints a
`resource` holding the tag-stripped body, linked
`derived_from` back to the original — keyed by the original document's id, so repeated
extraction converges on one row rather than minting duplicates. Refuses `not_fetched` on a
document with no stored body.

### `web.ingest(source, origin?, depth?, limit?, namespace?, extract_links?)`

Fetch and extract over a single URL, a JSON array of URLs, or — with `origin` given — a
directory on disk laid out as `origin` would serve it (`origin` then supplies the `site`
identity for every file in the tree, and no network request is made for the disk case).
`depth` bounds how many hops of `links`-extracted targets are followed beyond the seed URLs
(default `0`: seeds only). In URL mode, depth zero does not extract links by default; set
`extract_links=true` to record the page's links without following them. Positive depth extracts
links only on pages below the requested depth so their targets can be followed. Across the whole
call, link targets processed are capped by `limit`, and each page also uses the `web.extract`
default ceiling of 100. `limit` bounds the total number of documents ingested in one call
(default 100).

The disk-mode `source` directory is confined to the operator's configured `[web] read_roots`
(modeled on `[exec] read_roots`): absent or empty refuses every disk ingest outright, and a
`source` that is not itself one of the configured roots or nested under one is refused before
anything is read. Every path discovered while walking the tree — including a symlink target —
is re-canonicalized and re-checked against the same roots, so a symlink planted inside an
allowed root cannot serve content from outside it.

The URL-crawl mode's reply carries `ingested` (the minted document ids) and `refused` (one
`{url, error}` entry per URL that `web.fetch` refused — an egress refusal, a transport error, or
anything else short of a persisted document): a refused URL is named in the reply rather than
silently dropped from the crawl, so a caller can tell "nothing matched" apart from "some targets
were refused."

### Deleting routed entities

Web entities are stored on the web backend while their body attachments are rooted on the main
backend. A hard-delete retry that finds the entity row already absent can remove a remaining main
backend attachment. Its response has `deleted: false` and `attachment_cleanup: true`. `deleted`
reports whether this call removed the entity row; attachment-only cleanup does not establish
whether an earlier attempt completed its index cleanup or appended its entity deletion event.

### `web.search(query, provider?, limit?, persist?)`

Query a configured `[[web.search_providers]]` entry — a `Fixture` (canned results, for tests and
offline configurations) or an `Http` provider (a templated URL plus an optional bearer-token
environment variable). No provider configured, or an ambiguous unnamed selection among several
non-default providers, refuses `no_search_provider_configured`/`search_provider_not_configured`
rather than returning an empty list. The receipt preserves the exact ordered result array and a
BLAKE3 digest over it. `persist` defaults to `false`; `true` mints each hit's URL as an unfetched
`resource` under its site, same as an unvisited `extract(links)` target.

### `web.refresh(id)`

Conditionally re-fetch an already-fetched document using its stored `etag`/`last_modified` as
`If-None-Match`/`If-Modified-Since`. A `304`, or a `200` whose body content-addresses to the
_same_ reference already stored, writes a receipt only — no entity or blob change. A genuinely
changed body puts the new blob and patches the entity in place. Every refresh's receipt
supersedes the immediately prior receipt for the same document, so the note history is the
resource's refresh timeline. Follows the same bounded redirect chain `fetch` does, through the
same egress checks on every hop: identity is by address, so the terminal address's own row
receives the body on a redirect, the entity the caller asked to refresh keeps its own recorded
`url`, and the reply's `final_id` names whichever row actually received the content.

No `db`/`target` parameter exists anywhere on this pack's verb surface — every write lands in
the caller's own namespace through the runtime's ordinary create/update/link seam, the same seam
every other pack's handlers use.

## Egress policy (`[web]` config section)

```toml
[web]
timeout_default_s = 30       # optional; built-in default shown
timeout_max_s = 120
max_bytes_default = 5242880  # 5 MiB
max_bytes_max = 52428800     # 50 MiB
search_limit_default = 10
search_limit_max = 50
read_roots = ["/srv/ingest-sources"]  # web.ingest disk mode; absent/empty refuses disk ingest entirely

[[web.allowlist]]
host = "example.com"         # with any [[web.allowlist]] entry present, only listed hosts are reachable

[[web.credentials]]
name = "example-api"
env_var = "EXAMPLE_API_TOKEN"
hosts = ["api.example.com"]  # exact match or a subdomain of the entry; IP-literal entries match only that exact address

[[web.search_providers]]
name = "canned"
default = true
[[web.search_providers.results]]
title = "..."
url = "..."
snippet = "..."
```

Refused unconditionally, regardless of configuration: any scheme other than `http`/`https`; any
URL carrying `user:password@` userinfo; loopback, link-local, private, CGNAT
(`100.64.0.0/10`), multicast, broadcast, and unspecified addresses (checked against the
_resolved_ address, re-resolved and re-checked immediately before connecting, refusing on any
disagreement between the two — a DNS-rebinding defense); the `Authorization`/`Cookie`/
`Proxy-Authorization` request headers (send a scoped `credential` instead); a caller-supplied
`max_bytes`/`timeout_s`/search `limit` above the operator's configured ceiling. A credential
requires `https` at every hop.

## Receipts

Every `fetch`/`refresh`, and every `search` with `persist=true` or a hit, writes one
`observation` note annotating the entity (or entities) it touched, carrying the request record
(method, final URL, status, allow-listed response headers, byte count, redirect chain) or
(for `search`) the query, provider, effective limit, and the ordered result array plus its
BLAKE3 digest. A note write that the secret-detection gate refuses (a result title containing
what looks like a credential, for example) leaves no partial receipt behind — but is not itself
atomic with the entity/blob write that precedes it in the same call: `fetch`/`refresh` mint or
patch the entity and put the blob first, and only then write the receipt, so a refused receipt
on an otherwise-successful fetch leaves the entity/blob change in place with no corresponding
observation note.

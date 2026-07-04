# ADR-090: Docs site standard — navigation, agent md/txt surfaces, visual style

**Status**: Proposed
**Date**: 2026-07-04
**Depends on**: none (governs `.github/workflows/pages.yml` and `docs/guide/*.md`)

## Context

khive publishes a public GitHub Pages docs site (`https://ohdearquant.github.io/khive`),
built by `.github/workflows/pages.yml` from `docs/guide/*.md` via Jekyll's
`remote_theme: just-the-docs`. The site serves two distinct audiences from one source: humans
browsing the HTML site, and agents fetching machine-readable summaries (`llms.txt` and raw
markdown). Ocean's bar for the site: match the OpenAI docs standard — easy to navigate, clean,
uncluttered — and keep agent-facing content hosted as md/txt, not HTML.

Two problems surfaced on the live site and were fixed in PR #582 (following the site's initial
build in PR #579):

1. **Sidebar duplication.** The raw agent-fetchable `/md/*.md` copies were staged inside the
   Jekyll source tree (`$SRC/md/`). The `github-pages` gem bundles
   `jekyll-optional-front-matter` and `jekyll-titles-from-headings`, which promoted each raw
   markdown file into its own titled page, doubling every sidebar entry.
2. **Default theme color.** `just-the-docs` defaults to a purple accent family, which does not
   match khive's visual identity and reads as generic theme boilerplate rather than a
   deliberately designed site.

Both fixes are mechanical and already implemented; what has been missing is a written standard
that says why the site is shaped this way, so future changes to navigation, color, or the
agent surface do not regress these decisions or drift the two audiences apart. This ADR
codifies that standard, retroactively covering the decisions in PR #579 and PR #582.

## Decision

### 1. Audience split, single source of truth

The site serves two audiences from one generation step in `pages.yml`:

- **Humans** get the Jekyll-built HTML site: sidebar navigation, built-in search, standard
  typography.
- **Agents** get md/txt-first surfaces (below), generated in the same workflow run from the
  same `docs/guide/*.md` files.

Both outputs are produced by one workflow triggered on the same push. There is no separate
agent-docs source tree and no second generation path — a new or edited guide page appears in
both audiences' surfaces automatically, and the two can never drift apart because they are not
independently maintained.

### 2. Agent surfaces (normative)

The workflow MUST produce all of the following, all agent-fetchable at the site root:

| Path             | Contents                                                                                                            |
| ---------------- | ------------------------------------------------------------------------------------------------------------------- |
| `/llms.txt`      | Short project summary plus a linked index of every doc page (the llms.txt convention)                               |
| `/llm.txt`       | Alias of `/llms.txt` — agents and humans guess both spellings; both must resolve                                    |
| `/llms-full.txt` | Every doc page, concatenated into one file, for a single-fetch full read                                            |
| `/md/<page>.md`  | Byte-identical raw markdown copy of each `docs/guide/<page>.md`, linked from the bottom of every rendered HTML page |

The workflow loops over `docs/guide/*.md` to build the `NAV_ORDER`-driven HTML pages, the
`llms.txt`/`llms-full.txt` indexes, and the `/md/` copies from the same iteration. Adding a new
guide page therefore makes it appear in the HTML sidebar (subject to §3), the `llms.txt` index,
`llms-full.txt`, and its own `/md/<page>.md` copy without any change beyond adding the file and
(per §3) a `NAV_ORDER` entry.

Raw `/md/<page>.md` files MUST be staged outside the Jekyll source tree during the build
(`_raw_md/`, not `$SRC/md/`) and copied into the built site (`_site/md/`) in a step that runs
**after** the Jekyll build step. Jekyll's optional-front-matter and titles-from-headings
plugins promote any markdown file present in the source tree into a titled page; staging raw
copies inside the source is the root cause of the sidebar-duplication defect this ADR closes,
and MUST NOT recur.

### 3. Information architecture (OpenAI-docs bar)

- **Single flat sidebar.** At most one nav entry per rendered page. Raw `/md/` copies are never
  present in the Jekyll source tree (per §2), so they can never contribute a duplicate entry.
- **Quickstart-first ordering.** Getting Started is `nav_order: 2`, immediately after Home
  (`nav_order: 1`). Reference material (API Reference) sorts last.
- **Task-oriented titles.** Page titles name what the reader does or learns ("Getting
  Started", "Knowledge Graph Modeling"), not internal component names.
- **`NAV_ORDER` is the sidebar contract.** The `NAV_ORDER` associative array in `pages.yml` is
  the single authoritative mapping from `docs/guide/<page>.md` basename to `nav_order`, title,
  and one-line description. A guide page added without a corresponding `NAV_ORDER` entry falls
  through to `nav_order: 99` with a title derived from its first heading — this is a defect to
  fix, not an acceptable steady state, since it also means the page has no curated description
  for `llms.txt`.
- **Built-in search stays enabled** (`search_enabled: true`) — it is the primary in-page
  navigation aid `just-the-docs` provides and must not be disabled to "simplify" the site.
- **Homepage is overview, not marketing.** `index.md` carries: a one-paragraph project
  description, the agent-surface pointers (§2), install instructions, and the documentation
  index. No pricing, testimonials, or promotional copy — content-first, matching the OpenAI
  docs homepage pattern.

### 4. Visual standard

- **Custom `khive` color scheme**, activated via `color_scheme: khive` in `_config.yml` and
  defined in `_sass/color_schemes/khive.scss`, staged inside the Jekyll source so
  `just-the-docs` picks it up as a theme variant.
- **Neutral blue accent family** (`#0969da` and its light/dark variants), overriding the
  theme's default purple palette (`$purple-*`) plus the derived `$link-color` and
  `$btn-primary-color`, which the theme's light scheme computes from the purple palette at its
  own load time and must be re-declared explicitly. The site MUST NOT ship with the theme's
  default purple accent.
- **Restrained styling.** No custom layout beyond the theme's default page/sidebar structure,
  no added chrome, no decorative elements. Generous whitespace and prominent code blocks are
  inherited from `just-the-docs` defaults and are not to be overridden — the content is the
  interface, matching the OpenAI docs content-first aesthetic.

### 5. Page contract

Every `docs/guide/*.md` page is transformed, at build time, into a site page carrying:

- Front matter (`layout`, `title`, `nav_order`, and `description` when the page has a
  `NAV_ORDER` entry) injected by the workflow — authors do not hand-write front matter into
  guide source files.
- A raw-markdown footer link (`Raw markdown for this page: /md/<page>.md`) on every rendered
  page.
- A one-line description sourced from the same `NAV_ORDER` entry, reused verbatim in the
  page's front matter `description` and in its `llms.txt` index entry, so the two surfaces
  never present conflicting summaries of the same page.

### 6. Linkage

- `README.md` carries a Docs badge and a Discord badge in the existing badge row, plus a
  Documentation-and-Discord line near the top of the file.
- The site header `aux_links` include, at minimum: GitHub, crates.io, Discord, and
  `llms.txt` — giving a human landing on any page a one-click path to the source repo, the
  published crate, the community channel, and the agent-facing index.

### 7. Change control

Changes to the information architecture (§3), the color scheme (§4), or the shape of the
agent-facing surface (§2 — adding, removing, or renaming one of `llms.txt` / `llm.txt` /
`llms-full.txt` / `/md/<page>.md`) require amending this ADR. Adding a guide page under the
existing `NAV_ORDER` scheme, or updating page content, does not.

## Alternatives considered

1. **Bespoke static-site framework (Mintlify, Docusaurus, Fumadocs).** These frameworks give
   more layout control and closer visual parity with the OpenAI docs product itself, but each
   requires an npm/Node toolchain and its own build/deploy pipeline. khive ships a single Rust
   binary with zero Node dependency anywhere in the repo; adding one solely for the docs site
   would be a new build-time dependency for a project that otherwise has none. Rejected.
2. **Serve agent docs as HTML with content negotiation** (agents get markdown via `Accept`
   headers or a `?format=md` query param on the same URL humans use). GitHub Pages is a static
   host with no server-side content negotiation, so this would require a proxy or a different
   hosting target. `llms.txt` and per-page raw markdown at predictable, stable paths is an
   emerging, simple convention that agents already know to probe for; it needs no negotiation
   logic and no additional hosting. Rejected.
3. **Keep the theme default purple accent.** Zero-effort, but it reads as unstyled theme
   boilerplate rather than a site khive deliberately designed, and does not meet Ocean's
   "clean, uncluttered, considered" bar. Rejected.

## Consequences

- `remote_theme: just-the-docs` constrains the site to that theme's layout primitives (sidebar,
  header, `aux_links`, color-scheme SCSS variables). This ADR adopts the OpenAI docs
  _principles_ — flat navigation, quickstart-first ordering, content-first styling, one entry
  per page — within that theme, not a reimplementation of OpenAI's own (non-public) docs
  framework or stack.
- Because generation is a single workflow (`pages.yml`) triggered on push to `docs/guide/**`
  (or `README.md`, or the workflow file itself), docs changes ship only through that path.
  There is no local preview step in this ADR's scope; authors verify by reading the rendered
  Markdown and, for structural changes to the workflow, by running the assembly script's shell
  logic locally before pushing (as done for PR #579 and PR #582).
- The `NAV_ORDER` table is bash-associative-array state embedded in a YAML workflow file. This
  is intentionally low-tech (no static-site generator config format, no separate data file) to
  keep the zero-Node-toolchain property; the cost is that `NAV_ORDER` and the per-page loop
  logic live in a shell heredoc rather than a typed config. Acceptable at the current page
  count (7 guide pages); if the page count grows enough that this becomes unwieldy, an ADR
  amendment can move `NAV_ORDER` to a data file without changing any of the standards in §2-§6.
- Raw markdown staged outside the Jekyll source (`_raw_md/` → post-build copy into `_site/md/`)
  adds one additional workflow step after the Jekyll build, with existence assertions
  (`test -f _site/md/<page>.md`) guarding against silent breakage of the agent surface.

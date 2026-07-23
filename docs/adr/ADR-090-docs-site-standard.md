# ADR-090: Docs site standard for navigation, machine-readable surfaces, and visual style

**Status**: Accepted
**Date**: 2026-07-04
**Depends on**: none (governs `.github/workflows/pages.yml` and `docs/guide/*.md`)

## Context

khive publishes a public GitHub Pages docs site (`https://ohdearquant.github.io/khive`),
built by `.github/workflows/pages.yml` from `docs/guide/*.md` via Jekyll's
`remote_theme: just-the-docs`. The site serves interactive readers and automated documentation
consumers from one source. HTML provides navigation and search, while `llms.txt` and raw
Markdown provide stable machine-readable representations. The site must use a clear information
hierarchy, restrained styling, accessible navigation, and predictable content URLs.

Two implementation constraints shape the site:

1. **Sidebar duplication.** The raw `/md/*.md` copies were staged inside the
   Jekyll source tree (`$SRC/md/`). The `github-pages` gem bundles
   `jekyll-optional-front-matter` and `jekyll-titles-from-headings`, which promoted each raw
   markdown file into its own titled page, doubling every sidebar entry.
2. **Default theme color.** `just-the-docs` defaults to a purple accent family, which does not
   match khive's visual identity and reads as generic theme boilerplate rather than a
   deliberately designed site.

This ADR records why the site is shaped this way, so future changes to navigation, color, or the
machine-readable surface do not regress these decisions or let the representations drift apart.

## Decision

### 1. Audience split, single source of truth

The site serves two audiences from one generation step in `pages.yml`:

- **Interactive readers** get the Jekyll-built HTML site: sidebar navigation, built-in search, standard
  typography.
- **Automated consumers** get Markdown and text surfaces, generated in the same workflow run from the
  same `docs/guide/*.md` files.

Both outputs are produced by one workflow triggered on the same push. There is no separate
machine-readable source tree and no second generation path. A new or edited guide page appears in
both representations automatically, and the two cannot drift because they are not
independently maintained.

### 2. Machine-readable surfaces (normative)

The workflow MUST produce all of the following at stable paths under the site root:

| Path             | Contents                                                                                                            |
| ---------------- | ------------------------------------------------------------------------------------------------------------------- |
| `/llms.txt`      | Short project summary plus a linked index of every doc page (the llms.txt convention)                               |
| `/llm.txt`       | Compatibility alias of `/llms.txt` for clients using the singular convention                                        |
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

### 3. Information architecture

- **Single flat sidebar.** At most one nav entry per rendered page. Raw `/md/` copies are never
  present in the Jekyll source tree (per §2), so they can never contribute a duplicate entry.
- **Quickstart-first ordering.** Getting Started is `nav_order: 2`, immediately after Home
  (`nav_order: 1`). Reference material (API Reference) sorts last.
- **Task-oriented titles.** Page titles name what the reader does or learns ("Getting
  Started", "Knowledge Graph Modeling"), not internal component names.
- **`NAV_ORDER` is the sidebar contract.** The `NAV_ORDER` associative array in `pages.yml` is
  the single authoritative mapping from `docs/guide/<page>.md` basename to `nav_order`, title,
  and one-line description. A guide page added without a corresponding `NAV_ORDER` entry fails
  the build (§7): it is never silently assigned a fallback `nav_order` or a heading-derived
  title, since either would also leave the page without a curated description for `llms.txt`.
- **Built-in search stays enabled** (`search_enabled: true`). It is the primary in-page
  navigation aid `just-the-docs` provides and must not be disabled to "simplify" the site.
- **Homepage is overview, not marketing.** `index.md` carries: a one-paragraph project
  description, the machine-readable surface pointers (§2), install instructions, and the
  documentation index. It contains no testimonials or promotional copy.

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
  inherited from `just-the-docs` defaults and are not to be overridden. The content remains
  the primary interface.

### 5. Page contract

Every `docs/guide/*.md` page is transformed, at build time, into a site page carrying:

- Front matter (`layout`, `title`, `nav_order`, and `description` when the page has a
  `NAV_ORDER` entry) injected by the workflow: authors do not hand-write front matter into
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
  `llms.txt`. This gives a reader landing on any page a one-click path to the source repo, the
  published crate, the community channel, and the machine-readable index.

### 7. Change control

Changes to the information architecture (§3), the color scheme (§4), or the shape of the
machine-readable surface (§2, including adding, removing, or renaming one of `llms.txt` / `llm.txt` /
`llms-full.txt` / `/md/<page>.md`) require amending this ADR. Adding a guide page under the
existing `NAV_ORDER` scheme, or updating page content, does not.

This standard is enforced at build time, not left as aspirational prose. The "Assemble
Jekyll source" step in `pages.yml` asserts, and fails the build (`exit 1`) on violation,
that:

- Every `docs/guide/*.md` page (other than `README.md`) has a `NAV_ORDER` entry: no
  silent fallback to `nav_order: 99` or a heading-derived title (§3).
- Every basename produced by the guide-page loop is present in both `llms.txt` and
  `llms-full.txt` (`grep`-checked after generation): the regression guard for §2's "one
  source, no drift" claim if a future change reintroduces a separately maintained page
  list.
- Every basename has a corresponding raw-markdown copy in the built site
  (`_site/md/<page>.md`), checked in the post-build step against a manifest the assemble
  step writes, covering every page rather than a fixed sample.

A change to `pages.yml` that weakens or removes one of these assertions is itself a
change to this ADR's enforcement mechanism and needs the same amendment this section
already requires for information architecture, color, and machine-readable-surface changes.

## Alternatives considered

1. **Bespoke static-site framework (Mintlify, Docusaurus, Fumadocs).** These frameworks give
   more layout control, but each
   requires an npm/Node toolchain and its own build/deploy pipeline. khive ships a single Rust
   binary with zero Node dependency anywhere in the repo; adding one solely for the docs site
   would be a new build-time dependency for a project that otherwise has none. Rejected.
2. **Serve machine-readable documentation through content negotiation** (clients get Markdown
   via `Accept` headers or a `?format=md` query parameter on the same URL). GitHub Pages is a static
   host with no server-side content negotiation, so this would require a proxy or a different
   hosting target. `llms.txt` and per-page raw Markdown at predictable, stable paths form a
   simple convention that requires no negotiation logic or additional hosting. Rejected.
3. **Keep the theme default purple accent.** Zero-effort, but it reads as unstyled theme
   boilerplate rather than a site khive deliberately designed, and does not meet the
   "clean, uncluttered, considered" bar. Rejected.

## Consequences

- `remote_theme: just-the-docs` constrains the site to that theme's layout primitives (sidebar,
  header, `aux_links`, and color-scheme SCSS variables). The selected information architecture
  uses flat navigation, quickstart-first ordering, content-first styling, and one entry per page.
- Because generation is a single workflow (`pages.yml`) triggered on push to `docs/guide/**`
  (or `README.md`, or the workflow file itself), docs changes ship only through that path.
  There is no local preview step in this ADR's scope; authors verify by reading the rendered
  Markdown and, for structural changes to the workflow, by running the assembly script's shell
  logic locally before pushing.
- The `NAV_ORDER` table is bash-associative-array state embedded in a YAML workflow file. This
  is intentionally low-tech (no static-site generator config format, no separate data file) to
  keep the zero-Node-toolchain property; the cost is that `NAV_ORDER` and the per-page loop
  logic live in a shell heredoc rather than a typed config. Acceptable at the current page
  count (7 guide pages); if the page count grows enough that this becomes unwieldy, an ADR
  amendment can move `NAV_ORDER` to a data file without changing any of the standards in §2-§6.
- Raw markdown staged outside the Jekyll source (`_raw_md/` → post-build copy into `_site/md/`)
  adds one additional workflow step after the Jekyll build, with existence assertions
  (`test -f _site/md/<page>.md`) guarding against silent breakage of the machine-readable surface.

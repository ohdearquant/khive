# KG diff

`kkernel kg diff [<ref>] --repo <repository>` renders changes to the tracked
`.khive/kg/entities.ndjson` and `.khive/kg/edges.ndjson` files. The reference defaults
to `HEAD`; branches, tags and revision expressions must resolve to a commit.

The comparison includes staged and unstaged changes together, from that commit to
the working tree. As with `git diff HEAD`, a newly created untracked file appears
only after `git add` (including intent-to-add). This command never stages files,
changes the index, opens a database, or runs validation rules.

Records are paired by entity `id` or edge `edge_id` from Git's removed and added
NDJSON lines. Output is deterministic: entities first, then edges, each ordered by
UUID. `+` means added, `-` removed and `~` changed. Entity summaries include their
kind and name; edge summaries include the edge ID, relation and endpoint names
when locally available. Changed fields follow each summary. Property changes name
the top-level property key and show the old/new JSON values. `<absent>` differs from
JSON `null`; nested property changes show the entire value of that property.
Object member order and unchanged records moved between lines produce no entry.

Invalid references, malformed changed NDJSON, missing required identity/display
fields and duplicate changed IDs fail without partial output. Working-tree entity
records are also read for endpoint names. An empty semantic diff prints
`No KG changes.` The command is a presentation pass over Git's unified patch,
not a database status comparison or a change-set writer (ADR-020 section 7).

Git receives a resolved commit ID and fixed file paths. External diff and textconv
are disabled, as are configured content-filter programs through the existing
hardened Git builder. Concurrent file edits are not an atomic snapshot; run again
after the working tree settles if another process is editing these files.

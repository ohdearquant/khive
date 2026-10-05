# Repair duplicate issue and pull-request notes

`kkernel git-dedup` previews deliberate note merges for one live canonical
project. It does not change ingest behavior or add an MCP verb.

```sh
kkernel git-dedup --project <full-project-uuid> --namespace local --db /path/to/khive.db
kkernel git-dedup --project <full-project-uuid> --namespace local --db /path/to/khive.db --apply
```

The default is a read-only repair preview. Normal runtime startup can still run
schema migrations. `--apply` makes a fresh preview and applies its guarded pairs;
it does not accept a saved JSON report as authorization. The callable Rust API
also separates `plan_dedup` from `apply_dedup` for a caller holding the actual
opaque plan. The `preview_id` fingerprints the bounded observations and planned
pairs; it is not a database-wide transaction or a promise that later evidence
will be unchanged.

A note belongs to the selected project only when both its `properties.project_id`
and its live `annotates` edge prove that association. Retained deleted project
ancestors count only when their explicit `merged_into` chain reaches the selected
live project. Equal project names or unmerged anchors do not count. An annotation
to another live project makes the note ambiguous, including when the other
project is in another namespace.

Typed positive integer numbers group notes by project, kind, and number. The
survivor is a numbered note with a real title, then the longest body, most property
keys, earliest creation time, and lowest UUID as deterministic tie breakers.
Numberless notes are left untouched unless their name is exactly `[issue]` or
`[pull_request]` and their stored HTTP(S) URL uniquely matches one numbered group.
URL equality is byte-for-byte, with no network request or normalization. URLs
with credentials, query strings, or fragments cannot supply this evidence.
Malformed numbers, missing project evidence, and ambiguous URLs are reported
unchanged. A numbered donor with the accepted URL is merged before a numberless
placeholder when the chosen survivor lacks a URL key. An existing null or
conflicting survivor URL is never overwritten to manufacture evidence.

The census is limited to 4,096 project lineage entries, 64 lineage hops, 10,000
candidate notes, 256 KiB of name/body/properties per note, and 16 MiB of those
payloads in total. The SQL query suppresses oversized payloads before returning
them. Exceeding any bound refuses the entire apply; uniqueness is never inferred
from a truncated page. Narrow the data deliberately rather than treating the
partial report as permission to merge.

Each pair is atomic, and the whole run can partially apply. Every merge rechecks
exact note revisions, canonical project membership, and any required URL evidence
on the merge writer connection. Further pairs use the revision returned by the
previous committed merge. An intervening edit or changed evidence stops that
group without rereading or retrying; independent groups can continue. The JSON
report lists planned, applied, refused, and unchanged note IDs. Planned revisions
are the original observations; applied rows contain the committed `kept_version`
and the normal merge summary, including any post-commit reindex warning.

Merges retain distinct bodies, normal edge rewiring and edge-preimage audit
behavior, and the tombstoned duplicate's original content and properties. The
survivor's merge history records the repair key, observed revisions, old project
IDs, and accepted placeholder URL. The `NoteMerged` event is not a full note
property snapshot. Surviving `project_id` is normalized inside the same merge,
so ordinary number lookup can find the canonical note. A repeated successful
apply with no intervening changes produces no further merge or event.

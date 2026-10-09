# Corpus export

`knowledge.export(format="jsonl")` returns a deterministic snapshot of the selected
namespace's knowledge corpus. `format` is optional; `jsonl` is its only value. The
ordinary request `namespace` override selects one namespace through the registry's
gate and token. Without an override, the token's primary namespace is used, not its
wider read-visibility set.

```text
knowledge.export(namespace="local")
```

The response contains:

- `format`: `"jsonl"`.
- `namespace`: the selected namespace.
- `data`: one compact JSON object per line, with a final newline for each record.
  An empty corpus produces `""`.
- `counts`: `{atoms, domains, sections}`, counting the exported physical table rows.

Save `data` in the caller if a file is wanted. The verb accepts no filesystem path
and performs no corpus writes. The registry retains its ordinary dispatch audit.

Each line has a `type` discriminator: `atom`, `domain`, or `section`. Records sort
by the stored `id` with binary text ordering, then `type`. Object keys are sorted
recursively, including objects inside arrays; array order and textual payloads are
preserved. Equivalent stored JSON object key order therefore produces identical
dump bytes. No generated timestamp, search score, or embedding vector is included.

The fields are the source table fields, except section `embedding`: identifiers,
namespace, slug/name/content or description, tags, properties or members, lifecycle
status, source metadata, section heading/type/hash/order/token count, and stored
timestamps as integer microseconds. JSON columns become typed JSON values, SQL
NULL becomes JSON null, and atom `finalized` is a boolean. A retired section keeps
its stored `section_type` spelling and additionally carries `retired: true`.

The selection rules are:

- Live atom and domain rows in the selected namespace are included, regardless of
  lifecycle status: draft, deprecated, and other stored statuses are not search
  eligibility filters here.
- A domain's same-namespace mirror atom is included as its own atom row. The domain
  and atom may share an ID; they retain distinct `type` values and both count.
- Sections are included only when their namespace and their live parent atom's
  namespace both match the selection. Retired sections are preserved. Sections
  of tombstoned atoms and mismatched namespace/parent rows are excluded.
- Tombstoned atoms and domains are excluded. No row is mutated or deleted.

All rows come from one SQL statement and its read snapshot. The full dump is
materialized in memory; this is an export, not a paged search. A read or malformed
stored JSON error refuses the entire response rather than silently dropping a row
or returning a partial dump.

This is the corpus slice of ADR-048 and issue #4815. It is not a graph
entity/note/edge export, and `knowledge.import` does not accept this JSONL as a
round-trip format. Those broader ADR-048 section 9 behaviors remain deferred.

# Record attachments

`attach` adds or replaces one rendition of an existing entity or note. Roles are
caller-chosen strings, with at most one attachment for each role on a record.
A different version of the content belongs on another record linked in the graph;
a different format of the same content belongs under another attachment role.

```text
attach(id="<record UUID>", role="markdown", bytes="aGVsbG8K", media_type="text/markdown")
get(id="<record UUID>")
get(id="<record UUID>", hydrate=["markdown"])
```

The example stores the bytes `hello\n`. For a held Markdown document, `markdown`
is a descriptive rendition role; `content` is also a generic role for ordinary
documents. Small text that belongs in a record's text column can stay there.
An attachment is the record's own content, not a substitute for an edge to another
document or for an external URL.

Inline bytes use the existing 64 MiB whole-object limit. Oversized encoded input
is refused before decoding; decoded bytes are checked against the same limit.

The attachment response contains `id`, `role`, `content_ref`, `size`, `media_type`
and boolean `replaced`. Replacing a role keeps one row for that role. Blob bytes
are confirmed before the attachment transaction; the transaction rechecks the live
record and publishes its attachment row. A refused row write may leave an
unreferenced blob, but does not expose a new attachment row or remove an existing
role's previous attachment. An indeterminate commit outcome remains indeterminate;
a lost acknowledgement is not proof that the transaction rolled back.

`get` returns an `attachments` object keyed by role when attachments exist. Each
entry contains `role`, `content_ref`, `size` and `media_type`. Only roles named in
`hydrate` gain a `bytes` field containing base64. Default reads never inline blob
bytes. An unknown requested role is an error naming that role. Hydration reads
verified content; it does not fetch an external URL.

Generic publication refuses authenticated roles owned by another pack. In
particular, moodboard's protected content and network representations must use
that pack's publication path. Malformed base64, invalid roles and absent records
produce explicit errors.

This initial surface publishes attachments for records owned by the canonical
main backend. A runtime routed to a secondary backend cannot publish attachment
rows there. Attachment metadata for readable routed records is still read from
the canonical main authority. Local file paths, detach, export and attachments
during record creation are separate surfaces.

Soft deletion retains attachment rows; hard deletion removes them under the
existing cleanup rules. This change does not enable garbage collection on a
current-schema store: the existing exact-epoch sweep guard still refuses both
dry and live sweeps before deleting anything. That restriction is tracked by
[#3324](https://github.com/ohdearquant/khive/issues/3324).

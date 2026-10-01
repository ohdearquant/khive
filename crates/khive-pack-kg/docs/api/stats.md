# KG stats

`stats()` counts live entities, edges, and notes across the caller-visible namespace set.
`count_scope` states `namespaces: caller_visible` and `rows: live_only`; soft-deleted entities
are excluded from both entity fields.

On a backend supporting grouped entity reporting, `entities_by_type` is an array:

```json
{
  "entities": 4,
  "entities_by_type": [
    {"entity_type": null, "count": 1},
    {"entity_type": "<null>", "count": 1},
    {"entity_type": "algorithm", "count": 2}
  ]
}
```

The null record appears exactly once, first, with count zero when no live entity has SQL NULL
in `entity_type`. Subsequent records cover observed stored string labels in ascending UTF-8 byte
order. Labels remain exact: SQL NULL, the empty string, `null`, `<null>`, and backslash-prefixed
labels are distinct. There is no sentinel or escaping convention. Counts group the stored
`entity_type` column across entity kinds; they do not infer a type from properties or restrict
historical labels to the currently registered vocabulary.

The bucket sum equals `entities`: the total is derived from that same grouped backend read,
not from a second scalar count. SQLite executes one grouped SELECT for the entire visible set,
including sets above the scalar counter's namespace chunk size. Duplicate namespaces do not
multiply rows; an empty visible set contributes no rows. An empty supported backend returns
`entities: 0` and `entities_by_type: [{"entity_type": null, "count": 0}]`.

`EntityStore::count_entities_by_type` is provided and defaults to `Ok(None)`. That result means
reporting is unavailable. In that case stats retains its legacy scalar count and omits
`entities_by_type` entirely. Absence does not mean zero entities or an empty supported report.
A supported backend's read error remains an error; stats does not hide it behind the scalar
fallback. Existing implementors need not add a method to retain their scalar stats behavior.

Only the entity total and its breakdown share this snapshot. Other stats fields keep their
existing separate reads; this feature does not promise one snapshot for the entire response.

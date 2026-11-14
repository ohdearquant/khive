# Tool registry

`tool.register` creates a tool, skill, plugin or verb by name in the request's
namespace. Registering that name again updates its supplied fields on the same
entity. Names compare without ASCII case; the stored spelling and UUID remain
unchanged.

```text
tool.register(name="echo", source="mcp:example")
tool.register(name="echo", schema={"type":"object"})
tool.describe(tool="echo")
```

The second call stores the schema and returns `created:false`, `updated:true`,
`unchanged:false`. Repeating it with the same schema returns `created:false`,
`updated:false`, `unchanged:true`.

These three flags describe the **registry entity row**. An unchanged row can
still gain capability links or have its search index repaired. A successful
creation returns `created:true`, `updated:false`, `unchanged:false`.

| Field                            | Existing-name behavior                                                            |
| -------------------------------- | --------------------------------------------------------------------------------- |
| `kind`                           | Replace the subtype and its kind tag; retain other tags.                          |
| `description`                    | Replace the description and update its search indexes.                            |
| `schema`                         | Replace the entire object, including nested members; `{}` stores an empty object. |
| `source`, `side_effect`, `trust` | Replace the supplied value.                                                       |
| `tags`                           | Add extra tags without removing existing extra tags.                              |
| `capabilities`                   | Create missing capability concepts and add missing `implements` links.            |

Omitted and `null` optional fields preserve existing values. Empty tag and
capability arrays remove nothing. The `tool`, `write` and `external` defaults
apply only when creating a row. Updates preserve the UUID, name spelling,
namespace, creation time, `registered_at` and unrelated stored properties.
An equal row keeps its revision and update timestamp. A concurrent change can
refuse the update or index repair; retry with the current definition. Index
repair checks the live row revision before each text or vector publication, so
a stale retry cannot overwrite indexes from a newer registration.

Grants pin the registry UUID and a canonical digest of `source`, `side_effect`,
`trust` and `schema`. Changing one of those inputs makes a grant for the old
definition stop matching. The grant row is retained, and policy or the default
decision can still allow the tool. Metadata and capability edits do not change
that digest. Restoring the original four inputs can make its original grant
match again; registration is not permanent grant revocation.

`tool.ingest` retains its create-or-return behavior for existing names. Use
`tool.register` to correct a stored definition. Retired registry names still
require a new name; registration does not restore a deleted row.

See [ADR-180](../../docs/adr/ADR-180-tool-pack.md) for registry and grant semantics.

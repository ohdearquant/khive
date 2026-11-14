# Tool registry

`tool.register` creates a tool, skill, plugin or verb by name in the request's
namespace. Registering that name again updates its mutable supplied fields on
the same entity. Names compare without ASCII case; the stored spelling and UUID
remain unchanged.

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

| Field                  | Existing-name behavior                                                            |
| ---------------------- | --------------------------------------------------------------------------------- |
| `kind`                 | Preserve the subtype; a different supplied kind requires a new name.              |
| `description`          | Replace the description and update its search indexes.                            |
| `schema`               | Replace the entire object, including nested members; `{}` stores an empty object. |
| `source`               | Preserve the source; a different supplied source requires a new name.             |
| `side_effect`, `trust` | Replace only with the same or a later value in the orders below.                  |
| `tags`                 | Add extra tags without removing existing extra tags.                              |
| `capabilities`         | Create missing capability concepts and add missing `implements` links.            |

Omitted and `null` optional fields preserve existing values. Empty tag and
capability arrays remove nothing. The `tool`, `write` and `external` defaults
apply only when creating a row. Updates preserve the UUID, name spelling,
namespace, creation time, `registered_at` and unrelated stored properties.
An equal row keeps its revision and update timestamp. A concurrent change can
refuse the update or index repair; retry with the current definition. Index
repair checks the live row revision before each text or vector publication, so
a stale retry cannot overwrite indexes from a newer registration.

Changing `source` or `kind` on any existing registration refuses, including
plain `exec:` and `mcp:` registrations and changing one `exec:` path to another.
The refusal names the changed fields and requires registering the different
source or kind under a new name. Equal, omitted and `null` values preserve the
stored fields. A refused correction changes no other fields or capability
links and appends no `tool_registration_changed` event.

Corrections cannot move `side_effect` earlier in `read`, `write`, `egress`,
`irreversible`, or `trust` earlier in `first_party`, `marketplace`, `external`.
This applies to every caller, including an actor other than the original
registrar. A refusal names the field, stored value and passed value, preserves
the row, and appends no `tool_registration_changed` event.

For a legacy row whose stored `side_effect` is absent, null, non-string or
unrecognized, a supplied correction may use `write`, `egress` or `irreversible`,
but not `read`. For an unknown stored `trust`, only `external` is allowed.
Omitted, `null` and unrelated metadata updates preserve these stored values.
Refusals identify the field and stored-state category without echoing arbitrary
legacy text.

Grants pin the registry UUID and a canonical digest of `source`, `side_effect`,
`trust` and `schema`. Changing one of those inputs makes a grant for the old
definition stop matching. The grant row is retained, and policy or the default
decision can still allow the tool. Metadata and capability edits do not change
that digest. Restoring an original `schema` can make an original grant match
again. A `side_effect` or `trust` restoration must obey the same monotonic order;
restoring a looser definition
refuses and leaves its old grant inactive. Registration is not permanent grant
revocation.

`tool.ingest` retains its create-or-return behavior for existing names. Use
`tool.register` to correct a stored definition. Retired registry names still
require a new name; registration does not restore a deleted row.
Public generic deletion refuses registry rows in both soft and hard modes,
including atomic preparation. Direct Rust and store maintenance APIs remain
within the embedder's trust boundary.

See [ADR-180](../../docs/adr/ADR-180-tool-pack.md) for registry and grant semantics.

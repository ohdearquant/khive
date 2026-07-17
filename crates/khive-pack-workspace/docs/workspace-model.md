# Workspace model

A workspace is a durable graph container for records that belong to one human or agent working
context. It is an ordinary entity plus typed membership edges, not a private table or service.

Identity comes from the generic entity ID and name. `schema_version` makes the properties payload
evolvable, while `filesystem_path` remains a non-unique mutable hint: moving a checkout must not
change graph identity or invalidate the record.

Membership broadens the base entity-to-entity `contains` contract to five already-shipped note
kinds. Reusing the shared edge relation keeps graph traversal uniform and avoids introducing a
workspace-specific membership verb. The v0 pack therefore adds vocabulary and validation only.

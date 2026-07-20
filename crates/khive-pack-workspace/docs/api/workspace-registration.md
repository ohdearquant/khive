# Workspace pack registration

`WorkspacePack` registers the `workspace` entity kind, its creation hook, and five membership edge
rules with the shared runtime. It contributes no verbs or private schema.

## `WorkspacePack::new`

The constructor binds a `KhiveRuntime` handle used by the pack runtime and returns a pack ready for
inventory-created or explicit installation. The pack depends on `kg`, `gtd`, and `session` because
its endpoint rules refer to their note kinds. The `issue`/`pull_request`/`commit` note kinds are
registered by a git-lifecycle pack that is not part of this distribution; the endpoint rules
naming them stay declared but inert until such a pack is loaded.

## Creation hook

Generic entity creation already requires a non-empty name. `WorkspaceHook::prepare_create` adds one
workspace-specific invariant: `properties.schema_version` must exist and be a signed or unsigned
JSON integer. Missing, null, floating-point, or non-numeric values return
`RuntimeError::InvalidInput`. The hook performs no post-create work.

`filesystem_path` is deliberately optional and unvalidated. It is a mutable locator that may become
stale, not workspace identity.

## Membership rules

The source must be a `workspace` entity and the relation must be `contains`. Valid targets are git
`issue`, `pull_request`, or `commit` notes, a GTD `task` note, or a `session` note. Document
membership is absent until the document pack defines its substrate contract.

## Dispatch

The handler table is empty. Any direct pack dispatch returns `RuntimeError::InvalidInput`; callers
create workspaces and links through the generic KG `create` and `link` verbs.

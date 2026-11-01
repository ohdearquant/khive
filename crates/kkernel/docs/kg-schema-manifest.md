# The KG schema created by init

`kkernel kg init` creates `.khive/kg/schema.yaml` when that path is absent:

```yaml
format_version: "2.0.0"
ontology_version: "1.0.0"
khive_version: "<the kkernel package version>"
remotes: []
```

The format version selects the major-2 manifest structure accepted by
`kkernel kg update`. The initial ontology version is `1.0.0`; it is independent
of the CLI writer version.

This is a minimal starting manifest. ADR-020 §3 assigns merged vocabulary
sections (`entity_kinds`, `entity_types`, `edge_relations`, `properties`) and
`packs` to pack management. Init leaves those sections out rather than copying a
static vocabulary that can drift from the registered packs. Add pack-managed
declarations when a self-contained vocabulary snapshot is needed.

To configure a remote, add an entry to `remotes` while retaining the generated
metadata. `kkernel kg update <remote>` resolves that entry's Git ref and updates
its `commit`. A Git commit pin is separate from an archive `pin` value.

Rerunning init preserves every existing regular `schema.yaml` byte, including
custom metadata, vocabulary, remotes, commit pins, archive pins and comments.
Init uses create-only publication and never merges or resets an existing schema. A
directory or symlink at the schema path is refused without replacing it or
writing its target. The hooks-only `--add-hooks` mode retains its existing
behavior.

# khive-pack-gtd

The GTD (Getting Things Done) verb pack for khive. Adds the `task` note kind
and five lifecycle verbs (`assign`, `next`, `complete`, `tasks`, `transition`)
over the notes substrate.

## Verbs

| Verb             | What it does                                                                  |
| ---------------- | ----------------------------------------------------------------------------- |
| `gtd.assign`     | Create a task (note with `kind=task`); defaults `status=inbox`, `priority=p2` |
| `gtd.next`       | List actionable tasks (`status` in `next`/`active`), priority-sorted          |
| `gtd.complete`   | Mark a task `done` (or `cancelled`) with an optional result note              |
| `gtd.tasks`      | Filtered task listing by status, assignee, priority                           |
| `gtd.transition` | Explicit lifecycle change, validated against the state machine below          |

All five verbs are declared in `GTD_HANDLERS` (`src/vocab.rs`) and dispatched
by `GtdPack::dispatch` (`src/pack.rs`).

## Task lifecycle

The `task` note kind's lifecycle field is `kind_status` (not `status` — that
name is reserved for `Note.status`, the row-visibility field). States and
legal transitions, from `GTD_NOTE_KIND_SPECS`:

```text
inbox ──> next ──> active ──> done
  │         │         │      cancelled
  │         ├──> waiting <───┤
  │         ├──> someday     │
  └─────────┴────────────────┘
```

`inbox` is the initial state; `done` and `cancelled` are terminal — no outgoing
transitions are permitted from either. Common aliases are accepted on write:
`todo=inbox`, `in_progress=active`, `blocked=waiting`, `later=someday`,
`finished=done`.

The pack also extends the base `depends_on` edge endpoint contract
([ADR-002](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-002-edge-ontology.md))
to allow `task`→`task` edges (`GTD_EDGE_RULES`), so task blockers are
graph-traversable even though the base contract restricts `depends_on` to
entity→entity.

## Usage

`GtdPack` requires the `kg` pack (`REQUIRES = ["kg"]`) to be registered on the
same runtime — it stores tasks as notes and relies on `kg`'s note substrate.

```rust
use khive_pack_gtd::GtdPack;
use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, VerbRegistryBuilder};
use serde_json::json;

let runtime = KhiveRuntime::new(RuntimeConfig::default())?;

let mut builder = VerbRegistryBuilder::new();
builder.register(KgPack::new(runtime.clone()));
builder.register(GtdPack::new(runtime));
let registry = builder.build()?;

let result = registry
    .dispatch(
        "gtd.assign",
        json!({"title": "Write crate READMEs", "priority": "p1"}),
    )
    .await?;
```

Over MCP: `request(ops="gtd.assign(title=\"Write crate READMEs\", priority=\"p1\")")`.

## Where this sits

`khive-pack-gtd` sits alongside `khive-pack-memory`, `khive-pack-comm`, and
`khive-pack-schedule` in the pack layer — each depends on `khive-pack-kg` and
plugs into `khive-runtime`'s `VerbRegistry`, consumed in turn by `khive-mcp`.
Governing ADR:
[ADR-019](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-019-gtd-pack.md) (GTD pack),
built on [ADR-017](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-017-pack-standard.md) (pack standard).

## License

Apache-2.0.

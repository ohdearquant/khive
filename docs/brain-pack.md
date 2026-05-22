# Brain Pack

The brain pack (`khive-pack-brain`) provides adaptive learning for khive. It tracks which entities
and recall operations are useful, updates Beta posteriors for pack parameters, and projects
configuration weights automatically. It is infrastructure — events are emitted by the kg/recall
pipelines automatically; you only need to call its verbs for inspection and manual feedback.

## Loading

```sh
# Environment variable (recommended — add to your .khive/config.toml or shell):
KHIVE_PACKS=kg,brain

# CLI flag:
khive --pack brain ...
```

The brain pack depends on the `kg` pack (ADR-037 inter-pack vocabulary dependencies).

## Verbs

| Verb           | Args                                                                               | What it does                                                                                                        |
| -------------- | ---------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| `brain.state`  | _(none)_                                                                           | Return current `BrainState` snapshot — posteriors, epoch, and projected configs                                     |
| `brain.config` | `parameter?: string`                                                               | Return projected config for one named parameter (with `mean`, `variance`, `ess`, `alpha`, `beta`) or all parameters |
| `brain.events` | `namespace?: string`, `limit?: u32` (default 20, max 100)                          | List recent brain-relevant events (`recall`, `search`, `brain.emit`, `get`, `remember`) for debugging               |
| `brain.reset`  | _(none)_                                                                           | Reset posteriors to priors while preserving event history; increments `exploration_epoch`                           |
| `brain.emit`   | `target_id: string`, `signal: useful \| not_useful \| wrong`, `namespace?: string` | Manually emit feedback for an entity, append a `brain.emit` event, and update brain state                           |

## See also

- [ADR-064](adr/ADR-064-brain-architecture.md) — brain architecture and Beta posterior design
- [ADR-063](adr/ADR-063-dynamic-pack-loading.md) — dynamic pack registry (how brain is loaded)
- [ADR-025](adr/ADR-025-pack-standard.md) — pack standard (`Pack` trait)

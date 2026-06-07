# khive brain plugin

Bayesian adaptive tuning on top of [khive-mcp](https://github.com/ohdearquant/khive).

The brain pack maintains a profile registry. Each profile holds Bayesian posteriors over
recall-weight parameters (`relevance_weight`, `salience_weight`, `temporal_weight`). Posteriors
update automatically from every successful verb dispatch via a post-dispatch hook, and can be nudged
manually with `brain.feedback`. The built-in `balanced-recall-v1` profile is active by default.

## Concepts

**Profile** — a named state bundle with a lifecycle (`active`, `inactive`, `archived`). The default
`balanced-recall-v1` profile drives the `recall` consumer kind.

**Binding** — a (actor, namespace, consumer_kind) → profile mapping. `brain.resolve` walks the
binding table to find which profile should serve a given caller context.

**Posterior** — a Beta-distribution parameter posterior. `brain.feedback` nudges posteriors;
`brain.reset` reverts to the prior.

## Verbs

All verbs are dispatched through the single MCP `request` tool
([ADR-016](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-016-request-dsl.md)).

Enable the brain pack by passing `--pack brain` (the kg pack dependency is resolved automatically):

```bash
kkernel mcp --pack brain
```

The `request` tool also accepts an optional `presentation` field (`agent` | `verbose` | `human`) per
[ADR-045](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-045-verb-response-presentation.md).
Default is `agent` (token-efficient) for MCP callers.

### Read (assertive)

| Verb                                                              | What it does                                              |
| ----------------------------------------------------------------- | --------------------------------------------------------- |
| `brain.profiles(lifecycle?)`                                      | List profiles, optionally filtered by lifecycle.          |
| `brain.profile(profile_id)`                                       | Profile metadata, latest snapshot, current state summary. |
| `brain.resolve(consumer_kind, actor?, namespace?)`                | Show which profile would serve a caller context.          |
| `brain.bindings(profile_id?, actor?, namespace?, consumer_kind?)` | List binding rows, optionally filtered.                   |

### Write lifecycle (commissive)

| Verb                                                                         | What it does                                                    |
| ---------------------------------------------------------------------------- | --------------------------------------------------------------- |
| `brain.activate(profile_id)`                                                 | Move a profile to Active — starts the live update loop.         |
| `brain.deactivate(profile_id)`                                               | Move a profile to Inactive — stops live updates, retains state. |
| `brain.archive(profile_id)`                                                  | Move a profile to Archived — read-only, audit-retained.         |
| `brain.feedback(target_id, signal, served_by_profile_id?, section_signals?)` | Emit a FeedbackExplicit event and update posteriors.            |

### Write binding / declaration

| Verb                                                                     | What it does                                              |
| ------------------------------------------------------------------------ | --------------------------------------------------------- |
| `brain.create_profile(name, description?, consumer_kind?, seed_priors?)` | Create a new Bayesian profile; starts in Inactive state.  |
| `brain.bind(profile_id, actor?, namespace?, consumer_kind?, priority?)`  | Write a row in the profile resolution table.              |
| `brain.unbind(profile_id?, actor?, namespace?, consumer_kind?)`          | Remove rows (at least one filter required).               |
| `brain.reset(profile_id?)`                                               | Reset posteriors to priors; increments exploration_epoch. |

### Convenience

| Verb                                                                         | What it does                                                                                                                                            |
| ---------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `brain.auto_feedback(recall_results, served_by_profile_id?, consumer_kind?)` | Emit implicit feedback for a batch of `memory.recall` results. Agents call this after recall instead of constructing individual `brain.feedback` calls. |

### Internal / operator-only (Subhandler — not exposed on public MCP surface)

| Verb                       | What it does                                        |
| -------------------------- | --------------------------------------------------- |
| `brain.state()`            | Return full BrainState snapshot for debugging.      |
| `brain.config(parameter?)` | Return projected config for a named pack parameter. |
| `brain.events(limit?)`     | List recent brain-relevant events for debugging.    |
| `brain.emit(...)`          | Deprecated alias for `brain.feedback`.              |

## What's New in 0.2.3

- **`brain.auto_feedback` verb**: convenience verb that lets agents emit implicit feedback for an
  entire `memory.recall` result set in one call, keeping the memory and brain packs decoupled.

## Skills

- **inspect** — read current profile state, list active profiles, and check which profile serves a
  given consumer kind.
- **tune** — adjust posteriors via explicit feedback and reset the balanced-recall profile when
  behavior is off.
- **profiles** — create and query the profile registry (list, get, resolve).
- **manage** — control profile lifecycle (activate, deactivate, archive).
- **bind** — wire profiles to actors, namespaces, and consumer kinds.

## Prerequisites

This plugin provides skills only — it does **not** bundle an MCP server. Install `kkernel` and
register it with the `brain` pack before using any skill.

```bash
# Install the binary
cargo install kkernel

# Register in your harness (Claude Code example)
claude mcp add --transport stdio khive -- kkernel mcp --pack brain
```

Or add to your project's `.mcp.json`:

```json
{
  "mcpServers": {
    "khive": {
      "command": "kkernel",
      "args": ["mcp", "--pack", "brain"]
    }
  }
}
```

## Install

```bash
/plugin marketplace add ohdearquant/khive
/plugin install brain
```

## License

Apache-2.0

# khive-pack-brain

Brain pack: profile-oriented orchestration over the `Fold`/`Objective` primitives
from [`khive-fold`](https://crates.io/crates/khive-fold) (ADR-032). A "profile" is
a named, lifecycle-managed Beta-posterior state (recall relevance/salience/
temporal weights, or per-section usefulness weights) that other packs resolve at
call time and feed back into via explicit or implicit signals.

## Features

- **Profile lifecycle** (`brain.create_profile`, `brain.activate`,
  `brain.deactivate`, `brain.archive`, `brain.reset`) — a profile moves through
  `Active` / `Inactive` / `Archived`; reset restores posteriors to priors while
  preserving event history
- **Context-based resolution** (`brain.resolve`, `brain.bind`, `brain.unbind`,
  `brain.bindings`) — a resolution table maps `(actor, namespace, consumer_kind)`
  wildcards to a profile ID with priority ordering, so different callers can be
  served different tuned profiles
- **Feedback ingestion** (`brain.feedback`, `brain.auto_feedback`) — appends a
  `FeedbackExplicit` event to the shared event log; `brain.auto_feedback` is
  convenience sugar so an agent can credit the top `memory.recall` hit without
  constructing a full feedback call
- **Deterministic Fold-based state** — `BalancedRecallFold` and
  `SectionPosteriorFold` each implement `khive_fold::Fold<Event, S>`
  (`init`/`reduce`/`finalize`) over the append-only event log, so profile state is
  always a pure replay, never a mutation in place
- **Adapter integrity gating** (`brain.register_adapter`) — records a
  content-hash + base-model-revision pair so an FFN/LoRA router only composes
  adapters that match the currently active base model revision

## Usage

`BrainPack` is registered with the runtime via `inventory` and dispatches its
verbs through the MCP `request` DSL, not called directly as a Rust API:

```text
request(ops="brain.create_profile(name=\"my-profile-v1\", consumer_kind=\"recall\")")
request(ops="brain.resolve(consumer_kind=\"recall\", actor=\"agent:docs\")")
request(ops="brain.feedback(target_id=\"<uuid>\", signal=\"useful\")")
```

The `Fold` implementations are exposed as a Rust API for embedding a profile's
reduction logic in another crate:

```rust
use khive_fold::{Fold, FoldContext};
use khive_pack_brain::fold::BalancedRecallFold;

let fold = BalancedRecallFold::new(khive_pack_brain::ENTITY_CACHE_CAPACITY);
let ctx = FoldContext::default();
let state = fold.init(&ctx);
// state = fold.reduce(state, &event, &ctx) for each Event in the log
```

## Verbs

| Verb                                                                    | What it does                                              |
| ----------------------------------------------------------------------- | --------------------------------------------------------- |
| `brain.profiles` / `brain.profile`                                      | List profiles / fetch one profile's metadata and snapshot |
| `brain.resolve`                                                         | Show which profile would serve a given caller context     |
| `brain.activate` / `brain.deactivate` / `brain.archive` / `brain.reset` | Lifecycle transitions                                     |
| `brain.feedback` / `brain.auto_feedback`                                | Emit explicit / implicit feedback events                  |
| `brain.bind` / `brain.unbind` / `brain.bindings`                        | Manage the profile resolution table                       |
| `brain.create_profile`                                                  | Create a new profile with optional seed priors            |
| `brain.register_adapter`                                                | Register an adapter integrity record for router gating    |

`brain.state`, `brain.config`, `brain.events`, and the deprecated `brain.emit` are
`Visibility::Subhandler` — internal/operator-only, not on the agent-facing MCP
surface.

## Where this sits

`khive-pack-brain` sits in the pack tier, built on `khive-brain-core` (posterior
state types), `khive-fold` (the `Fold` trait), `khive-runtime`, and
`khive-storage`; it `REQUIRES` the [`khive-pack-kg`](https://crates.io/crates/khive-pack-kg)
substrate at runtime. Consumers include
[`khive-pack-knowledge`](https://crates.io/crates/khive-pack-knowledge) (routes
`knowledge.feedback` section signals to a configured brain profile) and
[`khive-pack-memory`](https://crates.io/crates/khive-pack-memory) (recall ranking
weights). Governing ADR:
[ADR-032 (Brain as Profile-Orchestration over Fold + Objective)](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-032-brain-profile-orchestration.md).

## License

Apache-2.0.

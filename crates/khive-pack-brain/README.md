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
  served different tuned profiles; specific bindings are validated against the
  consumer kinds declared by the loaded packs
- **Feedback ingestion** (`brain.feedback`, `brain.auto_feedback`) — appends a
  `FeedbackExplicit` event to the shared event log; `brain.auto_feedback` is
  convenience sugar for attributing a signal to one explicitly selected
  `memory.recall` result without constructing a full feedback call. An omitted
  signal is an abstention, and rank position never creates positive evidence
- **Deterministic fold reducers** — `BalancedRecallFold` and
  `SectionPosteriorFold` implement pure `khive_fold::Fold<Event, S>` reducers.
  Current handlers invoke them synchronously; durable handler mutations append to
  the private `brain_event_log` and write JSON snapshots. The best-effort
  `DispatchHook` updates in-memory state without durable replay, while automatic
  shared-log catch-up remains deferred by ADR-017.
- **Adapter integrity gating** (`brain.register_adapter`) — records a
  content-hash + base-model-revision pair so an FFN/LoRA router only composes
  adapters that match the currently active base model revision

## Usage

`BrainPack` is registered with the runtime via `inventory` and dispatches its
verbs through the MCP `request` DSL, not called directly as a Rust API:

```text
request(ops="brain.create_profile(name=\"my-profile-v1\", consumer_kind=\"recall\")")
request(ops="brain.resolve(consumer_kind=\"recall\")")
request(ops="brain.feedback(target_id=\"<uuid>\", signal=\"useful\")")
request(ops="brain.auto_feedback(query=\"why\", results=[{\"id\": \"<uuid>\"}], target_id=\"<uuid>\", signal=\"implicit_positive\")")
```

`brain.feedback` and `brain.auto_feedback` can target a live KG entity or note
on another configured pack backend, including a message returned by
`search(kind="message", query="handoff")`. Copy its full UUID into `target_id`;
`get(id="<uuid>")` uses the same shared entity/note read resolver. For
`auto_feedback`, the selected ID must still match exactly one supplied result.
Normal Gate checks apply, and target lookup by ID does not filter the stored
namespace. A short hex prefix must resolve to one distinct UUID across the
backends; existing entity/note/edge/event collision checks remain, and backend
errors propagate. Feedback then requires an entity or note, so an edge or event
candidate is not an eligible target.

Only target reads cross backends. Feedback events, private event-log entries,
profiles and snapshots stay on brain's configured home runtime; the target is
not moved or rewritten. Knowledge-private atom/domain targets remain outside
this API and use `knowledge.feedback`. The shared resolver is installed by
`PackRegistry::register_packs_with_runtimes`; single-runtime registration keeps
its existing local lookup. See
[ADR-028](../../docs/adr/ADR-028-pack-scoped-backends.md#amendment-a5-shared-kg-handle-reads-across-pack-backends-2026-09-22).

Event counts, profile resolution, and binding listing default to the authorized
caller's actor scope. An explicit foreign actor requires visibility; aggregate
event counts additionally require `all_actors=true` and a serving-runtime
`[brain] fleet_readers` entry. See the
[API reference](../../docs/guide/api-reference.md#brainevent_counts--assertive)
for the exact actor filters and anonymous-caller behavior.

For a verb-by-caller dispatch-audit census, add
`group_by=["verb","actor"]` and `kind="audit"` to `brain.event_counts`.
The requested cross is nested by verb, then actor; omission or null preserves the
existing response. Sampled windows expose the cross only as
`counts_by_verb_and_actor_page_scoped`. `exhaustive=true` uses the existing
full-window walk and safety bound. Grouping adds no separate cell-count budget
or authorization path. See the [grouping contract](docs/api/event-count-groups.md).

Feedback uses a known `served_by_profile_id` as supplied. If that ID is unknown
(including a bare role name), it resolves through the caller's actor, namespace,
and recall-consumer binding, using the same table as recall. It does not create
profiles or interpret role aliases. An unknown ID with no matching binding is
`not_found`, naming the requested ID; only an omitted ID may use the default
profile. Events record the resolved profile ID and `profile_resolution=binding`
when this fallback applies. A known archived profile is still refused.

Explicit and correction feedback requires an attributed caller. An anonymous
caller receives a typed `invalid_input` error before feedback writes, even when
the profile would resolve through the default. Configure `actor.id` to submit
these judgments. Implicit anonymous signals keep their existing admission rules;
omitting the signal from `brain.auto_feedback` still abstains. This admission
change applies to new writes: historical anonymous events and their existing
posterior effects are retained, with no automatic deletion or retraining.

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
| `brain.feedback` / `brain.auto_feedback`                                | Emit direct / selected-result feedback events             |
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

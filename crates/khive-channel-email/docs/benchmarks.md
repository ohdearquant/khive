# Email channel benchmarks

The suite measures public routing, sender/trust labelling, IMAP guardrails,
screening and suppression-fetch APIs with generated fixtures. It does not connect
to SMTP or IMAP, read a mailbox, use real credentials, or load models. Timing is
advisory. No historical measurements were previously recorded for this crate.

## Execution

From `crates/`, on a qualified measurement host:

```sh
NO_PROXY=127.0.0.1 cargo bench -p khive-channel-email --benches -- --quick --noplot
NO_PROXY=127.0.0.1 cargo bench -p khive-channel-email --bench email_fetch_suppression -- --noplot
cargo bench -p khive-channel-email --bench email_routes -- --list
cargo bench -p khive-channel-email --bench email_allocations -- --list
cargo bench -p khive-channel-email --bench email_allocations -- allocations/route_inbound
cargo test -p khive-channel-email --test email_allocation_observer
```

`email_routes`, `email_backoff`, `email_screen` and
`email_fetch_suppression` use Criterion: 10 samples, a one-second warmup and a
three-second measurement target before command-line overrides. Fixtures and
supplemental fixed reports are created only in selected benchmark callbacks;
`--list` does not create fixtures. Criterion's central estimate is a sample-based
estimate, not a per-call p99. `--quick` changes Criterion sampling, not fixture
sizes or correctness checks.

`email_allocations` is a separate instrumented fixed-report executable. It emits
one JSON result per selected scene, with no timing baseline. It accepts
`--bench`, `--quick`, `--noplot`, `--test`, `--list` and one optional **literal
substring** filter; unsupported flags fail explicitly. All scenes run by default.
Its observer integrity check runs before the first selected report, and does not
run under `--list`.

## Scenarios and timing boundaries

| Target                    | Scenes and input                                                                                                          | Measured work                                                                                                                    | Untimed setup and checks                                                                                                                                                      |
| ------------------------- | ------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `email_routes`            | `route_inbound`: thread-matched, alias-matched, unmatched; 1,000 unique envelopes each                                    | A batch of 1,000 actual calls, including returned owned fields and the in-memory owner callback; batch ns / 1,000 gives ns/email | RFC822 generation and parseability, route table and positive owner map, result-vector capacity; exact variant, actor, thread and address for every result                     |
| `email_routes`            | `external_delivery_marking`: plain, withheld labels, typed maintainer refusal; 1,000 each                                 | Sender normalization, external/untrusted metadata and owned labels, or typed refusal                                             | Generated sender strings; complete successful metadata or exact `MaintainerAddress` error                                                                                     |
| `email_backoff`           | `record_failure`: first, escalating, saturated                                                                            | One real state transition including random jitter; no sleeping                                                                   | State cloned independently before every timer; exact attempt, step, warning and delay bounds; success/reset preflight                                                         |
| `email_backoff`           | Uncontended single acquire                                                                                                | Acquire to returned permit, inside the executor                                                                                  | Guard/executor setup and permit release                                                                                                                                       |
| `email_backoff`           | Queued 1 and 100 acquisitions                                                                                             | Release of one held permit through all queued acquisitions and permit drops, including clock/receipt bookkeeping                 | Every future first polled to `Pending`; fixture construction, future boxes and queue insertion; exact completion count, ordered receipts and post-batch exclusivity check     |
| `email_screen`            | Benign, one-family withheld and two-family quarantined, each with 1,000 reduced inputs and 1,000 pre-parsed MIME messages | `screen` or `screen_message`; the latter includes conversion from parsed MIME into screening input                               | Fixture generation, MIME parsing, built-in rule initialization and result-vector capacity; complete rule IDs, ordered families, disposition and opt-out flag for every result |
| `email_fetch_suppression` | Exactly 1,000 and 10,000 generated unique 64-lowercase-hex keys                                                           | Actual public async fetch: fresh HTTP client, loopback connection, request/response and JSON validation                          | Response generation, listener bind, URI construction; exact returned ordered set and timestamp, request path/host, synthetic bearer and Accept header                         |

Route, marking and screening targets also print one actual 1,000-operation batch
before Criterion sampling, with batch ns and its per-operation average. HTTP
prints one actual call. These fixed observations are not percentiles.

Queued single flight pre-holds the sole permit and witnesses all acquisition
futures as `Pending`, then releases the blocker. A current-thread executor awaits
the queued futures in insertion order; no 100-thread fanout occurs. The fixed
report contains exactly 1 or 100 actual release-to-acquire intervals, each measured
from the same release instant. Median and p99 use nearest rank
`ceil(p * count) - 1`: for 100 intervals, ranks 50 and 99; for one, both are that
one observation. These are **queued handoff and scheduling** intervals, including
earlier waiters, not independent uncontended acquire latencies. Criterion trends
measure the whole batch; dividing by the count gives a batch average.

Backoff and uncontended-acquire measurements use `Instant` around each call.
They include clock-reading overhead without subtraction; very small observations
should not be interpreted as the API's isolated CPU cost.

Backoff uses a one-second base and a 16-second cap, with 0, 3 or 10 prior failures.
Each timed call starts from a fresh clone, so iteration history does not shift the
scenario. Jitter is the production RNG, checked only against its valid bounds.

## Allocation reports

Allocation scenario IDs begin with `allocations/`, so they differ from the
unchanged timing benchmark IDs. Both `--list` and the JSON `scenario` field use
these IDs. The 14 allocation scenarios are:

| ID prefix                                   | Suffixes                                         |
| ------------------------------------------- | ------------------------------------------------ |
| `allocations/route_inbound/`                | `thread_matched`, `alias_matched`, `unmatched`   |
| `allocations/external_delivery_marking/`    | `plain`, `withheld_labels`, `maintainer_refused` |
| `allocations/backoff_record_failure/`       | `first`, `escalating`, `saturated`               |
| `allocations/single_flight/`                | `uncontended`, `queued/1`, `queued/100`          |
| `allocations/fetch_remote_suppression_set/` | `1000`, `10000`                                  |

The instrumented binary counts successful Rust global allocator requests:
`alloc`, `alloc_zeroed` and successful `realloc` each count once. It also reports
peak live Rust heap above the pre-call baseline. The observer covers all threads;
it does not count C allocator calls, TLS-library heap outside Rust, mmap, RSS or
allocator-internal realloc copies. These are advisory measurements, not zero-allocation
promises. Timing baselines come exclusively from the uninstrumented targets.

Route and marking observations wrap exactly 1,000 calls after one checked warm
batch. Input/expected fixtures and output-vector capacity are excluded; operation
output allocations remain included and are retained until observation finishes.
The reported per-email request count divides the batch count by 1,000. Backoff
observes one failure transition, with cloned state outside the window.

The uncontended acquire window includes executor entry and the public acquire,
with permit release afterward. Queued allocation windows include actual queue
insertion, blocker release and every acquire/drop, plus receipt bookkeeping.
Preallocated future boxes and result capacity are outside the window. Reports
name the queue size and divide requests by that exact number of acquisitions;
they do not claim to include a caller's task-spawn/boxing cost.

HTTP observations run after one checked warm call. They include **the client and
loopback stub together**, executor scheduling, the stub request-header buffer and
the returned set; response bytes, listener binding and expected keys are prepared
before observation. There is one client/server pair at a time. Peak live Rust
heap is not total bytes allocated and is not the serialized response size. The
separate ordinary observer-integrity test verifies worker-thread allocation,
successful reallocation and a retained-live-heap peak; no latency threshold is
promoted into that test.

## HTTP isolation and bounds

The only destination is numeric `127.0.0.1` on an ephemeral listener bound to that
address. The public API already refuses redirects. Before any client construction,
the harness checks `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` and their lowercase
forms. If any is set, the effective `NO_PROXY`/`no_proxy` must contain an exact
`127.0.0.1` or `*` comma-separated token. Uppercase takes precedence, including an
empty value. Other proxy-pattern spellings are conservatively refused. Failure
names the variable but never prints its value. No environment variable is changed.

On macOS and Windows, that exact bypass is required even with no proxy environment
variables, because transitive dependency features can enable native system-proxy
lookup. The command above supplies it explicitly. On Linux with no proxy variables,
the bypass is optional. No operating-system proxy settings are inspected.

The prebuilt response is below 1 MiB, request headers are capped at 8 KiB, and each
sample has a 35-second scope timeout around the production client's 30-second
request deadline. Client and stub are joined futures in the same executor, with
no spawned server task or helper process. Timeout or unwind drops both futures
and their sockets. This is a harness bound, not a performance guarantee.

## Source-correct scope

`route_inbound` returns `Owner`, `RoutedAlias` or `ExternalInbound`. Quarantine
occurs earlier in the channel, after screening and before routing, so there is no
quarantine-route call to benchmark. Unmatched external delivery is not quarantine.
`external_delivery_marking` builds sender/trust metadata; it does not parse or
validate Authentication-Results.

`screen` and `screen_message` use the compiled rule set. The target reads its
actual rule count from `screening::rules()` at runtime and prints it with each
scene. There is no public evaluator accepting a custom 10- or 100-rule set;
constructing a `RuleSet` alone cannot substitute one. Screening quarantine is
measured at this real boundary. The unsupported literal rule-count scenarios are
not measured, and no copied matcher or production visibility change is introduced.

Suppression responses contain `as_of`, `recipient_keys` and an exact `count`.
There are no actor-normalization or `has_more` semantics in this API. Both sizes
exercise complete ordered-set equality, rather than accepting a truncated page.

## Baseline ledger

The owning `bench-track` component for `khive-channel-email` produces these
baselines on its qualified host. Record the published commit, Rust toolchain,
CPU/OS, command and output artifact when measured. No number is fabricated from
source inspection; no historical values existed to replace.

| Group                                              | Baseline                           | Commit / toolchain / machine |
| -------------------------------------------------- | ---------------------------------- | ---------------------------- |
| Routing and sender/trust labelling                 | Pending qualified-host measurement | Pending                      |
| Backoff and single flight                          | Pending qualified-host measurement | Pending                      |
| Built-in screening, reduced and MIME               | Pending qualified-host measurement | Pending                      |
| HTTP suppression fetch, 1k and 10k                 | Pending qualified-host measurement | Pending                      |
| Rust allocation requests and incremental live heap | Pending qualified-host measurement | Pending                      |

## Performance guarantees

| Operation                      | Input                     | Metric                                                                            | Bound / coverage                                                                                                 |
| ------------------------------ | ------------------------- | --------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| `route_inbound`                | matched, 1k envelopes     | ns/email                                                                          | Advisory trend against the recorded baseline; thread-matched scene                                               |
| `route_inbound`                | quarantined, 1k envelopes | alloc/email                                                                       | Not measured: quarantine happens before routing; no such public route exists                                     |
| `ImapSingleFlight::acquire`    | 100 concurrent            | ns to acquire                                                                     | Advisory trend against the recorded baseline; actual queued release-to-acquire intervals                         |
| `screen`                       | 100 rules × 1k messages   | ns/message                                                                        | Not measured: public screening always uses the compiled rules; the measured built-in count is printed at runtime |
| `fetch_remote_suppression_set` | 10k entries (stubbed)     | peak live Rust heap above pre-call baseline (bytes/op), client plus loopback stub | Advisory trend against the recorded baseline; excludes C heap, mmap and RSS                                      |

A last recorded value is an observation, not a promise. Timing and memory trends
have no fixed-percentage failure threshold. The ordinary observer-integrity test
checks the measurement mechanism; it is not a performance bound on these APIs.

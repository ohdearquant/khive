# Gate benchmarks

Generated deterministic requests exercise public classifier and mailbox APIs without a runtime,
database, transport or external corpus. Every classifier entry comes from the exported
CLASSIFIED_OPERATIONS slice; the suite does not copy a list of names or hardcode its length.

## Scenarios and baselines

| Group / target                       | Scenarios                                     | Measurement and size                                                                                                                       | Baseline                                                    |
| ------------------------------------ | --------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------- |
| gate_classify / gate_lifecycle       | all_verbs                                     | Criterion visits each exported verb once per iteration; fixed report records 1,000,000 individual durations per verb                       | pending per-call median/p99 ns; pending uniform-table trend |
| gate_mailbox_policy / gate_lifecycle | matched_owner, unmatched_owner, denied_reader | check_with_mailbox_policy using real MailboxReadGate over AllowAllGate; 128 calls per iteration plus separate 100,000-call report per case | pending ns/op                                               |
| gate_mailbox_owner / gate_lifecycle  | matched_owner, unmatched_owner, denied_reader | mailbox_read_owner on the same requests; 128 calls per iteration plus separate 100,000-call report per case                                | pending ns/op                                               |
| gate_allocations / gate_allocations  | all_verbs                                     | Separate observer reports 1,000,000 calls per verb; instrumented Criterion iteration observes one call per verb                            | pending observed alloc/op; required count 0                 |

Baseline measurements are pending the owning bench-track component's qualified-host run. Record
commit, Rust toolchain, machine, load, command and table size with future observations. There is
no previous published numerical baseline for this crate. Timing trends are advisory.

## Classifier protocol

For every exported verb, the timing target records one million individual durations in a single
reused Vec<u64>, an 8 MB logical sample payload. It processes one verb at a time, sorts after
recording, and emits its median and p99. Median is the mean of sorted positions 500,000 and
500,001; nearest-rank p99 is position 990,000 (one-based). These are individual-call distributions,
not quantiles of batch means or Criterion averages. Each result must equal the entry's expected
classification. Unknown names are not synthesized as Write.

The interval encloses the black-boxed public classify_operation call and result. Expected-result
assertions, sample storage, sorting and reporting occur afterwards. Tiny-call measurements include
clock/harness overhead and timer resolution; no overhead is subtracted and no hardware-cycle
accuracy is claimed. The separate Criterion all_verbs trend times one full-table pass including
its loop and result accumulation, with dynamic element throughput. Its batch time is not a
per-verb median or p99.

The fixed timing report performs table length times 1,000,000 calls. The allocation target and
ordinary allocation test each separately observe that same call count. --quick changes Criterion
sampling, not these fixed counts. Run the full suite on the qualified benchmark host.

## Mailbox semantics and timing

Fixture setup installs owner actor/lambda:owner and sole reader actor/lambda:owner:reader, over
AllowAllGate. Constructor SHA-256 fingerprinting, reader registration, request JSON and labels are
untimed. Each timed request is comm.inbox with a mailbox_actor selector and local namespace.

| Case            | Request                                              | mailbox_read_owner       | check_with_mailbox_policy     |
| --------------- | ---------------------------------------------------- | ------------------------ | ----------------------------- |
| matched_owner   | listed reader selects lambda:owner                   | Some(actor/lambda:owner) | Allow, no obligations         |
| unmatched_owner | listed reader selects lambda:other                   | Some(actor/lambda:other) | Deny mailbox_read_not_granted |
| denied_reader   | actor/lambda:owner:reader:child selects lambda:owner | Some(actor/lambda:owner) | Deny mailbox_read_not_granted |

Owner extraction validates a delegated target; it never grants access or returns a policy denial.
unmatched_owner is a valid selector whose owner is not granted by this policy, so extraction
returns Some. denied_reader names the paired policy outcome; extraction still succeeds. Colons
do not create a hierarchy. Untimed controls separately verify that own, omitted and non-mailbox
selectors return None, while local and null are InvalidSelector / GateError::Policy errors, not
GateDecision::Deny. comm.probe's different actor key and anonymous-local case are not covered.

The actual helper retains double composition: MailboxReadGate::check applies mailbox policy,
then check_with_mailbox_policy applies it again. A granted delegated request performs two owner
and capability checks. An initial Deny short-circuits the outer pass. No mock or shortcut removes
this current public behavior from the measurement.

Each function/case has a fixed 100,000-call report plus bounded 128-call Criterion trends. Reused
requests are borrowed. A preallocated buffer retains at most 128 results; each public call's own
ActorRef/String allocations and result retention are timed. Exact owners, decisions, empty
obligations and denial reasons are checked after each interval; returned objects are dropped
there. Fixed ns/op is total measured duration divided by 100,000, not a percentile. The mailbox
functions have no zero-allocation guarantee.

## Allocation guarantee and observer integrity

The separate gate_allocations executable and classifier_allocation integration test forward to
System through a const thread-local counter. It observes successful alloc, alloc_zeroed and realloc
requests only in the synchronous measuring scope; deallocation is not an allocation. Unchanged
pointer/layout contracts are forwarded, hooks use allocation-free Cell updates, and a drop guard
disables observation on unwind. Other test threads are excluded; this classifier starts no worker
threads. The metric is Rust global allocator requests, not RSS.

all_classifier_verbs_do_not_allocate observes 1,000,000 actual calls per exported entry, requires
the correct result on every call, and asserts zero allocations after observation. The integrity
test allocation_observer_detects_heap_growth must see a real black-boxed Box. The allocation
benchmark repeats that positive control before its reports. Its instrumented Criterion time
includes observer management and assertion bookkeeping, so it is not a latency baseline. The
gate_lifecycle executable has no custom allocator.

The zero-allocation test must turn red if an allocation is inserted inside classify_operation;
the integrity test must turn red if the observer increment is disabled. Timing has no fixed
threshold and remains separate from these deterministic checks.

## Performance guarantees

| Operation                   | Input                    | Metric                 | Bound                                                       |
| --------------------------- | ------------------------ | ---------------------- | ----------------------------------------------------------- |
| `classify_operation`        | all verbs, 1M calls      | median latency (ns/op) | Advisory trend against the recorded baseline                |
| `classify_operation`        | all verbs, 1M calls      | alloc/op               | 0 allocations per call (deterministic; gates pull requests) |
| `check_with_mailbox_policy` | matched label, 100k      | ns/op                  | Advisory trend against the recorded baseline                |
| `mailbox_read_owner`        | unmatched selector, 100k | ns/op                  | Advisory trend against the recorded baseline                |

## Execution

```sh
cargo bench -p khive-gate --benches -- --quick --noplot
cargo test -p khive-gate --test classifier_allocation
```

The benchmark command executes both targets, all exported classifier entries and all six mailbox
function/case combinations. Fixed reports remain full-sized with --quick. Benchmark-track workflow
wiring is managed separately; this suite changes no production classifier or mailbox policy.

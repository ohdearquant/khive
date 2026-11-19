# Channel benchmarks

The generated fixtures use a fixed Ed25519 seed, canonical signed receipt bytes and deterministic
UUIDs. No external corpus, credentials, transport, runtime, or database is required. The timing
suite invokes the public verification APIs and the actual SenderReceiptClaim fields.

## Scenarios and baselines

| Group / target                             | Scenarios              | What is measured                                                                                       | Size                                                                                   | Baseline                                    |
| ------------------------------------------ | ---------------------- | ------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------- | ------------------------------------------- |
| channel_sig_verify / channel_lifecycle     | 1, 1000                | Signature verification with an already admitted public key and prepared canonical receipt input        | 1 / 1000 verifies per Criterion iteration and fixed report                             | pending ns/verify                           |
| channel_receipt_verify / channel_lifecycle | valid_128, bad_sig_128 | Complete owned receipt verification, including canonical input reconstruction and public-key admission | 128 calls per Criterion iteration; separate fixed 100,000-call report for each outcome | pending ns/op                               |
| channel_sender_receipt / channel_lifecycle | build_read_10000       | In-memory claim construction, reading all three fields, and retaining the observed tuples              | 10,000 claims per Criterion iteration and fixed report                                 | pending ns/op                               |
| channel_allocations / channel_allocations  | sig_verify_1000        | Isolated allocation observation around prepared signature verification                                 | fixed reports for 1 / 1000 calls; 1000 calls per instrumented Criterion iteration      | pending observed alloc/op; required count 0 |

The fixed reports print JSON with calls, elapsed_ns and ns_per_call. The latter is total measured
batch duration divided by actual call count, not a percentile. Criterion displays time per batch
and element throughput; divide a batch time by its declared size for per-call comparisons.
The allocation target also prints integer allocations and alloc_per_verify. Its Criterion time
includes observer management and assertion bookkeeping and is not the uninstrumented timing
baseline. These targets are distinct executables so the global allocator observer cannot add
hooks to channel_lifecycle timing.

All baselines are pending measurement by the owning bench-track component on a qualified host.
Record the commit, Rust toolchain, machine, load and command alongside future results. No prior
published numerical baseline exists for this crate.

## Timing boundaries and bounded work

- Signing, key construction, fixture generation and input cloning are untimed for sig_verify.
  Its loop and Result success accumulation are timed; correctness is asserted afterwards.
- receipt_verify processes one chunk of at most 128 cloned inputs and one preallocated result
  buffer at a time. Cloning and both buffer allocations are outside each measured chunk. The
  timer includes public verify and retaining its Result. Successful owned receipts are checked
  and dropped afterwards; an invalid receipt's cleanup occurs inside the owned verify call.
  Every successful result is compared with the exact source receipt, and every bad signature
  must return InvalidSignature. No 100k owned-input batch accumulates in Criterion warmup.
- The bad signature is a single-byte corruption of a genuine 64-byte signature over valid
  canonical identifiers. It reaches cryptographic verification, rather than failing signature
  length or identifier parsing first.
- Claim identifiers and epochs are generated before timing. Construction and all field reads,
  plus pushes into a preallocated tuple buffer, are timed. Every output tuple is compared with
  its input after the timer. This measures a local value round-trip, not serialization,
  persistence, network transport or a whole outbound delivery.
- Fixed reports execute 100,000 valid receipt checks, 100,000 bad-signature checks and 10,000
  claim round-trips once per target run. Criterion then trends bounded batches. --quick affects
  Criterion sampling, not these fixed report counts. No latency threshold is enforced.

## Source limitations

There is no expired receipt scenario: DeliveryReceiptBinding and DeliveryReceipt contain no
expiration field, and VerifiedRecipientReceipt::verify accepts no clock or expiration policy.
Its checks concern signature length, canonical UUID input, key admission and signature validity.
The requested expiry case cannot be represented at this public API and is explicitly omitted.

SenderReceiptClaim is a public field struct, with no serialization implementation or build/read
methods. The benchmark measures its actual construction and field access. It represents claimed
identifiers on unverified receipt items and does not authorize a delivery-state change.

## Deterministic allocation guarantee

The integration test signature_verification_does_not_allocate observes 1000 calls to
ReceiptSigningPublicKey::verify after fixture/key preparation, with no verifier warm-up, and
requires every signature to pass with zero allocations. The guarantee concerns the prepared
verifier, not receipt verification or canonical input construction, which allocate owned values.

A const thread-local counter forwards to System and counts successful alloc, alloc_zeroed and
realloc requests only while the measured synchronous scope is active. Deallocation is not an
allocation. The scope guard disables counting on unwind, and allocator hooks do not allocate or
panic. Other test threads are excluded from the measurement; no worker thread is started by
this verifier fixture.

allocation_observer_detects_heap_growth requires the counter to see a real black-boxed heap
allocation. The allocation benchmark repeats this integrity check before its zero-count report.
The gate must fail if a heap allocation is added inside verify; the integrity control must fail
if the observer increment is disabled. Timing remains advisory and separate from this count gate.

## Performance guarantees

| Operation                          | Input         | Metric    | Bound                                                       |
| ---------------------------------- | ------------- | --------- | ----------------------------------------------------------- |
| `ReceiptSigningPublicKey::verify`  | 1000 verifies | ns/verify | Advisory trend against the recorded baseline                |
| `ReceiptSigningPublicKey::verify`  | 1000 verifies | alloc/op  | 0 allocations per call (deterministic; gates pull requests) |
| `VerifiedRecipientReceipt::verify` | valid, 100k   | ns/op     | Advisory trend against the recorded baseline                |
| `VerifiedRecipientReceipt::verify` | bad-sig, 100k | ns/op     | Advisory trend against the recorded baseline                |
| `SenderReceiptClaim` round-trip    | 10k           | ns/op     | Advisory trend against the recorded baseline                |

Only the allocation row is deterministic; timing rows are advisory against recorded baselines.

## Execution

```sh
cargo bench -p khive-channel --benches -- --quick --noplot
cargo test -p khive-channel --test signature_allocation
```

The benchmark command runs both targets and every declared scenario. The test command checks
the zero-allocation bound and observer integrity. No production API or workflow changes are
needed for these fixtures; benchmark-track wiring is managed separately.

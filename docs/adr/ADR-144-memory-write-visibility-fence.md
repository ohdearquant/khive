# ADR-144: Operation-Level Write-Visibility Fence for Memory Recall

- Status: Accepted
- Decision: Arm A — additive write receipt plus session read fence
- Date: 2026-08-05
- Related: ADR-118 (fresh exact tail), #1084 (per-hit route labels, declined shape),
  #1161 (Cold/Empty cap divergence, separate lane)

## Context

ADR-118 gives recall a fresh exact tail: rows written after the last ANN segment
publication are scored exactly and merged with ANN candidates into a single
model source before fusion. That merge is deliberately silent — a result list
can contain both ANN-served and exactly-scored hits with no per-hit marker, and
nothing in the response distinguishes them.

What the surface does not offer is any way for a caller to _prove_ that a
specific write is visible to a subsequent recall:

- `memory.remember` returns `id`, `kind`, `salience`, `decay_factor`,
  `memory_type`, `created_at`, and optionally `edge_id`. It returns no log
  sequence, generation, or visibility state for the vectors it wrote.
- `memory.recall` accepts no consistency parameter. Its only inspected
  degradation field is `ann_unavailable`.
- `VectorSearchHit` is `subject_id`, `score`, `rank`; `ScoreBreakdown` exposes
  ranking components, not retrieval origin.

Mature vector and search systems place their strongest freshness control at the
write/request consistency boundary rather than in per-hit provenance:
Elasticsearch `refresh=wait_for` blocks the write acknowledgement until the
change is search-visible; Lucene exposes an index generation callers can wait
on; Milvus Session consistency maps the client's latest write timestamp into
the read guarantee; Qdrant `wait=true` returns a write only once it is applied
and searchable; LanceDB searches unindexed rows by brute force unless the
caller opts out with `fast_search=True`. The mechanisms differ, but each makes
the freshness/latency trade visible at operation or query scope.

The previously proposed alternative (#1084) — a per-model `ann | exact` route
label on responses — cannot express ADR-118's design: one model source may
legitimately contain both origins, and a route summary cannot prove that one
particular write is covered. Origin labels are diagnostics, not a correctness
primitive.

## Decision (accepted: Arm A — the fork below records the sign-off question as posed)

Add an additive, operation-scoped visibility contract to the memory pack:

1. **Write receipt.** `memory.remember` additionally returns a
   `visibility_token`: the namespace plus one `{model, ann_write_log_seq}`
   fence per vector written. Existing fields are unchanged; callers that
   ignore the token see today's behavior.

2. **Read fence.** `memory.recall` accepts an optional
   `consistency: "eventual" | "session"` parameter together with either a
   previously returned `visibility_token` or an equivalent `after` fence.
   - `eventual` (default) is today's behavior, unchanged.
   - `session` succeeds only when every requested model proves coverage of the
     fence by `segment_watermark ∪ exact_tail_snapshot`. When coverage cannot
     be proven it waits up to a caller-provided timeout, then returns a typed
     `freshness_unmet` result. It never silently serves an uncovered state.

3. **Diagnostics stay diagnostics.** Verbose responses may report per-hit
   origin and per-model watermarks, but no correctness claim rides on them.

### The fork as posed at sign-off (resolved: Arm A)

- **Arm A (accepted):** the receipt/fence contract above.
- **Arm B (explicit null):** formally decide that `memory.remember`
  acknowledges storage durability only, that search visibility is best-effort,
  and document that contract at the verb surface. This is a legitimate product
  decision; today's state is Arm B in behavior but undocumented, which is the
  actual defect. Accepting either arm closes it; leaving the surface silent
  does not.

## Alternatives considered

- **Per-hit origin labels (#1084 shape).** Rejected as the primitive: cannot
  prove visibility of a specific write, and ADR-118's merged model source makes
  the label set (`ann | exact`) incomplete. Retained only as optional
  diagnostics under this design.
- **Force-visible writes** (Elasticsearch `refresh=true` analog: every
  `remember` blocks until searchable). Rejected: taxes every write with
  worst-case publication latency to serve the minority of callers that need a
  fence, and removes the caller's ability to choose.
- **Global read-your-writes session state held server-side.** Rejected for the
  additive stage: khive's callers span processes and namespaces; an explicit
  token keeps the fence self-describing, replayable across processes, and free
  of server session affinity.

## Consequences

- Callers that need read-your-writes get a provable, bounded-wait contract;
  callers that do not pay nothing.
- The fence is per-model, so a mixed-model recall degrades precisely: a
  `freshness_unmet` result names the models that failed coverage.
- The exact-tail snapshot participates in coverage proofs, so under normal
  operation a `session` recall issued immediately after `remember` succeeds
  without waiting for segment publication — the tail already covers the fence.
- `freshness_unmet` is a new typed degradation state and must follow the
  established degradation-marking rules (flag says whether, log says why).
- Implementation risk concentrates in the coverage proof
  (`segment_watermark ∪ exact_tail_snapshot` per model, one snapshot); it must
  be read from one consistent snapshot to avoid proving coverage with two
  clocks.

## Out of scope

- The Cold/Empty cap belongs to ADR-118, not this contract. Its former
  cross-pack divergence was resolved by #1161: both memory and knowledge use
  the corpus-relative rebuild threshold for the newest log suffix.
- Knowledge-pack freshness: the fresh-tail helper landed (#1589) and the pack
  proves no current gap; this ADR adds no knowledge-pack requirement.

## Acceptance

Accepted with Arm A, under four conditions recorded here as part of the
decision:

1. This record's Status and Decision lines name the accepted arm before the
   record merges.
2. Sequencing is as written: receipt first, fence second, both additive.
   Implementation is tracked follow-up work and does not preempt existing
   scheduled priorities.
3. The one-consistent-snapshot property of the coverage proof (see
   Consequences) is review-blocking at implementation time: a coverage proof
   assembled from two separately read clocks must fail review.
4. The session-fence wait carries a server-side maximum timeout cap;
   caller-provided timeouts bound below that cap and can never request an
   unbounded hold.

## Amendment 1 (2026-09-27): receipt shape and one-snapshot proof

**Status**: Accepted (2026-09-27). This amendment records the owner's rulings
on implementation choices left open by the original decision; it does not
weaken Arm A.

### Receipt and replay

`memory.remember` returns `visibility_token` as a strict JSON object:

```json
{
  "version": 1,
  "namespace": "local",
  "fences": [{ "model": "example-model", "ann_write_log_seq": 123 }]
}
```

The namespace is the actual write namespace. There is exactly one fence per
vector written, with the `ann_write_log.seq` captured in that vector write's
transaction; model entries are unique and sorted by model name for stable
serialization. A write with no registered embedding models returns the same
object with `fences: []`. No token is returned for a rolled-back write.

An exact keyed `memory.remember` replay writes nothing and returns the original
durably stored per-memory, per-model fences. It must not mint a fresh log row
or infer a sequence from a later `MAX(seq)` query. If the original receipt
cannot be recovered, the replay reports `freshness_unmet` rather than claiming
new visibility. The receipt storage and vector-log insertion commit together
for keyed writes.
The receipt records the expected model count so a missing per-model fence
cannot be mistaken for a legitimate zero-model receipt.

### Session input and bounded wait

`memory.recall` defaults to `consistency: "eventual"`, preserving existing
behavior. `consistency: "session"` requires `visibility_token`; a missing token,
malformed shape or version, foreign namespace, duplicate model, or token model
outside the recall's requested model set is `InvalidInput`. A well-formed
fence whose sequence cannot be proven, including a future/unobserved sequence,
returns a typed `freshness_unmet` result naming the failed models; this does
not assert that the token was authentic. A missing keyed replay receipt is
likewise unmet, not a new token. A token with an empty fence list has no
per-model vector coverage obligation.

The caller may supply `timeout_ms`; its default is zero (one immediate proof
attempt). Waiting is bounded by 10,000 ms and must finish strictly before
the request's remaining deadline minus a 2-second margin. The implementation
polls with bounded sleeps and honors cancellation. On timeout it reports
`freshness_unmet`, not a successful stale read or an unbounded hold.

### Coverage proof

For each requested model, session success requires evidence that the same
candidate-producing read covered its fence through the served segment
watermark together with the exact-tail snapshot. The candidates and the
coverage evidence must come from one coherent read snapshot; a separate
preflight query followed by ordinary recall cannot establish the guarantee.
A skipped, capped, floored, or degraded tail leg may succeed only if the
remaining evidence still proves the fence. An exact SQLite vector scan may
serve as coverage proof only when the scan and fence evidence share that
same snapshot. Otherwise the model is named in `freshness_unmet`.

Coverage is a retrieval-source guarantee, not a promise that every covered
memory survives ranking, caller filters, or a zero limit. Acceptance uses a
distinctive matching query, permissive limit, and no excluding filters for
the immediate remember→session recall test, plus a separate test proving that
an unprovable fence refuses success.

Returning a token while bypassing the session coverage check must turn the
unprovable-fence refusal test red while that named test runs; removing the
same-snapshot requirement (a preflight query then ordinary recall) must turn
the interleaved compaction test red.

## Amendment 2 (2026-09-29): sealed receipts and legacy keyed replay

**Status**: Proposed for owner sign-off. This amendment combines the #3619
visibility-token ruling with the #3549 pre-V45 keyed-replay ruling. It
supersedes Amendment 1's public version-1 JSON receipt shape and refines its
missing-receipt result. Amendment 1's durable per-model fences, zero-model
distinction, bounded wait, and one-snapshot coverage proof remain in force.
The joint cutover below supersedes the original receipt-first, fence-second
implementation order.

### Confidentiality boundary and token contract

On the multi-tenant cloud path, a caller must not infer another namespace's
ANN write volume from gaps in the database-wide `ann_write_log.seq`. The
`visibility_token` is an opaque, self-contained AEAD ciphertext, not a clear
sequence, a hash of a sequence, or a lookup handle. This protects the memory
verb receipt surface; it does not claim isolation from a principal with direct
database or server-key access. In a local deployment where actors share an OS
user, ADR-127's key-custody limitation still applies.

Version 2 is an unpadded base64url string encoding a binary envelope of at
most 64 KiB: version byte `2`, one-byte key-ID length (1–64 bytes), key ID
using only ASCII letters, digits, `.`, `_`, or `-`, 24-byte nonce, and
ciphertext with its 16-byte authentication tag. The
key ID is a non-secret, immutable name for
one server key; it is in the header so the reader can choose a decryption key.
The client treats the entire string as opaque. No namespace, model name,
sequence, or issue time appears outside the ciphertext. Reject noncanonical
encoding, extra bytes, invalid field lengths, and oversized envelopes before
key lookup.

The encrypted plaintext is a strict length-prefixed binary record containing
the actual write `namespace`, the server-set token `issued_at` (signed 64-bit
UTC Unix milliseconds), and a sorted, unique `fences` list of
`{model, ann_write_log_seq}`. Each sequence is a fixed-width positive 64-bit
integer captured in that model's vector-write transaction; an empty list
represents a genuine zero-model write. The byte order is: two-byte big-endian
namespace length, UTF-8 namespace, eight-byte big-endian issue time, two-byte
big-endian fence count, then for each fence a two-byte big-endian model-name
length, UTF-8 model name, and eight-byte big-endian sequence. Reject trailing
bytes and invalid UTF-8. In particular,
ciphertext length must not vary with the numeric magnitude of a sequence;
plaintext decimal JSON would leak changes at powers of ten. The AEAD associated data
binds the purpose label `khive.memory.visibility`, the envelope version, and
the length-prefixed key ID. Authenticate and decrypt before interpreting any
plaintext. Then require the decrypted namespace to match the recall's
effective namespace and require its models to be within the requested model
set. The encrypted sequence is never echoed in errors, logs, metrics, or
verbose responses.

A requested model that has no fence in the token carries no coverage
obligation under that token. The fence list names every model the write
produced a vector under, and Amendment 1's expected model count keeps a
missing fence from reading as an absent one, so the write left nothing under
that model for the session to prove. That model's candidates are served as
under eventual consistency, with no wait and no `freshness_unmet` entry for
it. This is the per-model form of Amendment 1's rule that an empty fence list
has no coverage obligation, and it fixes the reading of Amendment 1's coverage
predicate: "each requested model" there means each requested model that has
a fence in the token. The requested set need not equal the token's model set,
so a caller may recall over more models than one write touched; a token model
outside the requested set remains `InvalidInput`.

Use XChaCha20-Poly1305 through RustCrypto's `chacha20poly1305` crate. Pin its
version and subject it to the existing cargo-deny advisories audit. Use a
256-bit server key and a fresh 192-bit nonce from the operating system's
cryptographic random source for every sealing operation, including a resealed
replay. Its long nonce permits independent random issuance of short receipts
without a persistent per-key counter. AES-GCM-SIV was considered but has no
interoperability or platform requirement here. A nonce failure refuses
issuance; it must not fall back to a fixed or time-derived nonce. The key must
not be reused for another protocol purpose.

The #3549 writer and #3619 source land together as one cutover, so no version-1
token is ever issued. `memory.remember` emits only version 2. A session recall
refuses a version-1 cleartext receipt as `InvalidInput` with reason
`visibility_token_legacy`; otherwise accepting an attacker-chosen clear
sequence would retain a write-rate probe through the success/refusal result.
This refusal remains a backstop for a stray or partially staged client. The
default `eventual` recall path remains unchanged.

### Key custody, rotation, and refusal

The operator provisions key bytes through a server-side secret facility, not
through the khive SQLite store or a client-visible config value. Configuration
may contain only a secret reference and its key ID. The same immutable
ID-to-key mapping must be available to every replica serving a receipt, and
must survive restart. Do not generate a replacement key at startup if the
configured key is unavailable. The hosted deployment must keep the key
outside tenant access; a keychain or config file readable by all actors under
one local OS user is not a tenant-separation boundary (ADR-127).

Rotation makes a newly provisioned ID the sole encrypting key while old IDs
remain decrypt-only for outstanding tokens. Never reuse an ID for different
bytes. The token's `issued_at` is set at sealing, including exact keyed replay;
it is never caller-supplied and is not the original write time. A replay with a
complete durable receipt may reseal the original stored fences under the
current key with a fresh `issued_at`, so the ciphertext bytes need not repeat;
it must not mint a later fence or ANN log row.

After authentication and namespace/model checks, session recall checks
plaintext `issued_at` against a maximum token age of 24 hours, including when
the fence list is empty. A read-after-write fence is needed for immediate reads
and short cross-process retries; one day covers those uses while bounding a
leaked token's useful life and the decrypt-only key ring. Allow at most five
minutes of issuer-to-verifier clock skew: a token more than five minutes in the
future is `InvalidInput`, and a token older than 24 hours in the verifier's
clock returns typed `freshness_unmet` with reason `visibility_token_expired`
and `retryable: false`. Waiting cannot restore an expired token; an exact
keyed replay with a complete durable receipt may issue a new token for the
same original fences. Never echo `issued_at` or a hidden sequence in the
refusal. After a key stops encrypting, retain its decrypt-only bytes for 24
hours plus the five-minute skew allowance after its last sealing operation
across all replicas, then it may be retired. This retention window is the
maximum token age plus skew.

An unavailable, stale, or unknown key ID refuses session consistency with
typed `freshness_unmet`, reason `visibility_key_unavailable`, and
`retryable: true`; it never becomes a successful stale read or an implicit
fallback to another key. Retryability permits key distribution or restoration
and does not promise that a permanently retired key can be recovered. A
malformed envelope, failed authentication, or decrypted foreign namespace is
`InvalidInput`, not a key-availability refusal. Do not expose whether a
recognized key's authentication failed, its bytes, or the hidden sequence.
Once a key is retired, its ciphertext cannot be decrypted to classify expiry;
an unknown ID still returns `visibility_key_unavailable`, without extending
the token's 24-hour acceptance window.

### Missing durable receipt on exact keyed replay

An exact keyed replay with a complete durable receipt returns its original
per-model fences sealed as above, including the stored zero-model case. When
the receipt is absent, the server must distinguish the original write epoch
using durable provenance independent of that receipt. A row written before
V45 (including one written under V44 and replayed after migration) has no
recoverable original fence: return terminal, non-retryable
`freshness_unmet` with typed reason `legacy_receipt_absent`. A row originally
written under V45 or later whose receipt is transiently unavailable returns
retryable `freshness_unmet` with a distinct typed reason
`receipt_temporarily_unavailable`. Neither case writes a new vector, invents
a sequence from `MAX(seq)`, or issues a token. Absence of a receipt alone,
the database's current schema version, and wall-clock age are insufficient
to classify a note: the implementation must persist a write-epoch marker
atomically with each new keyed note and preserve the legacy/modern distinction
through migration and replay.

### Acceptance

- Two namespaces can interleave repeated ANN writes. Each caller proves its
  own fence through the same candidate-producing snapshot, while its receipts
  and refusal responses reveal no intervening foreign sequence count; token
  length remains constant for the same namespace and model set as sequences
  grow through decimal digit boundaries.
- The #3549 writer and #3619 source land together; no version-1 token is
  issued. Session recall rejects a clear version-1 token, tampering, a foreign
  namespace, malformed envelope, and a token model outside the requested model
  set; an unavailable or unknown key ID gets the named retryable refusal, never
  a stale success.
- A token survives server restart and key rotation while its ID remains in
  the decrypt-only ring and its 24-hour age has not elapsed. An authenticated
  expired token gets `visibility_token_expired` with `retryable: false`; a
  token more than five minutes in the future is invalid. The retired key
  remains decrypt-only for 24 hours plus five minutes after its last issuance.
- Exact keyed replay under the current key proves the original fences with a
  fresh token issue time, without a new vector write or later inferred
  sequence.
- A V44-written keyed row without a receipt replays terminal with
  `legacy_receipt_absent`; a V45-written row with a transiently missing
  receipt remains retryable. A present zero-model receipt is neither case.
- A session recall whose requested models include one absent from the token
  succeeds on the token models' coverage alone, applying no wait and no
  `freshness_unmet` entry to the absent model.
- The one-snapshot proof, timeout cap, cancellation, and no-unproven-success
  tests from Amendment 1 remain review-blocking.

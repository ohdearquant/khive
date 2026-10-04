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

**Status**: Accepted (2026-10-01). This amendment combines the #3619
visibility-token ruling with the #3549 pre-V46 (`memory_visibility_receipts`) keyed-replay
ruling. It supersedes Amendment 1's public version-1 JSON receipt shape, refines its
missing-receipt result, narrows its coverage predicate to requested models that have a fence in
the token, and clarifies that the "original" per-model fences Amendment 1 returns on exact keyed
replay of a moved memory are its current durable fences after the move's transactional updates.
It also makes a namespace move refuse, with reason `memory_vector_left_behind`, when the move
would carry a memory's receipt or fence to a target that does not receive that memory's vector
rows. Amendment 1's durable per-model fences, zero-model distinction, bounded wait, and one-snapshot
coverage proof remain in force, subject to that clarification. The joint cutover below
supersedes the original receipt-first, fence-second implementation order.

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

### Namespace moves and stored receipt fences

A namespace move that actually re-writes a memory vector in its destination (a destination
upsert) updates that model's durable receipt fence to the exact destination upsert sequence,
captured from the upsert in the same
move transaction. The receipt follows the note without breaking its composite foreign key.
Only a vector actually written by that move may refresh its matching existing fence; an
unrelated destination vector, a later database-wide maximum, or a replay itself cannot supply
that sequence. The stored model set and explicit zero-model receipt are preserved.

For a moved memory, “original fences” in the keyed replay rule means the current durable
fences after these transactional move updates. The replay still writes no vector or log row.
Its token names the destination namespace and proves either the exact destination row in
its candidate snapshot or a published watermark covering that updated sequence. Before
publication, a present destination row can prove the exact-tail arm; after log compaction,
the covering watermark can prove the published arm.

A move refuses, changing nothing, when it would carry a memory visibility receipt or fence into
a target namespace that does not also receive the vector rows the source namespace holds for
that memory. The refusal reason is `memory_vector_left_behind`. The rule is keyed on what the
move carries, not on the shape of the move. ADR-189 specifies that every vector row moves with
its subject; the move primitive as implemented carries vector rows only when every route names
one target and reports them as left behind otherwise. Under that implementation a partitioning
move refuses while the source holds vector rows for a memory note whose receipt or fence the
move would carry. A single-target move, which carries the vector rows with the receipt, is
unaffected, and so is a memory note that has no receipt or fence. A move primitive that carries
each memory's vector rows to the target that receives its receipt does not meet the refusal
condition. The refusal is decided before any row is written, so the source and every target stay
as they were. Without it the destination would hold a receipt whose
fence it can satisfy only through the published arm: a namespace-wide published watermark may
already cover the original sequence while the vector remains in the source namespace and cannot
be retrieved from the destination. The refusal keeps Arm A's rule that session recall never
silently serves an uncovered state true by construction. Relaxing it, for example by returning
a typed `freshness_unmet` for that model in the destination, needs a later amendment with its
own proof.

### Missing durable receipt on exact keyed replay

An exact keyed replay with a complete durable receipt returns its original
per-model fences sealed as above, including the stored zero-model case. When
the receipt is absent, the server must distinguish the original write epoch
using durable provenance independent of that receipt. A row written before
V46 (`memory_visibility_receipts`), including one written under V45
(`recipient_transport`) and replayed after migration, has no
recoverable original fence: return terminal, non-retryable
`freshness_unmet` with typed reason `legacy_receipt_absent`. A row originally
written under V46 (`memory_visibility_receipts`) or later whose receipt is transiently
unavailable returns retryable `freshness_unmet` with a distinct typed reason
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
- A V45-written (`recipient_transport`) keyed row without a receipt replays terminal with
  `legacy_receipt_absent`; a V46-written (`memory_visibility_receipts`) row with a
  transiently missing receipt remains retryable. A present zero-model receipt is neither case.
- A keyed memory moves from A to B in a move that carries its vector rows and replays in B
  without another log append. Its receipt names the exact B upsert written by the move and
  session recall finds the memory both before ANN consumes that present row and after
  publication/compaction. A control that removes only the fence refresh must fail this
  regression; zero-model receipts and unrelated destination vectors retain their existing
  semantics.
- A partitioning move whose source holds a vector for a fenced memory note, with the ANN
  watermark already covering that memory's original write, refuses with
  `memory_vector_left_behind` and leaves the source and every target unchanged: the note,
  receipt, fences, and vector rows stay where they were and nothing is appended to the ANN write
  log. That fixture must fail against a move implementation that carries receipts and fences
  for every target but moves vector rows only for a single-target move, and a control that
  removes only the refusal must turn it red. A partitioning move whose source holds vector rows
  only for memory notes without a receipt or fence is not refused by this rule.
- A session recall whose requested models include one absent from the token
  succeeds on the token models' coverage alone, applying no wait and no
  `freshness_unmet` entry to the absent model.
- The one-snapshot proof, timeout cap, cancellation, and no-unproven-success
  tests from Amendment 1 remain review-blocking.

## Amendment 3 (2026-10-04): partitioned moves carry memory vectors with receipts

**Status**: Proposed. Refs #3696.

On acceptance, this amendment retires the `memory_vector_left_behind` refusal and
replaces Amendment 2's partitioned-move refusal acceptance arm. A partitioned move
carries each source memory vector to its note's target, alongside the receipt and
fences in the same backend transaction. A vector that has no unique target refuses
before writes under ADR-189 Amendment 2; partitioning does not turn that vector into
a `left_behind` row.

Only a matching existing fence is refreshed, using the exact destination upsert
sequence returned in that transaction. Receipt model counts, explicit zero-model
receipts and unrelated destination fences remain unchanged. This does not infer a
fence from a database-wide maximum, change a receipt's expected model count, or
create a fence merely because a receipt moved.

### Superseded passages

On acceptance, only the refusal mechanism, the description of partitioned vectors
being left behind, and the corresponding refusal acceptance obligation in these
three byte-exact Amendment 2 passages are superseded. They remain above as the
historical record. The opaque sealed token, namespace/model validation, exact keyed
replay with no new vector or log row, stored model set, exact upsert fences,
one-snapshot session proof, bounded wait and cancellation requirements remain in
force. The existing moved-memory session-recall acceptance before publication and
after compaction remains required.

Opening refusal sentence:

```text
It also makes a namespace move refuse, with reason `memory_vector_left_behind`, when the move
would carry a memory's receipt or fence to a target that does not receive that memory's vector
rows.
```

Refusal paragraph under “Namespace moves and stored receipt fences”:

```text
A move refuses, changing nothing, when it would carry a memory visibility receipt or fence into
a target namespace that does not also receive the vector rows the source namespace holds for
that memory. The refusal reason is `memory_vector_left_behind`. The rule is keyed on what the
move carries, not on the shape of the move. ADR-189 specifies that every vector row moves with
its subject; the move primitive as implemented carries vector rows only when every route names
one target and reports them as left behind otherwise. Under that implementation a partitioning
move refuses while the source holds vector rows for a memory note whose receipt or fence the
move would carry. A single-target move, which carries the vector rows with the receipt, is
unaffected, and so is a memory note that has no receipt or fence. A move primitive that carries
each memory's vector rows to the target that receives its receipt does not meet the refusal
condition. The refusal is decided before any row is written, so the source and every target stay
as they were. Without it the destination would hold a receipt whose
fence it can satisfy only through the published arm: a namespace-wide published watermark may
already cover the original sequence while the vector remains in the source namespace and cannot
be retrieved from the destination. The refusal keeps Arm A's rule that session recall never
silently serves an uncovered state true by construction. Relaxing it, for example by returning
a typed `freshness_unmet` for that model in the destination, needs a later amendment with its
own proof.
```

Partitioned-move refusal acceptance arm:

```text
- A partitioning move whose source holds a vector for a fenced memory note, with the ANN
  watermark already covering that memory's original write, refuses with
  `memory_vector_left_behind` and leaves the source and every target unchanged: the note,
  receipt, fences, and vector rows stay where they were and nothing is appended to the ANN write
  log. That fixture must fail against a move implementation that carries receipts and fences
  for every target but moves vector rows only for a single-target move, and a control that
  removes only the refusal must turn it red. A partitioning move whose source holds vector rows
  only for memory notes without a receipt or fence is not refused by this rule.
```

### Replacement acceptance

- `namespace_move::partition_tests::partitioned_memory_vectors_refresh_exact_fences_and_preserve_the_model_set`
  must carry the partitioned memories' vectors, receipts and fences to the same
  target. Every stored fence must equal its own destination upsert sequence even
  when a pre-existing watermark covers the old source sequence. Receipt model
  counts two, one and zero must survive; the zero-model receipt must acquire no
  fence; an unrelated resident fence must remain unchanged; composite foreign keys
  must stay valid. Independently removing fence refresh or partitioned receipt
  carry, or substituting a database-wide maximum or fixed model count, must fail
  this fixture.
- `namespace_move::partition_tests::partitioned_vectors_follow_every_source_class_and_its_sections`
  must carry byte-exact source vectors to their logical subjects' targets and
  append source-delete then destination-upsert instructions for every moved vector,
  excluding destination residents. Removing partitioned carry must fail it.
- `namespace_move::partition_tests::partitioned_orphan_and_competing_subject_routes_refuse_before_any_write`
  must retain the distinct `unroutable_vector` refusal for zero or multiple distinct
  source-subject targets, before any move write. Removing the zero-target or
  multiple-target check must fail its corresponding case.

These database fixtures establish row placement and durable fence values. They do
not replace Amendment 2's session-recall and keyed-replay acceptance or the warmed
consumer acceptance in ADR-189 Amendment 1. Seeding a covering watermark does not
establish an actual consumer's session proof.

## Amendment 4 (2026-10-04): unknown write epochs at the sealed-receipt cutover

**Status**: Proposed. Refs #3619. Acceptance is required before the dependent
sealed-receipt implementation merges.

### Deployed state and scope

The joint rollout required by Amendment 2 did not occur: [#3549](https://github.com/ohdearquant/khive/pull/3549)
merged as `83886c71b82486fbb0299fd752312bd0886960ef` on 2026-10-02 UTC while
issuing clear version-1 receipts. At that commit,
`crates/khive-db/sql/046-memory-visibility-receipts.sql` creates receipt headers
and model fences, but no independent write-epoch marker. The exact keyed replay
path in `crates/khive-runtime/src/keyed_memory.rs` cannot distinguish a pre-V46
note from a V46-or-later note whose receipt was lost. The existing
`keyed_memory_replay_refuses_when_original_receipt_is_missing` fixture in
`keyed_memory_tests.rs` demonstrates the latter possible state by removing a
receipt after a successful keyed write; it does not establish that production
data has suffered that loss.

This amendment covers keyed memory notes present when the sealed-receipt
cutover runs, including soft-deleted notes that could later be restored. A note
is identified by its immutable ID and memory kind, with its current namespace
carried as attribution; a mutable key string alone is not provenance. The
ambiguous cohort consists of notes for which neither independent durable
write-epoch evidence nor a complete original receipt is available at cutover.
It can contain both old notes and notes written after V46. No claim is made
about its size in any deployed database.

On acceptance, this amendment replaces Amendment 2's unrealized joint-landing
requirement with a coordinated upgrade to v2-only issuance and validation. It
extends the missing-receipt rule with an explicit unknown cohort and defines
how the independent marker is established during that upgrade. Amendment 2's
cryptography, key custody, v1 rejection, zero-model distinction, replay without
new vector or log writes, and one-snapshot proof remain required. Amendment 3
and its Proposed status are unchanged; this amendment decides no move-routing
policy.

### Establishing durable provenance

The cutover persists one of `legacy`, `modern`, or `unknown` for every existing
keyed memory note in the same consistent migration transaction. The marker is
stored independently of the receipt and its model fences, so removing either
cannot erase or reclassify the epoch. Use an append-only schema migration;
do not modify already-shipped V46 or allocate a migration number in this ADR.

- `legacy` requires durable evidence that the exact note already existed
  before V46. An upgrade beginning below V46 may capture the exact keyed-memory
  IDs from its coherent pre-V46 snapshot, before V46 is applied, and retain
  that inventory transactionally as the evidence. Merely observing an old
  schema version without that identity-bound inventory is insufficient.
- `modern` requires an existing independent marker for the original write,
  or a complete original receipt verified at cutover. The latter verification
  must bind the header and fences to the same note and namespace in one
  snapshot, check the expected model count and valid unique per-model fences,
  and preserve the explicit zero-model case. Persist the resulting marker in
  that transaction before serving the upgraded database. This records the
  witnessed receipt; it does not reconstruct a missing receipt or fence.
- Every remaining note is `unknown`, including absent or incomplete receipts
  with no independent epoch evidence. Contradictory evidence must not be
  silently overwritten or used to issue a token; it takes the same unknown
  refusal below. Receipt absence, `created_at`, the current database version,
  ANN log contents, and a later `MAX(seq)` cannot classify these notes.

New keyed notes written after cutover record `modern` atomically with the note,
receipt header, and vector fences. A rollback leaves none of them committed.
New code must not serve a database until its cutover has completed, and old
writers must not keep writing after that point. An interrupted migration either
rolls back or resumes from its durable provenance without substituting the
database's now-current version for the captured historical population.

The cutover reports what it classified, per database, in its own output. The
migration runner returns only the schema version it reached, so the cutover
change adds that output: for each database it upgrades, the number of keyed
memory notes classified `legacy`, `modern`, and `unknown`, and the `unknown`
count for each namespace that has one. The counts are read from the persisted
markers after classification, inside the cutover transaction, so a resumed
cutover reports its final population once rather than adding to an earlier
partial count. The report goes to the operator log of the process that applied
the migration and is never part of an error returned to a caller; no verb
exposes it.

The marker follows its note across namespace moves in the same transaction,
survives soft delete and restore, and is removed with hard deletion. Reusing a
key for another note ID cannot inherit the deleted note's epoch. Missing or
unreadable provenance at replay never defaults to `modern` or `legacy`.

### Replay disposition

Exact replay reads the epoch and receipt coherently and applies these rules:

| Durable provenance and receipt                             | Result                                                                                     |
| ---------------------------------------------------------- | ------------------------------------------------------------------------------------------ |
| `modern`, complete receipt                                 | Reseal the stored fences as v2, including a genuine zero-model receipt, under Amendment 2. |
| `modern`, absent or incomplete receipt                     | `freshness_unmet`, reason `receipt_temporarily_unavailable`, `retryable: true`.            |
| `legacy`, absent receipt                                   | `freshness_unmet`, reason `legacy_receipt_absent`, `retryable: false`.                     |
| `unknown`, missing provenance, or contradictory provenance | `freshness_unmet`, reason `receipt_epoch_unknown`, `retryable: false`.                     |

An unreadable provenance store is an availability failure, not proof of an
unknown epoch: return a retryable unavailable error without a token and without
changing the persisted classification. A legacy marker paired with a receipt
is contradictory evidence and takes `receipt_epoch_unknown`.

**What the caller holds after a refusal.** Every refusal in the table is
raised on the exact-replay path: the key claim found a live holder whose
content is identical, and the attempt's atomic unit rolled back without
writing. Each refusal therefore carries the holder's ID as `memory_id` in its
details, as the version 1 missing-receipt refusal in `keyed_memory.rs` already
does, and carries `domain_disposition: "not_committed"`. That is the
pack-raised class ADR-133 Amendment 3 (A3.1) opens with `key_conflict`: the
attempt wrote nothing and the error names the existing record. Today the
refusal has no named disposition, so it surfaces as `unknown`, and ADR-179 D5
resolves an `unknown` replay by replaying again; for a terminal reason that
loop never ends. In ADR-179's terms the replay has reconciled: the caller holds
the ID of the one memory it intended, and that memory's write committed
earlier. What it does not hold is a freshness token for that write. After
`legacy_receipt_absent` or `receipt_epoch_unknown` the caller reads `memory_id`
and is done, as after `key_conflict`; resending returns the same refusal and
the same ID, because replay never changes the marker. Such a caller has no
token to present for that memory and relies on eventual recall. After
`receipt_temporarily_unavailable` the same ID is returned, and the caller may
retry for a token knowing the write exists. A receipt found missing right after
the same attempt's own write committed is outside this rule: that attempt did
write, so its error keeps the `unknown` disposition and the caller reconciles
it by replay.

The terminal unknown refusal means repeating the same call or waiting cannot
establish the original fence. It does not authorize deletion, re-embedding,
recreating a note under its existing key, or copying a later log sequence into
its receipt. Replay never changes the marker, emits a new vector or log row,
or issues a token in a refusal case. An operator repair protocol is outside
this amendment and requires its own evidence and contract; restoring a receipt
alone cannot silently promote an unknown marker. Eventual recall remains
available under its existing contract. Errors disclose neither fence values
nor a database-wide cohort count.

### Settled points for the implementation

These four points close questions the text above leaves open. The first two
clarify sentences of Amendment 2 without changing its security properties.

- **Bytes after the nonce.** The version 2 envelope carries no ciphertext
  length, so every byte after the nonce belongs to the ciphertext and its tag.
  Amendment 2's rule to reject extra bytes before key lookup therefore applies
  to what the envelope delimits: canonical unpadded base64url, the version
  byte, the key-ID length and alphabet, the 24-byte nonce, a ciphertext no
  shorter than its 16-byte tag, and the 64 KiB bound. Bytes appended to the
  ciphertext cannot be told apart from ciphertext before authentication; they
  fail authentication. Trailing bytes inside the authenticated plaintext fail
  strict decoding. No length field is added to the envelope.
- **Effective namespace.** The decrypted namespace must be a member of the
  recall's effective read set: the one namespace an explicit request names,
  once authorized, or otherwise every namespace the caller's visibility admits
  for that recall. The set is computed from authorization before the token is
  opened, so a token cannot widen it. A default recall that reads the caller's
  local and actor namespaces accepts a receipt from either; an explicit request
  for one namespace refuses a receipt from another. This is the behaviour the
  clear version 1 path already has.
- **Which writers establish `modern`.** Only the runtime unit that creates a
  keyed memory note together with its receipt header and fences writes the
  `modern` marker. That covers the specialized memory writer, generic keyed
  note creation with kind `memory`, and new keyed members of a stream batch.
  Lower-level storage constructors that accept a caller-supplied note, the
  exported note statement builders, and transport commits have no model or
  fence context; they write no marker. A keyed memory note they create has
  missing provenance and takes the terminal unknown refusal on exact replay,
  while eventual recall is unchanged. No caller can assert `modern` through a
  note field, and a current binary performing an insert is not evidence of an
  original write.
- **Where the pre-V46 inventory is captured.** Capture runs inside the
  transaction that applies V46, after the runner's under-lock version checks
  and before the unchanged V46 SQL executes. The inventory and the V46 ledger
  row commit together, and the write lock that transaction holds excludes
  other writers while the population is read. A database that already records
  V46 skips capture: its keyed notes without a complete receipt become
  `unknown`. The V46 SQL bytes and ledger identity stay unchanged; the
  transaction gains one effect, defined by a new SQL file that the cutover
  change owns. A separate pre-upgrade capture transaction was rejected: it is
  sound only if every old writer stays stopped across all later version steps,
  and it adds a preparation journal with its own lifecycle.

### Production callers of the routes that write no marker

The routes that write no marker are the storage constructors `upsert_note`,
`insert_note_if_absent`, `try_insert_note`, `try_insert_note_with_attachments`,
`upsert_notes` and `batch_upsert_notes` in `crates/khive-db/src/stores/note.rs`
(also reached through the policy wrapper that `runtime.notes(token)` returns,
in `crates/khive-runtime/src/note_store_guard.rs`), the insert statement
builders `note_upsert_statement`, `note_insert_if_absent_statement` and
`note_insert_keyed_statement` in the same file, and
`RecipientTransportStore::commit` in
`crates/khive-db/src/stores/note/recipient/mod.rs`. Every production caller of
these at `e59b98b99bbafc09f6590b442ec165eff7b0d925`, outside test modules, is
listed below with its disposition.

| Caller (path, function)                                                                                                                                         | What it writes                                                        | Disposition                                                                                                                            |
| --------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/khive-runtime/src/operations.rs`, `create_note_inner` (generic note creation)                                                                           | A new note built with `Note::new`; the function takes no key          | Cannot create keyed memory                                                                                                             |
| `crates/khive-runtime/src/operations.rs`, `try_create_note_impl` (trusted channel ingest)                                                                       | A new note built with `Note::new`, no key                             | Cannot create keyed memory                                                                                                             |
| `crates/khive-pack-comm/src/handlers.rs`, `handle_heartbeat`                                                                                                    | Kind `channel_health`, no key                                         | Cannot create keyed memory                                                                                                             |
| `crates/kkernel/src/code_ingest.rs`, `persist_ingest_note` (code findings ingest)                                                                               | Kind `finding`, no key                                                | Cannot create keyed memory                                                                                                             |
| `crates/khive-runtime/src/atomic_prepare.rs`, `prepare_add_note` (proposal apply)                                                                               | A new note built with `Note::new`, no key                             | Cannot create keyed memory                                                                                                             |
| `crates/khive-runtime/src/keyed_message.rs`, `create_keyed_message_pair_with_attachments`                                                                       | Kind restricted to `message`                                          | Cannot create keyed memory                                                                                                             |
| `crates/khive-runtime/src/atomic_message.rs`, `prepare_atomic_note_requests`, and `crates/khive-runtime/src/note_create.rs`, `prepare_note_create`              | New notes of any kind, keyed or not, prepared inside the runtime unit | The runtime unit. No marker exists anywhere at this commit; after the cutover change a keyed memory note created here records `modern` |
| `crates/khive-runtime/src/comm_recipient.rs`, `ingest_verified_recipient` (recipient transport commit; called only from tests and a doc example at this commit) | Kind `message`, no key                                                | Cannot create keyed memory                                                                                                             |

The other exported note builders change or remove an existing row and create
no identity: `note_replace_if_unchanged_statement` and
`note_metadata_replace_if_unchanged_statement` (called from
`prepare_versioned_note_update` in `crates/khive-runtime/src/note_write.rs`),
and `note_soft_delete_statement` and `note_hard_delete_statement` (called from
`prepare_delete` in `crates/khive-runtime/src/atomic_prepare.rs` and
`delete_note_with_post_commit_report` in
`crates/khive-runtime/src/operations.rs`). Hard deletion removes the marker
with its note. The replace statement and the upsert statement's conflict branch
both set `kind`, so a route could turn an existing keyed row into kind
`memory`. Such a row is not an original memory write: it carries no marker and
takes the unknown refusal on exact replay.

The routes that move or revive an existing note create no identity and keep
its marker: `move_kinded_subject` in `crates/khive-db/src/namespace_move.rs`
(namespace move target), `restore_note` in
`crates/khive-runtime/src/operations.rs` (undelete), and `merge_note_sql` in
`crates/khive-runtime/src/curation.rs`, which writes onto a destination ID that
`read_merge_note` has already read and does not transfer the source's marker.
Import does not reach these routes for memory: `import_kg` in
`crates/khive-runtime/src/portability.rs` reads archives of entities and edges
only. The tree has no database restore-from-backup operation; a copy of the
database file carries the marker table with its notes, so every note keeps its
class.

So at this commit no production caller creates a new keyed memory note through
a route that writes no marker. The disposition for the routes themselves is
unknown forever, by intent: they have no model or fence context, and a library
caller that hands them a `Note` with kind `memory` and a key gets a note that
refuses as `receipt_epoch_unknown` on exact replay. A production route that
needs to create keyed memory, including a hosted ingest service that embeds
this runtime outside this repository, goes through the runtime unit.

Inside the runtime unit, three routes create keyed notes today without the
receipt flag: `create_note_with_options` and
`create_note_with_options_resolving_annotations` in
`crates/khive-runtime/src/note_write.rs`, and new keyed members of
`stream.batch` in `crates/khive-runtime/src/streams.rs`. When the memory pack
is registered, its creation hook in `crates/khive-pack-memory/src/hook.rs`
refuses kind `memory` on the shared `create` handler, which runs the hook
before it calls `create_note_with_options_resolving_annotations`, and on
`stream.batch`. The two other production callers of `create_note_with_options`
create fixed kinds: `handle_assign` in `crates/khive-pack-gtd/src/handlers.rs`
creates `task`, and `record_outbound_external_id_diagnostic` in
`crates/khive-runtime/src/curation.rs` creates `observation`. So in a served
daemon only `memory.remember` creates keyed memory. A library caller of
`create_note_with_options` is not behind the hook, which is why these routes
are among the writers that must write `modern`, as the settled point above
says.

### Acceptance before implementation merge

- Upgrade actual pre-V46 data and actual already-V46 data through the migration
  runner. Capture the former population before V46 is applied. In the latter,
  include a keyed write whose receipt is deliberately removed before cutover:
  it must become `unknown`, never inferred legacy or modern. Include an
  incomplete receipt, a complete positive-model receipt, and an explicit
  zero-model receipt. Preserve all note, vector, and ANN log rows and values.
- A known modern marker with a subsequently removed header or model fence
  remains modern and produces the retryable missing-receipt result. The
  otherwise identical unknown cohort produces the terminal unknown result.
  Removing only the independent-marker discrimination must fail this test.
- A captured legacy note stays legacy after upgrade and restart. Restarting
  an interrupted upgrade must neither broaden the captured population nor
  overwrite an unknown marker. A control that classifies missing receipts
  from current schema version or note age must fail the unknown-cohort case.
- Known modern complete receipts reseal without changing original fences,
  model counts, vectors, log counts, or markers. Zero-model receipts remain
  distinct from incomplete ones. An unknown marker with a later restored
  receipt still refuses; removing only that refusal must fail the fixture.
- Namespace move, soft delete/restore, hard delete, and key reuse preserve the
  identity-bound provenance rules. An unreadable provenance store must refuse
  without persisting `unknown`; test it separately from a missing marker.
- A crash after the V46 transaction commits and before the cutover leaves the
  captured inventory intact, and resuming classifies exactly that population;
  holding the captured identities only in process memory must fail this test.
  A failure injected after capture and before the V46 ledger row commits leaves
  neither the inventory nor V46 committed; committing the capture in its own
  earlier transaction must fail that test.
- A keyed memory note created through a lower-level storage constructor has
  no marker and refuses as `receipt_epoch_unknown` on exact replay. The same
  note created through the runtime keyed writer is `modern` and reseals. A
  keyed note of another kind that is replaced in place with kind `memory` also
  refuses as `receipt_epoch_unknown`.
- Exact replay through the request envelope of an `unknown` keyed memory, and
  separately of a `legacy` one without a receipt, returns its refusal with the
  holder's ID as `memory_id` and `domain_disposition: "not_committed"`. A
  second identical replay returns the same refusal and ID, and note, vector,
  ANN log, edge, receipt and marker rows are unchanged across both. Removing
  the named disposition, so that the refusal surfaces as `unknown`, must fail
  this test.
- Upgrading a database seeded with a known mix of `legacy`, `modern` and
  `unknown` keyed memories across two namespaces reports exactly those three
  counts and the per-namespace `unknown` counts. An interrupted and resumed
  cutover reports the same values once. No replay refusal carries any of them.
- A source census of production callers of the routes that write no marker
  reproduces the caller table above, and a new production caller fails it
  until the table and that caller's disposition are updated. The census
  asserts a non-empty caller set, so a pattern that silently matches nothing
  fails instead of passing. It extends an existing source census in the tree
  rather than adding a new mechanism.
- A default recall accepts a token whose namespace is in its visible set and
  refuses one outside it; an explicit one-namespace request refuses a token
  from another visible namespace. A byte appended to a valid envelope fails
  authentication, and a valid envelope whose plaintext carries a trailing byte
  fails strict decoding.
- Run the inherited v2-only confidentiality, tampering, key rotation, expiry,
  keyed replay, and session proof acceptance from Amendment 2. A clear v1 token
  must still be denied. These outputs and the unknown-cohort controls are
  required evidence for sign-off; a docs-only proposal does not execute them.

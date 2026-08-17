# Ranked-prefix materialization

`materialize_ranked_prefix` is a policy-free controller for a bounded candidate sequence that the
caller has already placed in one strict total order. It does not search, authorize, hydrate blobs,
or interpret records.

The caller supplies four policy seams:

1. `order_key` exposes the existing strict total order for preflight validation. Keys must be
   strictly increasing; descending score order can use `Reverse(score)` followed by a stable ID.
2. `candidate_validator` validates candidates in order immediately before each loader batch.
3. `batch_loader` receives at most the configured nonzero batch size and returns keyed rows in any
   order. Missing keys are valid; unexpected, duplicate, or excess rows are structural errors.
4. `classifier` receives each candidate plus its optional correlated row and returns `Keep`,
   `Drop`, or `Fatal`.

Candidate keys must be unique. The controller validates request ceilings, the closed ordinal drop
taxonomy, uniqueness, and strict ordering before validator or loader work. Returned loader rows are
fully correlated and structurally checked before any candidate in that batch is classified. Error
precedence is deterministic for malformed batches: an excess row count wins first; within the
requested count, an unexpected key wins over a duplicate key regardless of return order.

`Keep` preserves the original score and order and assigns the next compact one-based rank. `Drop`
updates a fixed 32-slot counter array and retains only the first configured number of typed details;
`diagnostics_truncated` records omitted details without changing materialization. `Fatal`,
candidate-validator errors, and loader errors are returned as `MaterializationError::Caller(E)`
without string conversion.

Classification stops at the Kth `Keep`. Any later rows already loaded in that batch are ignored;
the remaining candidate tail is still validated in order, but no further loader or classifier call
occurs. A zero output limit therefore validates the whole supplied tail with zero loader I/O.

Portable v1 maxima are 4,096 candidates, 256 rows per loader batch, 4,096 accepted outputs, 4,096
retained diagnostic details, and 32 declared drop reasons. `MaterializationLimits` lets a consumer
lower the first four ceilings. Generic key, score, row, output, reason, and error types remain caller
owned; the controller clones only the bounded loader key batch.

This API implements proposed ADR-160 D5. It is reviewable in a draft implementation PR, but must not
merge until the ADR is ratified.

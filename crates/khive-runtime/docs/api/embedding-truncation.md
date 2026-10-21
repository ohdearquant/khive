# Document embedding truncation

Document embedding outcomes report two independent limits. The runtime first
bounds the document to its UTF-8 byte budget, reserving space for the model's
passage instruction. It then requests passage-role embeddings with the provider's
per-input token report. A document is `truncated` if either limit shortened it.
The stored source content remains unchanged.

`DocumentEmbeddingOutcome` retains `source_bytes` and `embedded_bytes` for the
runtime byte bound. `provider_token_count`, when present, contains
`before_truncation` and `embedded` token counts from the provider. Those counts
include special tokens and any passage instruction. They describe the already
byte-bounded document, not the original text removed by the byte limit. Byte
counts do not estimate token counts, and token counts do not change the existing
prepared-input fingerprint.

A provider returning `EmbedError::Unsupported` falls back to its existing
`embed_passage` method with exactly the same bounded input and model. Its token
count is `None`, meaning unknown, rather than a claimed zero-token loss. Other
provider errors propagate without a vector-only retry. Reporting providers must
return one vector and one valid count per input; mismatched lengths or an
embedded count greater than the pre-truncation count fail the whole call.

The same rules apply to single documents and batches. Batch input order and
batching are preserved. Empty batches do not call a provider. The vector-only
batch convenience method refuses a known byte or token cut and points callers to
the outcome-bearing method.

## Aggregate reports

`EmbeddingTruncationReport` exposes:

| Field                         | Meaning                                                    |
| ----------------------------- | ---------------------------------------------------------- |
| `truncated`                   | Outcomes cut by either limit, counted once when both apply |
| `discarded_bytes`             | Bytes removed by the runtime's input bound                 |
| `discarded_tokens`            | Tokens removed from provider-reported, byte-bounded inputs |
| `token_reporting_unavailable` | Outcomes whose provider does not support token reporting   |

Observation and merging saturate counters rather than wrapping. Old serialized
reports deserialize with zero for the two new counters. Zero-valued new counters
are omitted from serialized reports, preserving the empty-report shape. Defaulted fields
cannot reconstruct whether historical providers supported reporting.

Existing response warnings that use `any_truncated()` now cover provider token
cuts as well as byte cuts. Availability is explicit in runtime outcomes and
aggregate reports. This change does not add an availability field to every pack
response, a caller-selectable reject policy, or durable truncation metadata.

## Native worker and cache behavior

Built-in provider token counting uses the same bounded native worker as inference,
including its input, queue and in-flight byte limits. Tokenization is never run
directly on an async request thread. Cancelled queued jobs are skipped.

The upstream result cache reuses vectors but recomputes token counts on every
reporting call. A warm document vector therefore still needs worker admission to
obtain its report. Vector-only cache calls, including query embeddings, keep their
existing admission bypass on a cache hit.

One original admission deadline covers the count and vector phases. Work already
admitted is allowed to finish; if the next phase reaches admission after expiry,
it returns the existing admission timeout without starting that phase. The
supervisor continues observing phase transitions rather than abandoning deadline
checks after the first accepted job.

# Retrieval response evidence

`RetrievalEnvelope<Hit>` pairs existing serializable wire hits with a
`RetrievalEvidence` record. It is an additive foundation for Proposed ADR-031
Amendment 5; existing APIs and responses are unchanged. Request parsing, configured
defaults, validation and runtime/handler integration remain separate work.

`RetrievalStrategy` composes the existing `FusionStrategy` values. Its wire form
describes an already resolved plan: weighted engine fusion includes the effective
positional weights. It is not the request example that omits those weights for
name-based resolution. There is no serde deserializer or implicit default here.

Producers validate the plan before embedding or search and report canonical engine
order for requested, selected, used and per-engine records. `requested_engines:
null` means the request omitted a subset; `[]` preserves an explicit empty subset.
Used engines completed successfully, including those returning zero hits. A failed or unavailable
engine remains selected with its own disposition and reason. Explicit unweighted
RRF reports effective engine weights of one.

Each vector engine and the text arm reports requested and returned candidate
counts. `outcome.status` is `ran`, `skipped`, `unavailable` or `failed`; the latter
three also carry a reason. Thus a measured zero is distinct from a skipped or
failed arm. A keyword-only response has a null engine strategy and score kind,
empty engine lists and evidence for the text arm. Both fusion stages disclose
their score kind; maximum-score union is not labelled scale independent or as a
probability.

The proposed evidence representation is version 1. These structs serialize
observations; they do not assert plan validity or automatically infer degradation.
Producers supply finite positive effective weights, consistent participation and
counts, and the declared custom score kind. They also keep raw query vectors out
of result values and free-form reasons. The diagnostic model itself has no query
vector field.

Serialization fixtures cover populated, empty, degraded and keyword-only results,
canonical order, the omitted versus empty subset distinction, all arm statuses and
custom score-kind evidence. Native compilation and fixture execution are pending
owner validation.

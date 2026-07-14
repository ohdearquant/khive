# Recall Scoring

Design rationale and algorithmic detail extracted from `crates/khive-pack-memory/src/scoring.rs`
doc-comments. The doc-comments in source remain the complete API contract; this file is
background only.

## Entity Candidate Extraction

`extract_entity_candidates` (khive #dead-parameter defect): `entity_names` was originally a
caller-supplied request field feeding the `EntityMatch` adjustment, but no caller ever populated
it, so the ×1.3 boost in `default_adjustments` never fired in practice. The function derives
candidates server-side from the query text instead, so the boost has something to match against.

Capitalization is the only signal used because it is the sole low-noise proper-noun indicator
available without an NER model or a lookup against known entity records. `EntityMatch::matches`
does a free-text boundary-anchored match against raw memory content — not something anchored to
actual KG entity references — so admitting ordinary lowercase content words as candidates
degenerates the boost into a second, redundant lexical-overlap signal on top of retrieval-stage
relevance. Review confirmed that realistic score inputs clamp at the `[0, 1]` ceiling and flatten
top-rank ordering when generic query words are treated as entity candidates.

Many recall callers — agents in particular — pass fully lowercase queries. The function
deliberately returns an empty list for them rather than guessing at content words. Covering that
case precisely would require anchoring candidates against known entity records (e.g. resolving
query tokens against the KG) rather than lexical heuristics over the query string; that is out of
scope for this function, and is instead addressed by `entity_lookup_candidates` below.

## Entity Lookup Candidate Sampling

`entity_lookup_candidates` (ADR-104 §5 / Stage C, rider R1) exists because the capitalization
gate above returns nothing for lowercase queries, and precision there instead comes from the
caller (`memory.recall`) only keeping a candidate that matches the *name* of a real KG entity via
one batched `EntityFilter::names_ci` lookup — so this function can afford to sample broadly.

Sampling algorithm: CJK substrings reserve a fair quota for every supported length from
`MIN_CJK_LOOKUP_CHARS` (2) through `MAX_CJK_LOOKUP_CHARS` (8), redistributing unused quota from
short runs that don't have enough substrings to fill their share. Within each length, available
start positions are sampled evenly: a quota greater than one guarantees both the first and final
valid starts are included; a quota of exactly one selects the first endpoint. The result is both
length-fair (no single substring length dominates) and position-fair (no single region of the
query dominates) under the `MAX_ENTITY_LOOKUP_CANDIDATES` (64) cap.

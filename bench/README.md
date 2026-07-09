# bench/

Small, hand-curated evaluation fixtures for retrieval/scoring work.

`recall_eval.jsonl` — one JSON object per line, each row a synthetic mirror of
a private eval case used while validating `memory.recall`'s scoring pipeline.
Rows are deliberately generic (no real names, no memory UUIDs, no private
content) so they're safe to commit to this public repo; they exercise the
same *shape* of query/content distinction as the private case they mirror
(e.g. "a query naming specific proper nouns should outrank generic-content
distractors that don't mention them").

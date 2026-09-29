-- Provenance for dynamically named vec0 tables. A missing row represents a
-- vector written before provenance was recorded.
CREATE TABLE IF NOT EXISTS vector_provenance (
    model_key TEXT NOT NULL,
    subject_id TEXT NOT NULL,
    namespace TEXT NOT NULL,
    embedding_digest TEXT NOT NULL,
    text_fingerprint TEXT,
    updated_at TEXT,
    PRIMARY KEY (model_key, subject_id),
    CHECK (text_fingerprint IS NULL OR (
        typeof(text_fingerprint) = 'text'
        AND length(CAST(text_fingerprint AS BLOB)) = 64
        AND instr(CAST(text_fingerprint AS BLOB), x'00') = 0
        AND text_fingerprint NOT GLOB '*[^0-9a-f]*'
    )),
    CHECK (typeof(embedding_digest) = 'text'
        AND length(CAST(embedding_digest AS BLOB)) = 64
        AND instr(CAST(embedding_digest AS BLOB), x'00') = 0
        AND embedding_digest NOT GLOB '*[^0-9a-f]*')
);

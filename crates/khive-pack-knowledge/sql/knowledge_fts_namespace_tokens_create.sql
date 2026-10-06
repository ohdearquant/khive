CREATE VIEW knowledge_fts_namespace_tokens AS
SELECT namespace_value AS namespace,
       char(57344 + slot % 6400,
            57344 + (slot / 6400) % 6400,
            57344 + (slot / 40960000) % 6400) AS namespace_key
FROM knowledge_fts_namespace_keys;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const CONVERTED: &[&str] = &[
    "khive-pack-brain",
    "khive-pack-comm",
    "khive-pack-git",
    "khive-pack-gtd",
    "khive-pack-kg",
    "khive-pack-knowledge",
    "khive-pack-memory",
    "khive-retrieval",
    "kkernel",
];

/// The SQL spelling predicate deliberately does not parse arbitrary assembled SQL.
#[path = "../../../../scripts/architecture-audit/src/scanner.rs"]
mod scanner;
use scanner::{
    inline_errors, reference_errors, sql_literals, still_inline_seen, strip_test_modules, Keep,
};

pub(super) fn check(sources: Vec<(PathBuf, String)>, crates_root: &Path) {
    assert!(!sources.is_empty(), "source inventory is empty");
    let sources = sources.into_iter().collect::<BTreeMap<_, _>>();
    let mut assets = BTreeSet::new();
    for entry in std::fs::read_dir(crates_root).expect("read crates directory") {
        let entry = entry.expect("read crate entry");
        if !entry.file_type().expect("read crate entry type").is_dir() {
            continue;
        }
        let dir = entry.path().join("sql");
        match std::fs::metadata(&dir) {
            Ok(metadata) => assert!(
                metadata.is_dir(),
                "SQL path is not a directory: {}",
                dir.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => panic!("inspect {}: {e}", dir.display()),
        }
        for entry in std::fs::read_dir(&dir).expect("read SQL directory") {
            let path = entry.expect("read SQL entry").path();
            if path.extension().is_some_and(|e| e == "sql") {
                let sql = std::fs::read_to_string(&path).expect("read SQL asset");
                assert!(
                    !sql.trim().is_empty(),
                    "empty SQL asset: {}",
                    path.display()
                );
                assets.insert(path);
            }
        }
    }
    assert!(!assets.is_empty(), "SQL asset inventory is empty");
    for name in CONVERTED {
        assert!(
            assets.iter().any(|p| p.starts_with(crates_root.join(name))),
            "converted crate {name} contributed no SQL assets"
        );
    }
    let mut errors = reference_errors(&sources, &assets, crates_root);
    errors.extend(inline_errors(
        &sources,
        crates_root,
        CONVERTED,
        INLINE_KEEPS,
    ));
    assert!(
        errors.is_empty(),
        "SQL policy violations:\n{}",
        errors.join("\n")
    );
    let still_inline = still_inline_seen(&sources, &crates_root.join("khive-db"));
    assert!(
        still_inline,
        "must-match control: khive-db no longer trips the inline SQL predicate"
    );
    // Reinsert every extracted statement in all three Rust literal spellings.
    // This is independent of reference discovery and precise inline exceptions.
    let mut round_tripped = 0usize;
    for file in &assets {
        if !CONVERTED
            .iter()
            .any(|name| file.starts_with(crates_root.join(name)))
        {
            continue;
        }
        let statement =
            std::fs::read_to_string(file).unwrap_or_else(|e| panic!("read {file:?}: {e}"));
        // Headers are not part of the statement rendered back into Rust.
        let body = statement
            .lines()
            .skip_while(|line| {
                let start = line.trim_start();
                start.is_empty() || start.starts_with("--")
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !body.trim().is_empty(),
            "{file:?} holds nothing but comments, so it declares no statement for \
                     the census to protect"
        );
        // A quoted identifier would otherwise close the synthetic literal early
        // and fail this control for a reason that has nothing to do with it.
        let statement = body.trim().replace('"', "\\\"");
        let shapes = [
            (
                "one line",
                statement.split_whitespace().collect::<Vec<_>>().join(" "),
            ),
            ("escaped newlines", statement.replace('\n', "\\n")),
            (
                "line continuations",
                statement.replace('\n', " \\\n            "),
            ),
        ];
        for (shape, rendered) in shapes {
            let snippet = format!("let statement = \"{rendered}\";");
            let seen = sql_literals(&snippet);
            assert_eq!(
                seen.len(),
                1,
                "must-fail control: {file:?} written back into Rust as {shape} was \
                         seen {} time(s), so the census would not notice this statement \
                         moving home",
                seen.len()
            );
            round_tripped += 1;
        }
    }
    assert!(
        round_tripped > 0,
        "must-fail control ran on nothing: {CONVERTED:?} contributed no .sql files, so \
             its passing says only that the loop body never executed"
    );
}

const INLINE_KEEPS: &[Keep] = &[
    Keep { name: "property_cas_notes", path: "khive-db/src/statement_lists.rs", sql: "UPDATE notes SET properties = json_set(COALESCE(properties, '{}')", reason: "Statement prefix for the closed notes arm; the JSON SET and guard arity varies per call and every JSON path and value is bound, with caller-owned lifecycle fields." },
    Keep { name: "property_cas_entities", path: "khive-db/src/statement_lists.rs", sql: "UPDATE entities SET properties = json_set(COALESCE(properties, '{}')", reason: "Statement prefix for the closed entities arm; the JSON SET and guard arity varies per call and every JSON path and value is bound, with caller-owned lifecycle fields." },
    Keep { name: "runtime_pack_table_prefix", path: "khive-runtime/src/operations/resolve_prefix_in.rs", sql: "SELECT DISTINCT candidate.{id_column} AS resolved_id FROM {table} AS candidate WHERE candidate.{id_column} >= ?1 AND candidate.{id_column} < ?2{filters} ORDER BY candidate.{id_column} LIMIT 2", reason: "Static pack table and ID column are validated and quoted at runtime; prefix bounds and namespace are bound values; filters are fixed clauses." },
    Keep { name: "memory_ann_protected_tail", path: "khive-pack-memory/src/ann/watermark_log.rs", sql: "WITH tail AS MATERIALIZED ( SELECT seq, subject_id, op FROM ann_write_log WHERE embedding_model = ?1 AND kind = 'note' AND field = 'note.content' AND seq > ?2 ORDER BY seq LIMIT ?5 ), summary AS MATERIALIZED ( SELECT COUNT(*) AS raw_count, ({minimum}) AS min_watermark FROM tail ), selected AS MATERIALIZED ( SELECT seq, subject_id, op FROM tail WHERE (SELECT raw_count FROM summary) <= ?4 ) SELECT 0 AS is_summary, summary.raw_count, summary.min_watermark, NULL AS seq, NULL AS subject_id, NULL AS op, NULL AS vector_model, NULL AS vector_kind, NULL AS vector_field, NULL AS embedding, NULL AS live_note_id FROM summary UNION ALL SELECT 1 AS is_summary, NULL AS raw_count, NULL AS min_watermark, selected.seq, selected.subject_id, selected.op, vectors.embedding_model AS vector_model, vectors.kind AS vector_kind, vectors.field AS vector_field, vectors.embedding, live_note.id AS live_note_id FROM selected LEFT JOIN {table_name} AS vectors ON vectors.subject_id = selected.subject_id LEFT JOIN notes AS live_note ON live_note.id = selected.subject_id AND live_note.deleted_at IS NULL ORDER BY is_summary, seq", reason: "Trusted vector table identifier is embedded in the capped tail and registry snapshot statement." },
    Keep { name: "memory_session_exact_snapshot", path: "khive-pack-memory/src/ann/session_exact.rs", sql: "WITH session_proof AS MATERIALIZED ( SELECT ({proof}) AS has_fence), {knn_ctes}session_union AS MATERIALIZED ({union}), session_ranked AS MATERIALIZED ( SELECT c.subject_id, c.distance FROM session_union c JOIN notes n ON n.id = c.subject_id AND n.namespace = c.vector_namespace AND n.deleted_at IS NULL ORDER BY c.distance, c.subject_id LIMIT ?2) SELECT p.has_fence, r.subject_id, r.distance FROM session_proof p LEFT JOIN session_ranked r ON 1 = 1 ORDER BY r.distance, r.subject_id", reason: "Fence predicate, per-namespace exact-scan CTEs and union arms are assembled for one snapshot." },
    Keep { name: "session_union_arm", path: "khive-pack-memory/src/ann/session_exact.rs", sql: "SELECT subject_id, vector_namespace, exact_distance AS distance FROM session_knn_{index}", reason: "Per-namespace CTE identifier contains the runtime index." },
    Keep { name: "population_base_count", path: "khive-pack-memory/src/pack.rs", sql: "SELECT COUNT(*) AS cnt FROM {base_table} WHERE deleted_at IS NULL", reason: "Base table identifier is selected at runtime." },
    Keep { name: "population_fts_count", path: "khive-pack-memory/src/pack.rs", sql: "SELECT COUNT(*) AS cnt FROM {fts_table}", reason: "FTS table identifier is selected at runtime." },
    Keep { name: "prune_candidates", path: "khive-pack-memory/src/handlers/prune.rs", sql: "SELECT {projection} FROM notes WHERE kind = 'memory' AND namespace = ? AND deleted_at IS NULL", reason: "Projection depends on the requested pruning mode." },
    Keep { name: "knowledge_compose_section_window", path: "khive-pack-knowledge/src/knowledge/compose.rs", sql: "SELECT id, atom_id, section_type, heading, content, embedding FROM knowledge_sections WHERE namespace = ?1 AND atom_id IN ({placeholders}) AND {SERVABLE_SECTION}", reason: "Variable atom-ID placeholder list and shared servable-section predicate." },
    Keep { name: "knowledge_feedback_target_table", path: "khive-pack-knowledge/src/knowledge/crud.rs", sql: "SELECT id FROM {table} WHERE id = ?1 AND deleted_at IS NULL LIMIT 1", reason: "Atom/domain table identifier selected at runtime." },
    Keep { name: "knowledge_domain_offset_projection", path: "khive-pack-knowledge/src/knowledge/crud.rs", sql: "SELECT {select_columns} FROM knowledge_domains WHERE namespace = ?1 AND deleted_at IS NULL ORDER BY created_at DESC, id DESC LIMIT ?2 OFFSET ?3", reason: "Key-only versus full domain projection selected at runtime." },
    Keep { name: "knowledge_atom_offset_projection_status", path: "khive-pack-knowledge/src/knowledge/crud.rs", sql: "SELECT {select_columns} FROM knowledge_atoms WHERE namespace = ?1 AND deleted_at IS NULL AND NOT khive_tag_contains(tags, 'type:domain'){} ORDER BY created_at DESC, id DESC LIMIT ?2 OFFSET ?3", reason: "Selected projection and optional status clause." },
    Keep { name: "knowledge_atom_count_status", path: "khive-pack-knowledge/src/knowledge/crud.rs", sql: "SELECT COUNT(*) FROM knowledge_atoms WHERE namespace = ?1 AND deleted_at IS NULL AND NOT khive_tag_contains(tags, 'type:domain'){}", reason: "Optional status clause." },
    Keep { name: "knowledge_delete_domain_head", path: "khive-pack-knowledge/src/knowledge/crud.rs", sql: "SELECT id, slug FROM knowledge_domains", reason: "Incomplete domain projection passed to key_match_statements; runtime key column, namespace predicate and IN binds follow." },
    Keep { name: "knowledge_delete_atom_head", path: "khive-pack-knowledge/src/knowledge/crud.rs", sql: "SELECT id, slug, tags FROM knowledge_atoms", reason: "Incomplete atom projection passed to key_match_statements; runtime key column, namespace predicate and IN binds follow." },
    Keep { name: "knowledge_delete_atom_update_head", path: "khive-pack-knowledge/src/knowledge/crud.rs", sql: "UPDATE knowledge_atoms SET deleted_at = ?2", reason: "Incomplete UPDATE passed to key_match_statements; runtime key column, namespace predicate and IN binds follow." },
    Keep { name: "knowledge_cursor_page", path: "khive-pack-knowledge/src/knowledge/cursor_query.rs", sql: "SELECT {columns} FROM {table} WHERE namespace = ?1 AND deleted_at IS NULL {atom_filter}{seek}{status_clause} ORDER BY created_at ASC, id ASC LIMIT {limit}", reason: "Table, projection, status/seek clauses and bound limit assembled at runtime." },
    Keep { name: "knowledge_refusal_target", path: "khive-pack-knowledge/src/knowledge/refusal.rs", sql: "SELECT a.* FROM knowledge_atoms a WHERE {predicate} AND a.deleted_at IS NULL AND NOT EXISTS (SELECT 1 FROM knowledge_domains d WHERE d.id = a.id) LIMIT 1", reason: "ID or namespace/slug predicate selected from the refused input." },
    Keep { name: "knowledge_proto_fts_rowids", path: "khive-pack-knowledge/src/knowledge/search/lexical.rs", sql: "SELECT rowid FROM {table} WHERE {table} MATCH ?1 ORDER BY rowid LIMIT ?2", reason: "Test-only prototype chooses its FTS table at runtime." },
    Keep { name: "knowledge_proto_term_frequency", path: "khive-pack-knowledge/src/knowledge/search/lexical.rs", sql: "SELECT count(*) AS frequency FROM ( SELECT rowid FROM {table} WHERE {table} MATCH ?1 ORDER BY rowid LIMIT ?2 )", reason: "Test-only prototype chooses its FTS table at runtime." },
    Keep { name: "knowledge_fts_scoped_candidates", path: "khive-pack-knowledge/src/knowledge/search/lexical.rs", sql: "SELECT a.* FROM fts_knowledge CROSS JOIN knowledge_atoms AS a ON a.rowid = fts_knowledge.rowid WHERE fts_knowledge MATCH ?1 AND +a.namespace = ?2 AND a.deleted_at IS NULL{scoped_status_clause}{type_clause} ORDER BY fts_knowledge.rowid LIMIT ?3", reason: "Status and atom/domain eligibility clauses selected at runtime." },
    Keep { name: "knowledge_proto_fts_scoped_candidates", path: "khive-pack-knowledge/src/knowledge/search/lexical.rs", sql: "SELECT a.* FROM {table} CROSS JOIN knowledge_atoms AS a ON a.rowid = {table}.rowid WHERE {table} MATCH ?1 AND +a.namespace = ?2 AND a.deleted_at IS NULL{scoped_status_clause}{type_clause} ORDER BY {table}.rowid LIMIT ?3", reason: "Test-only FTS table and status/type clauses selected at runtime." },
    Keep { name: "git_entity_row_selector", path: "khive-pack-git/src/sql/entity_rows.rs", sql: "SELECT {columns} FROM entities WHERE {kind} AND namespace=?1 AND deleted_at {deletion} AND {predicate}{order}", reason: "Seven closed pack-local typed variants select all identifiers, projections, predicates and ordering; namespace and request values are bound. Existing tolerant row decoding remains at the callers." },
    Keep { name: "knowledge_fts_membership", path: "khive-pack-knowledge/src/knowledge/search/lexical.rs", sql: "SELECT 1 AS present FROM knowledge_atoms WHERE rowid IN ({placeholders}) AND namespace = ?1 LIMIT 1", reason: "Variable rowid placeholder list." },
    Keep { name: "knowledge_proto_namespace_present", path: "khive-pack-knowledge/src/knowledge/search/lexical.rs", sql: "SELECT 1 AS present FROM {table} CROSS JOIN knowledge_atoms AS a ON a.rowid = {table}.rowid WHERE {table} MATCH ?1 AND +a.namespace = ?2 LIMIT 1", reason: "Test-only prototype chooses its FTS table at runtime." },
    Keep { name: "knowledge_exact_name_eligibility", path: "khive-pack-knowledge/src/knowledge/search/lexical.rs", sql: "SELECT *, CASE WHEN 1{status_clause}{type_clause} THEN 1 ELSE 0 END AS exact_name_eligible FROM knowledge_atoms WHERE namespace = ?1 AND slug = ?2 AND deleted_at IS NULL LIMIT 1", reason: "Status and atom/domain eligibility clauses selected at runtime." },
    Keep { name: "knowledge_hydrate_atoms", path: "khive-pack-knowledge/src/knowledge/search/hydration.rs", sql: "SELECT id, slug, name, content, tags, finalized, status FROM knowledge_atoms WHERE id IN ({placeholders}) AND +namespace = ?1 AND deleted_at IS NULL", reason: "Variable atom-ID placeholder list." },
    Keep { name: "knowledge_hydrate_domains", path: "khive-pack-knowledge/src/knowledge/search/hydration.rs", sql: "SELECT id, slug, name, description, tags, status FROM knowledge_domains WHERE id IN ({placeholders}) AND +namespace = ?1 AND deleted_at IS NULL", reason: "Variable domain-ID placeholder list." },
    Keep { name: "knowledge_hydrate_phase_b", path: "khive-pack-knowledge/src/knowledge/search/hydration.rs", sql: "SELECT a.*, a.rowid AS rowid FROM knowledge_atoms AS a WHERE a.rowid IN ({rowid_placeholders}) AND +a.namespace = ?1 AND a.deleted_at IS NULL{status_clause}{type_clause}", reason: "Variable rowid placeholders and status/type clauses." },
    Keep { name: "knowledge_domain_member_sizes", path: "khive-pack-knowledge/src/knowledge/search/compose_helpers.rs", sql: "SELECT DISTINCT d.id AS domain_id, a.id AS atom_id, a.name, a.content FROM knowledge_domains AS d LEFT JOIN json_each(d.members) AS member ON 1 = 1 LEFT JOIN knowledge_atoms AS a ON a.namespace = d.namespace AND a.slug = member.value AND a.deleted_at IS NULL WHERE d.namespace = ?1 AND d.id IN ({placeholders}) AND d.deleted_at IS NULL", reason: "Variable domain-ID placeholder list." },
    Keep { name: "knowledge_atom_body_lines", path: "khive-pack-knowledge/src/knowledge/search/compose_helpers.rs", sql: "SELECT atom_id, SUM(CASE WHEN content = '' THEN 0 ELSE length(content) - length(replace(content, char(10), '')) + (CASE WHEN substr(content, -1) = char(10) THEN 0 ELSE 1 END) END) AS body_lines FROM knowledge_sections WHERE namespace = ?1 AND atom_id IN ({placeholders}) AND {SERVABLE_SECTION} GROUP BY atom_id", reason: "Variable atom-ID placeholders and shared servable-section predicate." },
    Keep { name: "knowledge_compose_atom_window", path: "khive-pack-knowledge/src/knowledge/search/handlers.rs", sql: "WITH input(ordinal,raw_ref,is_uuid,prefix) AS (VALUES {}) SELECT input.ordinal AS compose_ordinal, CASE WHEN input.is_uuid=0 AND NOT EXISTS( SELECT 1 FROM knowledge_atoms slug WHERE slug.slug=input.raw_ref AND slug.namespace=?1 AND slug.deleted_at IS NULL) THEN 1 ELSE 0 END AS compose_prefix, atom.* FROM input JOIN knowledge_atoms atom ON atom.rowid IN(SELECT by_id.rowid FROM knowledge_atoms by_id WHERE input.is_uuid=1 AND by_id.id=input.raw_ref AND by_id.namespace=?1 AND by_id.deleted_at IS NULL UNION ALL SELECT by_slug.rowid FROM knowledge_atoms by_slug WHERE input.is_uuid=0 AND by_slug.slug=input.raw_ref AND by_slug.namespace=?1 AND by_slug.deleted_at IS NULL LIMIT 1) OR atom.rowid IN(SELECT prefixed.rowid FROM knowledge_atoms prefixed WHERE input.is_uuid=0 AND input.prefix IS NOT NULL AND NOT EXISTS(SELECT 1 FROM knowledge_atoms slug WHERE slug.slug=input.raw_ref AND slug.namespace=?1 AND slug.deleted_at IS NULL) AND prefixed.namespace=?1 AND prefixed.deleted_at IS NULL AND prefixed.id LIKE input.prefix LIMIT 2) ORDER BY input.ordinal", reason: "Variable VALUES tuple list carrying per-input ordinals and bind positions." },
    Keep { name: "knowledge_test_low_overlap_seed", path: "khive-pack-knowledge/src/knowledge/search.rs", sql: "WITH RECURSIVE x(n) AS ( VALUES(0) UNION ALL SELECT n + 1 FROM x WHERE n < {x_max} ), y(n) AS ( VALUES(0) UNION ALL SELECT n + 1 FROM y WHERE n < {y_max} ) INSERT INTO knowledge_atoms ( id, namespace, slug, name, content, tags, properties, finalized, status, source_uri, source_type, created_at, updated_at, deleted_at ) SELECT printf('80000000-0000-0000-0000-%012d', x.n * {y_stride} + y.n), 'local', printf('lowoverlap-%06d', x.n * {y_stride} + y.n), printf('Low Overlap %06d', x.n * {y_stride} + y.n), 'synthetic corpus content entry ' || (x.n * {y_stride} + y.n) || ' discusses topic term' || ((x.n * {y_stride} + y.n) % {vocab_size}) || ' with padding context sentence for realistic length and additional filler', '[]', NULL, 1, 'reviewed', NULL, NULL, x.n * {y_stride} + y.n, x.n * {y_stride} + y.n, NULL FROM x CROSS JOIN y WHERE x.n * {y_stride} + y.n < {n}", reason: "Test-only corpus dimensions, strides, vocabulary and row ceiling interpolated at runtime." },
    Keep { name: "knowledge_challenge_dispute_count", path: "khive-pack-knowledge/src/knowledge/sections.rs", sql: "UPDATE knowledge_atoms SET properties=json_set(coalesce(properties,'{{}}'),'$.dispute_count',coalesce(json_extract(properties,'$.dispute_count'),0)+{affected}) WHERE id=?1 AND namespace=?2", reason: "Affected section count interpolated from the preceding write." },
    Keep { name: "knowledge_resolve_section_status", path: "khive-pack-knowledge/src/knowledge/sections.rs", sql: "SELECT content_hash FROM knowledge_sections WHERE atom_id=?1 AND section_type=?2 AND {status_filter}", reason: "Caller-selected status predicate." },
    Keep { name: "knowledge_adjudicate_status", path: "khive-pack-knowledge/src/knowledge/sections.rs", sql: "UPDATE knowledge_sections SET status='{new_status}' WHERE atom_id=?1 AND section_type=?2 AND content_hash=?3 AND status='disputed'", reason: "New status selected from the adjudication resolution." },
    Keep { name: "knowledge_adjudicate_dispute_count", path: "khive-pack-knowledge/src/knowledge/sections.rs", sql: "UPDATE knowledge_atoms SET properties=json_set(coalesce(properties,'{{}}'),'$.dispute_count',CASE WHEN coalesce(json_extract(properties,'$.dispute_count'),0) >= {affected} THEN coalesce(json_extract(properties,'$.dispute_count'),0)-{affected} ELSE 0 END) WHERE id=?1 AND namespace=?2", reason: "Affected section count interpolated from the preceding write." },
    Keep { name: "knowledge_section_embed_count", path: "khive-pack-knowledge/src/knowledge/sections_index.rs", sql: "SELECT count(*) AS cnt FROM knowledge_sections s JOIN knowledge_atoms a ON a.id = s.atom_id AND a.namespace = s.namespace AND a.deleted_at IS NULL WHERE s.namespace = ?1{atom_filter}{null_filter}", reason: "Optional atom and null-embedding filters." },
    Keep { name: "knowledge_section_embed_page", path: "khive-pack-knowledge/src/knowledge/sections_index.rs", sql: "SELECT s.id AS id, s.heading AS heading, s.content AS content, a.name AS atom_name FROM knowledge_sections s JOIN knowledge_atoms a ON a.id = s.atom_id AND a.namespace = s.namespace AND a.deleted_at IS NULL WHERE s.namespace = ?1{atom_clause}{null_filter}{keyset_clause} ORDER BY s.id LIMIT ?{limit_pos}", reason: "Optional atom/null/keyset clauses and runtime numbered limit position." },
    Keep { name: "knowledge_embedding_coverage", path: "khive-pack-knowledge/src/knowledge/util.rs", sql: "SELECT COUNT(DISTINCT a.id) FROM knowledge_atoms a WHERE a.namespace = ?1 AND a.deleted_at IS NULL AND NOT khive_tag_contains(a.tags, 'type:domain') AND a.id IN ( SELECT v.subject_id FROM {table_name} v WHERE v.namespace = ?1 AND v.embedding_model = ?2 AND v.field = 'knowledge.atom' )", reason: "Vector table identifier selected from the active model." },
    Keep { name: "ann_registry_minimum", path: "khive-retrieval/src/ann/registry.rs", sql: "SELECT MIN(watermark) FROM ann_consumer_watermark WHERE (namespace = ?{namespace_param} OR namespace = '*') AND embedding_model = ?{model_param}", reason: "Caller-selected numbered bind positions compose the same minimum inside a larger snapshot statement." },
    Keep { name: "ann_raise_authority", path: "khive-retrieval/src/ann/registry.rs", sql: "UPDATE ann_consumer_watermark SET watermark = ?4 WHERE consumer = ?1 AND namespace = ?2 AND embedding_model = ?3 AND {predicate}", reason: "Authority selects the predicate." },
    Keep { name: "ann_pending_backfill", path: "khive-retrieval/src/ann/registry.rs", sql: "INSERT OR IGNORE INTO ann_consumer_pending (consumer, namespace, embedding_model, registered_at_us) SELECT watermark.consumer, watermark.namespace, watermark.embedding_model, ?1 FROM ann_consumer_watermark watermark WHERE watermark.watermark = -2 AND watermark.embedding_model = ?2{scope_filter}", reason: "Compaction scope adds the optional namespace predicate." },
    Keep { name: "ann_pending_expired_select", path: "khive-retrieval/src/ann/registry.rs", sql: "SELECT watermark.consumer, watermark.namespace, watermark.embedding_model, pending.registered_at_us FROM ann_consumer_watermark watermark JOIN ann_consumer_pending pending ON pending.consumer = watermark.consumer AND pending.namespace = watermark.namespace AND pending.embedding_model = watermark.embedding_model WHERE pending.registered_at_us <= ?1 AND watermark.watermark = -2 AND watermark.embedding_model = ?2{scope_filter} ORDER BY watermark.consumer, watermark.namespace", reason: "Compaction scope adds the optional namespace predicate." },
    Keep { name: "ann_pending_expired_delete", path: "khive-retrieval/src/ann/registry.rs", sql: "DELETE FROM ann_consumer_watermark AS watermark WHERE watermark.watermark = -2 AND watermark.embedding_model = ?2{scope_filter} AND EXISTS (SELECT 1 FROM ann_consumer_pending pending WHERE pending.consumer = watermark.consumer AND pending.namespace = watermark.namespace AND pending.embedding_model = watermark.embedding_model AND pending.registered_at_us <= ?1)", reason: "Compaction scope adds the optional namespace predicate." },
    Keep { name: "ann_pending_metadata_prune", path: "khive-retrieval/src/ann/registry.rs", sql: "DELETE FROM ann_consumer_pending AS pending WHERE pending.embedding_model = ?2{pending_scope_filter} AND NOT EXISTS (SELECT 1 FROM ann_consumer_watermark watermark WHERE watermark.consumer = pending.consumer AND watermark.namespace = pending.namespace AND watermark.embedding_model = pending.embedding_model AND watermark.watermark = -2)", reason: "Compaction scope adds its aliased namespace predicate." },
    Keep { name: "ann_corpus_fingerprint", path: "khive-retrieval/src/ann/corpus.rs", sql: "SELECT COUNT(*) AS n FROM {corpus} WHERE {live}", reason: "Trusted vector table, optional live-note join and predicates are selected at runtime." },
    Keep { name: "ann_corpus_scan", path: "khive-retrieval/src/ann/corpus.rs", sql: "SELECT {columns}, {capture} AS log_s FROM {corpus} WHERE {live} ORDER BY {order}", reason: "Table/join, projection, watermark capture, predicate and ordering are assembled." },
    Keep { name: "ann_corpus_final_tail", path: "khive-retrieval/src/ann/corpus.rs", sql: "WITH {registry_cte}{live_cte}selected AS ( SELECT seq, subject_id, op FROM ann_write_log WHERE {predicate} AND seq > {tail_floor} {selected_order} ), finals AS ( SELECT seq, subject_id, op, first_seq FROM ( SELECT seq, subject_id, op, MIN(seq) OVER (PARTITION BY subject_id) AS first_seq, ROW_NUMBER() OVER ( PARTITION BY subject_id ORDER BY seq DESC ) AS final_rank FROM selected ) WHERE final_rank = 1 ) SELECT {columns} FROM {from} LEFT JOIN {table_name} AS vectors ON vectors.subject_id = finals.subject_id {note_join} ORDER BY finals.first_seq", reason: "Scope, explicit registry policy, optional raw cap and result framing are assembled before one coalesced vector join." },
    Keep { name: "ann_corpus_tail_exists", path: "khive-retrieval/src/ann/corpus.rs", sql: "SELECT EXISTS(SELECT 1 FROM ann_write_log WHERE {predicate} AND seq > ?{seq_param}) AS has_tail", reason: "Namespace/model scope determines predicates and numbered bind position." },
    Keep { name: "ann_corpus_scope_counts", path: "khive-retrieval/src/ann/corpus.rs", sql: "SELECT (SELECT COUNT(*) FROM {corpus} WHERE {live}) AS live, (SELECT COUNT(*) FROM ann_write_log WHERE {tail} AND seq > ?{seq_param}) AS tail", reason: "Table/join, live predicate, tail predicate and bind position are assembled." },
    Keep { name: "ann_corpus_classification_counts", path: "khive-retrieval/src/ann/corpus.rs", sql: "WITH tail AS MATERIALIZED ( SELECT COUNT(*) AS tail_rows FROM ann_write_log WHERE {tail} AND seq > ?{seq_param} ), cap AS MATERIALIZED ( SELECT CASE WHEN tail_rows = 1 THEN 1 WHEN tail_rows = 0 OR ?{cap_param} IS NULL OR tail_rows > 9223372036854775807 / ?{cap_param} THEN -1 ELSE tail_rows * ?{cap_param} END AS max_rows FROM tail ), live AS ( SELECT COUNT(*) AS live_rows FROM ( SELECT 1 FROM {corpus} WHERE {live} LIMIT (SELECT max_rows FROM cap) ) ) SELECT live.live_rows AS live, tail.tail_rows AS tail, cap.max_rows AS cap FROM live CROSS JOIN tail CROSS JOIN cap", reason: "Table/join, predicates and sequence/cap bind positions are assembled." },
    Keep { name: "weights_batch_load", path: "khive-retrieval/src/weights/engine_weights.rs", sql: "SELECT atom_id, weight FROM atom_weights WHERE namespace = ?1 AND atom_id IN ({placeholders})", reason: "Variable atom-ID placeholder list, chunked to the bind-parameter limit." },
    Keep { name: "replay_compose_payloads", path: "khive-retrieval/src/replay/engine_replay.rs", sql: "SELECT payload FROM brain_events WHERE kind = 'ComposeEvent' AND json_extract(payload, '$.lambda_id') = ?1", reason: "Compiled only under the undeclared `engine` feature; names the pre-event-log brain_events table, so the SQL lint cannot prepare it until the replay reads are retargeted to brain_event_log." },
];

#[path = "sql_policy_controls.rs"]
mod controls;

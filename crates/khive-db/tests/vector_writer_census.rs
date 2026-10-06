//! ADR-044 A4: raw vec0 writers must be reviewed when their source sites change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// These are production vec0 DML sites on the named base. Keep the store-owned
// sites explicit too: a new statement inside an existing function needs review
// just as much as a new function does.
const EXPECTED: &[(&str, &str, &str, &str, usize)] = &[
    (
        "khive-db/src/stores/vectors.rs",
        "delete_vector_statement",
        "DELETE",
        "{table}",
        1,
    ),
    (
        "khive-db/src/stores/vectors/dml.rs",
        "replace_vector_row_dml",
        "DELETE",
        "{table}",
        3,
    ),
    (
        "khive-db/src/stores/vectors/dml.rs",
        "replace_vector_row_dml",
        "INSERT",
        "{table}",
        1,
    ),
    (
        "khive-db/src/stores/vectors/dml.rs",
        "delete_vector_subjects_dml",
        "DELETE",
        "{table}",
        1,
    ),
    (
        "khive-db/src/stores/vectors/dml.rs",
        "delete_subject_from_vector_tables",
        "DELETE",
        "{table}",
        1,
    ),
    (
        "khive-db/src/stores/vectors/dml.rs",
        "orphan_sweep_dml",
        "DELETE",
        "{table}",
        1,
    ),
    (
        "khive-runtime/src/atomic_message.rs",
        "vector_insert_statements",
        "DELETE",
        "{table}",
        1,
    ),
    (
        "khive-runtime/src/atomic_message.rs",
        "vector_insert_statements",
        "INSERT",
        "{table}",
        1,
    ),
    (
        "khive-runtime/src/atomic_prepare/index_purge.rs",
        "purge_index_row_statement",
        "DELETE",
        "{table}",
        1,
    ),
    (
        "khive-runtime/src/curation.rs",
        "entity_vector_insert_statements",
        "DELETE",
        "{table}",
        1,
    ),
    (
        "khive-runtime/src/curation.rs",
        "entity_vector_insert_statements",
        "INSERT",
        "{table}",
        1,
    ),
    (
        "khive-runtime/src/curation/note_reindex.rs",
        "reindex_note_report_with_plan",
        "DELETE",
        "{table}",
        1,
    ),
    (
        "khive-db/src/namespace_move.rs",
        "move_vectors",
        "DELETE",
        "{quoted}",
        1,
    ),
    (
        "khive-db/src/namespace_move.rs",
        "move_vectors",
        "INSERT",
        "{quoted}",
        1,
    ),
    (
        "khive-runtime/src/note_write.rs",
        "apply",
        "DELETE",
        "main.{table}",
        1,
    ),
];

// Other production dynamic-table DML. Count these too so a new generic SQL
// writer cannot evade the vec0 census merely by using an unremarkable name.
const NON_VEC0: &[(&str, &str, &str, &str, usize)] = &[
    (
        "khive-db/src/backend.rs",
        "ensure_fts_rowid_map_backfilled",
        "DELETE",
        "{map}",
        1,
    ),
    (
        "khive-db/src/backend.rs",
        "ensure_fts_rowid_map_backfilled",
        "DELETE",
        "{table}",
        1,
    ),
    (
        "khive-db/src/backend.rs",
        "ensure_fts_rowid_map_backfilled",
        "INSERT",
        "{map}",
        1,
    ),
    (
        "khive-db/src/backend.rs",
        "ensure_fts_rowid_map_backfilled",
        "INSERT",
        "{state}",
        1,
    ),
    // Database identity bootstrap writes one fixed metadata table, never a
    // vec0 or note table. Keep the dynamic DML site visible to this census.
    (
        "khive-db/src/pool.rs",
        "initialize_database_id",
        "INSERT",
        "main.{DATABASE_ID_TABLE}",
        1,
    ),
    (
        "khive-db/src/namespace_move.rs",
        "move_kinded_subject",
        "UPDATE",
        "{}",
        2,
    ),
    (
        "khive-db/src/namespace_move.rs",
        "move_namespace",
        "UPDATE",
        "{}",
        1,
    ),
    (
        "khive-db/src/namespace_move.rs",
        "move_whole_table",
        "UPDATE",
        "{}",
        1,
    ),
    (
        "khive-db/src/stores/sparse.rs",
        "batch_insert_sparse_dml",
        "INSERT",
        "{table}",
        1,
    ),
    (
        "khive-db/src/stores/sparse.rs",
        "delete_sparse_subject",
        "DELETE",
        "{table}",
        1,
    ),
    (
        "khive-db/src/stores/sparse.rs",
        "upsert_sparse_vector",
        "INSERT",
        "{table}",
        1,
    ),
    (
        "khive-db/src/stores/text.rs",
        "batch_upsert_documents_dml",
        "DELETE",
        "{table}",
        1,
    ),
    (
        "khive-db/src/stores/text.rs",
        "batch_upsert_documents_dml",
        "INSERT",
        "{map}",
        1,
    ),
    (
        "khive-db/src/stores/text.rs",
        "batch_upsert_documents_dml",
        "INSERT",
        "{}",
        1,
    ),
    (
        "khive-db/src/stores/text.rs",
        "delete_document_map_statement",
        "DELETE",
        "{map}",
        1,
    ),
    (
        "khive-db/src/stores/text.rs",
        "delete_document_statement",
        "DELETE",
        "{table}",
        1,
    ),
    (
        "khive-db/src/stores/text.rs",
        "delete_document_statement_scan_fallback",
        "DELETE",
        "{table}",
        1,
    ),
    (
        "khive-db/src/stores/text.rs",
        "insert_document_map_statement",
        "INSERT",
        "{map}",
        1,
    ),
    (
        "khive-db/src/stores/text.rs",
        "insert_document_statement",
        "INSERT",
        "{table}",
        1,
    ),
    ("khive-db/src/stores/text.rs", "rebuild", "INSERT", "{}", 1),
    (
        "khive-db/src/stores/text.rs",
        "rename_namespace_dml",
        "DELETE",
        "{map}",
        1,
    ),
    (
        "khive-db/src/stores/text.rs",
        "rename_namespace_dml",
        "DELETE",
        "{}",
        1,
    ),
    (
        "khive-db/src/stores/text.rs",
        "rename_namespace_dml",
        "INSERT",
        "{map}",
        1,
    ),
    (
        "khive-db/src/stores/text.rs",
        "rename_namespace_dml",
        "INSERT",
        "{}",
        1,
    ),
    (
        "khive-runtime/src/curation.rs",
        "merge_entity_sql",
        "DELETE",
        "{fts_map}",
        1,
    ),
    (
        "khive-runtime/src/curation.rs",
        "merge_entity_sql",
        "DELETE",
        "{fts_table}",
        2,
    ),
    (
        "khive-runtime/src/curation.rs",
        "merge_entity_sql",
        "INSERT",
        "{fts_map}",
        1,
    ),
    (
        "khive-runtime/src/curation.rs",
        "merge_entity_sql",
        "INSERT",
        "{}",
        1,
    ),
    (
        "khive-runtime/src/curation.rs",
        "merge_note_sql",
        "DELETE",
        "{fts_map}",
        1,
    ),
    (
        "khive-runtime/src/curation.rs",
        "merge_note_sql",
        "DELETE",
        "{fts_table}",
        2,
    ),
    (
        "khive-runtime/src/curation.rs",
        "merge_note_sql",
        "INSERT",
        "{fts_map}",
        1,
    ),
    (
        "khive-runtime/src/curation.rs",
        "merge_note_sql",
        "INSERT",
        "{}",
        1,
    ),
];

const TEST_SUPPORT: &[(&str, &str, &str, &str, usize)] = &[
    (
        "khive-db/src/namespace_move_fixture.rs",
        "index_row",
        "INSERT",
        "{table}",
        1,
    ),
    (
        "khive-db/src/namespace_move_fixture.rs",
        "index_row",
        "INSERT",
        "{table}_rowids",
        1,
    ),
    (
        "khive-db/src/namespace_move_fixture.rs",
        "add_vector_row",
        "INSERT",
        "vec_{model_key}",
        1,
    ),
];

const DELETE_VECTOR_CALLERS: &[(&str, &str, usize)] = &[(
    "khive-db/src/stores/vectors.rs",
    "VectorStore for SqliteVecStore::delete",
    1,
)];

fn source_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read source directory") {
        let entry = entry.expect("read source entry");
        let path = entry.path();
        if path.is_dir() {
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == "tests" || name == "test_support")
            {
                continue;
            }
            source_files(&path, files);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            let name = path.file_name().unwrap().to_string_lossy();
            if name.ends_with("_tests.rs")
                || name.ends_with("_test.rs")
                || name == "tests.rs"
                || name == "test_support.rs"
                // This module is explicitly cfg(any(test, feature = "test-support"))
                // in khive-db/src/lib.rs. A future unconditional *_fixture.rs
                // must be scanned, regardless of its filename.
                || path.ends_with("khive-db/src/namespace_move_fixture.rs")
            {
                continue;
            }
            files.push(path);
        }
    }
}

fn all_rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read khive-db source directory") {
        let path = entry.expect("read khive-db source entry").path();
        if path.is_dir() {
            all_rust_files(&path, files);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            files.push(path);
        }
    }
}

fn test_only_cfg(line: &str) -> bool {
    let cfg = line.trim().replace(' ', "");
    let has_test_predicate = cfg
        .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .any(|token| token == "test");
    cfg == "#[cfg(test)]"
        || cfg == "#[cfg(feature=\"test-support\")]"
        || cfg == "#[cfg(any(test,feature=\"test-support\"))]"
        || (cfg.starts_with("#[cfg(all(")
            && cfg.ends_with("))]")
            && has_test_predicate
            && !cfg.contains("not(test)")
            && !cfg.contains("any("))
}

fn production_lines(source: &str) -> Vec<&str> {
    let mut lines: Vec<_> = source.lines().collect();
    let mut index = 0;
    while index < lines.len() {
        if !test_only_cfg(lines[index]) {
            index += 1;
            continue;
        }
        let start = index;
        let mut item = index + 1;
        while item < lines.len()
            && (lines[item].trim().is_empty() || lines[item].trim().starts_with("#["))
        {
            item += 1;
        }
        if item == lines.len() {
            break;
        }
        let indent = lines[item].len() - lines[item].trim_start().len();
        // A `use` item ends at its `;`, including a grouped `use a::{b, c};`, so its braces are
        // not a body to strip through.
        let head = lines[item].trim_start();
        let head = match head.strip_prefix("pub") {
            Some(rest) => rest.trim_start_matches(|c: char| c != ' ').trim_start(),
            None => head,
        };
        let use_item = head.starts_with("use ");
        let mut end = item;
        let mut has_body = false;
        while end < lines.len() {
            if !use_item && lines[end].contains('{') {
                has_body = true;
                break;
            }
            if lines[end].contains(';') {
                break;
            }
            end += 1;
        }
        if has_body {
            end += 1;
            while end < lines.len() {
                let current_indent = lines[end].len() - lines[end].trim_start().len();
                if lines[end].trim() == "}" && current_indent == indent {
                    break;
                }
                end += 1;
            }
        }
        let next = (end + 1).min(lines.len());
        lines[start..next].fill("");
        index = next;
    }
    lines
}

fn function_name(line: &str) -> Option<&str> {
    let line = line.trim_start();
    if line.starts_with("//") {
        return None;
    }
    let start = line.find("fn ")? + 3;
    let name = line[start..]
        .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .next()?;
    (!name.is_empty()).then_some(name)
}

fn dml_target(line: &str) -> Option<(&'static str, &str)> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") || trimmed.starts_with('*') {
        return None;
    }
    let upper = line.to_ascii_uppercase();
    for (needle, operation) in [
        ("INSERT OR REPLACE INTO ", "INSERT"),
        ("INSERT OR IGNORE INTO ", "INSERT"),
        ("INSERT INTO ", "INSERT"),
        ("REPLACE INTO ", "INSERT"),
        ("DELETE FROM ", "DELETE"),
        ("UPDATE ", "UPDATE"),
    ] {
        if let Some(start) = upper.find(needle) {
            if start > 0
                && (upper.as_bytes()[start - 1].is_ascii_alphanumeric()
                    || upper.as_bytes()[start - 1] == b'_')
            {
                continue;
            }
            let after = line[start + needle.len()..].trim_start();
            let target = after
                .split(|character: char| {
                    character.is_whitespace() || character == '(' || character == ';'
                })
                .next()
                .unwrap_or("")
                .trim_matches(|character| matches!(character, '"' | '\'' | '`' | '[' | ']'));
            if !target.is_empty() {
                return Some((operation, target));
            }
        }
    }
    None
}

fn sites(relative: &str, source: &str) -> Vec<(String, usize)> {
    let lines = production_lines(source);
    let mut functions: Vec<(usize, &str)> = vec![(0, "<module>")];
    for (index, line) in lines.iter().enumerate() {
        if let Some(name) = function_name(line) {
            functions.push((index, name));
        }
    }

    let mut found = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let Some((operation, target)) = dml_target(line) else {
            continue;
        };
        let unqualified = target.strip_prefix("main.").unwrap_or(target);
        let vector_table = unqualified.starts_with("vec_");
        let dynamic_table = unqualified.starts_with('{');
        if !vector_table && !dynamic_table {
            continue;
        }
        let owner = functions.partition_point(|(start, _)| *start <= index) - 1;
        let name = functions[owner].1;
        found.push((
            format!("{relative}::{name}::{operation}::{target}"),
            index + 1,
        ));
    }
    found
}

fn delete_vector_call_sites(relative: &str, source: &str) -> Vec<(String, usize)> {
    let lines: Vec<_> = source.lines().collect();
    let mut functions: Vec<(usize, &str)> = vec![(0, "<module>")];
    let mut impls: Vec<(usize, &str)> = vec![(0, "<module>")];
    for (index, line) in lines.iter().enumerate() {
        if let Some(name) = function_name(line) {
            functions.push((index, name));
        }
        if let Some(name) = line.trim_start().strip_prefix("impl ") {
            impls.push((index, name.split('{').next().unwrap().trim()));
        }
    }

    let mut found = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with("//")
            || function_name(line) == Some("delete_vector_statement")
        {
            continue;
        }
        let count = line.matches("delete_vector_statement(").count();
        if count == 0 {
            continue;
        }
        let function = functions[functions.partition_point(|(start, _)| *start <= index) - 1].1;
        let owner = impls[impls.partition_point(|(start, _)| *start <= index) - 1].1;
        for _ in 0..count {
            found.push((format!("{relative}::{owner}::{function}"), index + 1));
        }
    }
    found
}

#[test]
fn raw_vec0_writer_census_includes_note_vectors_apply() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace crates directory");
    let mut files = Vec::new();
    for entry in std::fs::read_dir(crates).expect("read workspace crates") {
        let entry = entry.expect("read crate");
        let src = entry.path().join("src");
        if src.is_dir() {
            source_files(&src, &mut files);
        }
    }
    files.sort();

    let mut actual = BTreeMap::<String, Vec<String>>::new();
    for path in files {
        let relative = path
            .strip_prefix(crates)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let source = std::fs::read_to_string(&path).expect("read source file");
        for (site, line) in sites(&relative, &source) {
            actual
                .entry(site)
                .or_default()
                .push(format!("{relative}:{line}"));
        }
    }

    let expected: BTreeMap<String, usize> = EXPECTED
        .iter()
        .chain(NON_VEC0)
        .map(|(path, function, operation, table, count)| {
            (format!("{path}::{function}::{operation}::{table}"), *count)
        })
        .collect();
    let counts: BTreeMap<String, usize> = actual
        .iter()
        .map(|(site, locations)| (site.clone(), locations.len()))
        .collect();
    assert_eq!(
        counts, expected,
        "raw vec0 SQL writer inventory changed: {actual:#?}"
    );

    // The SQL inventory excludes test-support by design; builder calls do not.
    let mut caller_files = Vec::new();
    all_rust_files(&crates.join("khive-db/src"), &mut caller_files);
    caller_files.sort();
    let mut callers = BTreeMap::<String, Vec<String>>::new();
    for path in caller_files {
        let relative = path
            .strip_prefix(crates)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let source = std::fs::read_to_string(&path).expect("read khive-db source file");
        for (site, line) in delete_vector_call_sites(&relative, &source) {
            callers
                .entry(site)
                .or_default()
                .push(format!("{relative}:{line}"));
        }
    }
    let expected_callers: BTreeMap<String, usize> = DELETE_VECTOR_CALLERS
        .iter()
        .map(|(path, owner, count)| (format!("{path}::{owner}"), *count))
        .collect();
    let caller_counts: BTreeMap<String, usize> = callers
        .iter()
        .map(|(site, locations)| (site.clone(), locations.len()))
        .collect();
    assert_eq!(
        caller_counts, expected_callers,
        "delete_vector_statement callers changed: {callers:#?}"
    );
}

#[test]
fn raw_vec0_writer_census_pins_test_support_fixture() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace crates directory");
    let lib = std::fs::read_to_string(crates.join("khive-db/src/lib.rs")).unwrap();
    assert!(lib.contains(
        "#[cfg(any(test, feature = \"test-support\"))]\npub mod namespace_move_fixture;"
    ));
    let relative = "khive-db/src/namespace_move_fixture.rs";
    let source =
        std::fs::read_to_string(crates.join("khive-db/src/namespace_move_fixture.rs")).unwrap();
    let expected: BTreeMap<String, usize> = TEST_SUPPORT
        .iter()
        .map(|(path, function, operation, table, count)| {
            (format!("{path}::{function}::{operation}::{table}"), *count)
        })
        .collect();
    let count_sites = |source: &str| {
        let mut counts = BTreeMap::new();
        for (site, _) in sites(relative, source) {
            *counts.entry(site).or_insert(0) += 1;
        }
        counts
    };
    assert_eq!(count_sites(&source), expected);

    for target in [
        "INSERT INTO {table} \\",
        "INSERT OR REPLACE INTO {table}_rowids",
        "INSERT INTO vec_{model_key} \\",
    ] {
        let mutant = source.replacen(target, "SELECT FROM fixture_row", 1);
        assert_ne!(mutant, source, "fixture template missing: {target}");
        assert_ne!(
            count_sites(&mutant),
            expected,
            "missing fixture pin: {target}"
        );
    }
}

#[test]
fn raw_vec0_writer_census_rejects_new_builder_caller() {
    let source = include_str!("../src/stores/vectors.rs");
    let relative = "khive-db/src/stores/vectors.rs";
    assert!(source.contains("pub(crate) fn delete_vector_statement("));
    let mutant = format!(
        "{source}\n#[cfg(test)]\nmod unclassified_caller {{\n \
         use super::*;\n \
         fn unclassified_delete(id: Uuid) {{ \
         let _ = delete_vector_statement(\"vec_new\", id, \"local\"); \
         }}\n}}\n"
    );
    assert_eq!(sites(relative, source), sites(relative, &mutant));
    assert_eq!(delete_vector_call_sites(relative, source).len(), 1);
    let added = delete_vector_call_sites(relative, &mutant);
    assert_eq!(added.len(), 2);
    assert!(added
        .iter()
        .any(|(site, _)| site.ends_with("::unclassified_delete")));
}

#[test]
fn adr044_a4_census_detects_unlisted_dynamic_writer() {
    let unknown = r#"fn unlisted_vec0_writer(conn: &Connection, model_key: &str) {
        let table = format!("vec_{model_key}");
        conn.execute(&format!("DELETE FROM {table} WHERE subject_id = ?1"), ["id"]);
    }"#;
    assert_eq!(sites("new-crate/src/writer.rs", unknown).len(), 1);
    assert!(sites(
        "new-crate/src/writer.rs",
        "fn sidecar() { sql!(\"DELETE FROM vector_provenance\"); }"
    )
    .is_empty());

    let fixture = "#[cfg(test)]\nmod tests {\n    fn fixture() { sql!(\"INSERT INTO vec_fixture VALUES (1)\"); }\n}\n";
    assert!(sites("khive-db/src/namespace_move.rs", fixture).is_empty());

    let interleaved = "fn before() {}\n#[cfg(test)]\nmod tests {\n    fn fixture() { sql!(\"DELETE FROM vec_fixture\"); }\n}\nfn after() { sql!(\"DELETE FROM vec_new WHERE subject_id=?1\"); }\n";
    assert_eq!(sites("new-crate/src/writer.rs", interleaved).len(), 1);
}

#[test]
fn raw_vec0_writer_census_ignores_identifier_suffixes() {
    // `properties_update {` in the route classifier is an identifier, not SQL.
    assert!(dml_target("if plain_insert || conflict_insert || properties_update {").is_none());
    assert_eq!(
        dml_target("conn.execute(&format!(\"UPDATE {table} SET value=?1\"), [])"),
        Some(("UPDATE", "{table}")),
    );
}

#[test]
fn raw_vec0_writer_census_rejects_unlisted_dynamic_pair() {
    let source = r#"fn unlisted_vec0_writer(conn: &Connection, model_key: &str) {
        let table = format!("vec_{model_key}");
        conn.execute(&format!("DELETE FROM {table} WHERE subject_id = ?1"), ["id"]);
        conn.execute(&format!("INSERT INTO {table} (subject_id) VALUES (?1)"), ["id"]);
    }"#;
    let actual = sites("new-crate/src/writer.rs", source);
    assert_eq!(actual.len(), 2, "DELETE and INSERT must each be found");
    assert!(actual
        .iter()
        .any(|(site, _)| site.ends_with("::DELETE::{table}")));
    assert!(actual
        .iter()
        .any(|(site, _)| site.ends_with("::INSERT::{table}")));

    let delete_removed = source.replacen("DELETE FROM {table}", "SELECT FROM {table}", 1);
    let insert_removed = source.replacen("INSERT INTO {table}", "SELECT INTO {table}", 1);
    assert_eq!(sites("new-crate/src/writer.rs", &delete_removed).len(), 1);
    assert_eq!(sites("new-crate/src/writer.rs", &insert_removed).len(), 1);
}

#[test]
fn raw_vec0_writer_census_pins_non_vector_dynamic_dml() {
    let source = include_str!("../src/stores/text.rs");
    let relative = "khive-db/src/stores/text.rs";
    let target = "DELETE FROM {table} WHERE namespace = ?1 AND subject_id = ?2";
    assert_eq!(source.matches(target).count(), 1);
    let baseline = sites(relative, source);
    assert!(baseline.iter().any(|(site, _)| {
        site == "khive-db/src/stores/text.rs::delete_document_statement_scan_fallback::DELETE::{table}"
    }));
    let mutant = source.replacen(
        target,
        "SELECT FROM {table} WHERE namespace = ?1 AND subject_id = ?2",
        1,
    );
    assert_eq!(sites(relative, &mutant).len() + 1, baseline.len());
}

#[test]
fn raw_vec0_writer_census_keeps_production_cfg_and_fixture_names() {
    let source = "#[cfg(not(test))]\nfn production_writer() { sql!(\"DELETE FROM vec_live\"); }\n\
                  #[cfg(any(test, feature = \"vectors\"))]\nfn feature_writer() { sql!(\"INSERT INTO vec_feature VALUES (1)\"); }\n\
                  #[cfg(all(feature = \"contest\", feature = \"vectors\"))]\nfn contest_writer() { sql!(\"DELETE FROM vec_contest\"); }\n\
                  #[cfg(test)]\nfn test_writer() { sql!(\"DELETE FROM vec_test\"); }\n";
    let found = sites("new-crate/src/writer.rs", source);
    assert_eq!(found.len(), 3, "only cfg(test) may be skipped");
    assert!(found
        .iter()
        .any(|(site, _)| site.contains("production_writer")));
    assert!(found
        .iter()
        .any(|(site, _)| site.contains("feature_writer")));
    assert!(found
        .iter()
        .any(|(site, _)| site.contains("contest_writer")));

    let dir = tempfile::tempdir().expect("fixture census tempdir");
    let fixture = dir.path().join("unconditional_fixture.rs");
    std::fs::write(
        &fixture,
        "fn writer() { sql!(\"INSERT INTO vec_new VALUES (1)\"); }",
    )
    .expect("write unconditional fixture");
    let mut files = Vec::new();
    source_files(dir.path(), &mut files);
    assert_eq!(files, vec![fixture.clone()]);
    let source = std::fs::read_to_string(fixture).unwrap();
    assert_eq!(
        sites("new-crate/src/unconditional_fixture.rs", &source).len(),
        1
    );
}

#[test]
fn raw_vec0_writer_census_ends_test_only_use_at_its_semicolon() {
    let writer = r#"sql!("DELETE FROM vec_live WHERE id = ?1");"#;
    let fixture = r#"sql!("INSERT INTO vec_fixture VALUES (1)");"#;
    for import in [
        "use super::{Uuid, Value};",
        "use super::{\n    Uuid,\n    Value,\n};",
        "pub(crate) use super::{Uuid, Value};",
    ] {
        let source = format!(
            "#[cfg(test)]\n{import}\nimpl Handler {{\n    fn writer() {{ {writer} }}\n}}\n\
             #[cfg(test)]\nmod tests {{\n    fn fixture() {{ {fixture} }}\n}}\n"
        );
        let found: Vec<_> = sites("new-crate/src/writer.rs", &source)
            .into_iter()
            .map(|(site, _)| site)
            .collect();
        assert_eq!(
            found,
            vec!["new-crate/src/writer.rs::writer::DELETE::vec_live".to_string()],
            "{import}"
        );
    }
}

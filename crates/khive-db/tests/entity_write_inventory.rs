//! Raw replacement destroys the persisted entity revision; typed upserts must
//! retain the old row and use a conflict UPDATE instead.

use std::path::Path;

fn contains_entity_replace(source: &str) -> bool {
    let flattened = source.replace("\\\n", " ").to_ascii_lowercase();
    let words: Vec<_> = flattened.split_whitespace().collect();
    words.windows(5).any(|words| {
        words[0].trim_start_matches('"') == "insert"
            && words[1] == "or"
            && words[2] == "replace"
            && words[3] == "into"
            && words[4]
                .split('(')
                .next()
                .unwrap()
                .trim_matches(['\\', '"', ';', '`', '[', ']'])
                == "entities"
    })
}

fn scan(root: &Path, path: &Path, offenders: &mut Vec<String>) {
    for entry in std::fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            scan(root, &path, offenders);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("rs" | "sql")
        ) {
            let relative = path.strip_prefix(root).unwrap();
            // Historical migration fixtures may demonstrate legacy replacement.
            let migration = relative == Path::new("khive-db/src/migrations_tests.rs")
                || relative == Path::new("khive-db/src/migrations.rs")
                || (relative.starts_with("khive-db/sql")
                    && path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(|c: char| c.is_ascii_digit()));
            if !migration && contains_entity_replace(&std::fs::read_to_string(&path).unwrap()) {
                offenders.push(relative.display().to_string());
            }
        }
    }
}

#[test]
fn issue2673_no_raw_entity_replace_outside_migrations() {
    assert!(
        contains_entity_replace(concat!(
            "INSERT OR REPLACE",
            " INTO entities (id) VALUES (?1)"
        )),
        "scanner must recognize the forbidden SQL before scanning source"
    );
    assert!(contains_entity_replace(concat!(
        "insert or replace",
        " into entities(id) values (?1)"
    )));
    assert!(!contains_entity_replace("INSERT INTO entities(id) VALUES (?1) ON CONFLICT(id) DO UPDATE SET version=entities.version+1"));
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut offenders = Vec::new();
    scan(root, root, &mut offenders);
    assert!(
        offenders.is_empty(),
        "raw entity replacement resets versions: {offenders:?}"
    );
}

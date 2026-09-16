//! `RuntimeError::NotFound` renders as `not found: {0}`. A payload that begins
//! with `not found: ` itself therefore reaches the caller as
//! "not found: not found: <id>" (#2864). Ten construction sites did exactly that
//! across two packs, and every test beside them matched the variant, which both
//! forms satisfy.
//!
//! This census re-derives the construction population from live source at test
//! time: it walks every `.rs` file under the workspace root and asserts no
//! `NotFound(` argument begins with the literal `"not found: `, whether passed
//! directly or through `format!(`, across any whitespace or line breaks between
//! those tokens. A new site fails here instead of reaching a caller.
//!
//! Its limits, stated so they can be argued with: it reads text, not an AST, so
//! a payload built in a variable and passed by name is invisible to it; and it
//! skips whole-line `//` comments but not a trailing comment on a code line.

use std::path::{Path, PathBuf};

/// This file quotes the pattern in its own controls.
const EXEMPT_FILES: &[&str] = &["khive-runtime/tests/not_found_payload_census.rs"];

/// The nearest ancestor of `CARGO_MANIFEST_DIR` whose `Cargo.toml` declares
/// `[workspace]`, which in this repository is `crates/`. `None` outside the
/// workspace checkout rather than a guessed parent.
fn find_workspace_root() -> Option<PathBuf> {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        let candidate = dir.join("Cargo.toml");
        if candidate.is_file() {
            if let Ok(contents) = std::fs::read_to_string(&candidate) {
                if contents.contains("[workspace]") {
                    return Some(dir);
                }
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some("target") {
                continue;
            }
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Blank every whole-line `//` comment, keeping its newline so the line numbers
/// of the code around it do not move.
fn without_line_comments(text: &str) -> String {
    text.lines()
        .map(|line| {
            if line.trim_start().starts_with("//") {
                ""
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 1-based line numbers of every `NotFound(` whose first argument begins with the
/// literal `"not found: `, directly or through `format!(`.
fn doubled_prefix_lines(text: &str) -> Vec<usize> {
    let text = without_line_comments(text);
    let needle = "NotFound(";
    let mut lines = Vec::new();
    let mut from = 0;
    while let Some(offset) = text[from..].find(needle) {
        let start = from + offset;
        let mut rest = text[start + needle.len()..].trim_start();
        if let Some(after) = rest.strip_prefix("format!(") {
            rest = after.trim_start();
        }
        if rest.starts_with("\"not found: ") {
            lines.push(text[..start].matches('\n').count() + 1);
        }
        from = start + needle.len();
    }
    lines
}

#[test]
fn detector_finds_the_doubled_prefix_in_every_shape_and_nothing_else() {
    // Must match: the shapes the ten sites had, plus a split a line grep misses.
    assert_eq!(
        doubled_prefix_lines(r#"Err(RuntimeError::NotFound(format!("not found: {id_str}")))"#),
        vec![1]
    );
    assert_eq!(
        doubled_prefix_lines(r#"let e = RuntimeError::NotFound("not found: x".into());"#),
        vec![1]
    );
    assert_eq!(
        doubled_prefix_lines(
            "fn f() {\n    return Err(RuntimeError::NotFound(\n        format!(\n            \"not found: {}\",\n            p.id\n        ),\n    ));\n}"
        ),
        vec![2],
        "a construction split across lines is the one a line-scoped grep cannot see"
    );
    // Must not match: the bare-payload convention, the error type's own
    // attribute, and a whole-line comment naming the pattern.
    assert!(doubled_prefix_lines(r#"NotFound(format!("entity {id}"))"#).is_empty());
    assert!(doubled_prefix_lines("NotFound(id.to_string())").is_empty());
    assert!(doubled_prefix_lines(r#"#[error("not found: {0}")]"#).is_empty());
    assert!(doubled_prefix_lines(r#"    // was: NotFound(format!("not found: {id}"))"#).is_empty());
}

#[test]
fn no_not_found_payload_repeats_the_prefix_its_error_type_renders() {
    let Some(root) = find_workspace_root() else {
        println!(
            "skipping not-found payload census: no ancestor Cargo.toml declaring [workspace] \
             above {}",
            env!("CARGO_MANIFEST_DIR")
        );
        return;
    };
    let mut files = Vec::new();
    collect_rs_files(&root, &mut files);
    assert!(
        files.len() > 100,
        "the walker found suspiciously few .rs files under {root:?} ({}); it likely resolved \
         the wrong root rather than an empty workspace",
        files.len()
    );
    // The population must contain the error type this census is about, or the
    // walk is reading some other tree and an empty result would mean nothing.
    assert!(
        files
            .iter()
            .any(|f| f.ends_with("khive-runtime/src/error.rs")),
        "the walk did not reach khive-runtime/src/error.rs under {root:?}"
    );

    let mut offending = Vec::new();
    for file in &files {
        let rel = file
            .strip_prefix(&root)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");
        if EXEMPT_FILES.contains(&rel.as_ref()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for line in doubled_prefix_lines(&text) {
            offending.push(format!("{rel}:{line}"));
        }
    }
    assert!(
        offending.is_empty(),
        "RuntimeError::NotFound renders as \"not found: {{payload}}\", so these payloads reach \
         callers as \"not found: not found: ...\". Pass the bare id instead: {offending:?}"
    );
}

//! ADR-115 Amendment 2: named redaction surfaces close the `mask_secrets`
//! call-site population outside `secret_gate.rs` itself. Every caller that
//! needs the canonical detector reaches it through
//! `secret_gate::mask_for_redaction_surface`, never the raw `mask_secrets`
//! primitive directly — a truncation-before-masking bug at a raw call site
//! is exactly what let a credential's terminating span survive past a fixed
//! input window on two prior call sites.
//!
//! This test re-derives the population from live source at test time: it
//! walks every `.rs` file under `crates/` outside `secret_gate.rs` (which
//! owns the primitive and its sole in-module wrapper) and asserts none of
//! them calls `mask_secrets(` directly. A future direct caller fails this
//! test instead of silently joining the population.

use std::path::{Path, PathBuf};

/// Files exempt from the census: the primitive's owning file (which also
/// defines the `mask_for_redaction_surface` wrapper every other caller must
/// go through), and this census test itself, which necessarily quotes the
/// literal string `mask_secrets(` in its own controls and panic message.
const EXEMPT_FILES: &[&str] = &[
    "crates/khive-runtime/src/secret_gate.rs",
    "crates/khive-runtime/tests/adr115_redaction_call_site_census.rs",
];

fn workspace_root() -> PathBuf {
    PathBuf::from(format!("{}/../..", env!("CARGO_MANIFEST_DIR")))
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Defensive: a workspace member never owns a `target` directory
            // of its own, but skip it if one is ever present so a stray
            // build artifact directory can't inflate or corrupt the scan.
            if path.file_name().and_then(|n| n.to_str()) == Some("target") {
                continue;
            }
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Whether a source line calls `mask_secrets(`, qualified or not. Deliberately
/// a plain substring match, not a parser: the census cares about the literal
/// call-site population, and a text match is what a future direct caller
/// would actually add.
fn line_calls_mask_secrets(line: &str) -> bool {
    line.contains("mask_secrets(")
}

#[test]
fn detector_recognizes_a_direct_call_and_ignores_the_wrapper() {
    // Positive control: this is (a paraphrase of) exactly the line the two
    // kkernel coordinator sites carried before this fix.
    assert!(line_calls_mask_secrets(
        "    let masked = khive_runtime::secret_gate::mask_secrets(&bounded_input);"
    ));
    assert!(line_calls_mask_secrets(
        "    let masked = mask_secrets(text);"
    ));
    // Negative control: routing through the surface wrapper must not trip
    // the detector, or every legitimate caller would fail this census too.
    assert!(!line_calls_mask_secrets(
        "    let masked = mask_for_redaction_surface(RedactionSurface::McpDiagnostic, text);"
    ));
}

#[test]
fn mask_secrets_has_no_direct_caller_outside_its_owning_file() {
    let root = workspace_root();
    let crates_dir = root.join("crates");
    let mut files = Vec::new();
    collect_rs_files(&crates_dir, &mut files);
    assert!(
        files.len() > 100,
        "the walker found suspiciously few .rs files under {crates_dir:?} ({}); \
         it likely resolved the wrong root rather than an empty workspace",
        files.len()
    );

    let mut offending_sites: Vec<String> = Vec::new();
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
        for (idx, line) in text.lines().enumerate() {
            if line_calls_mask_secrets(line) {
                offending_sites.push(format!("{rel}:{}", idx + 1));
            }
        }
    }

    assert!(
        offending_sites.is_empty(),
        "mask_secrets( must only be called from crates/khive-runtime/src/secret_gate.rs; \
         every other caller must route through secret_gate::mask_for_redaction_surface so \
         its mask-then-truncate contract applies instead of re-implementing (and potentially \
         getting wrong) the same bounding logic. Found direct callers outside the allow-list: \
         {offending_sites:?}"
    );
}

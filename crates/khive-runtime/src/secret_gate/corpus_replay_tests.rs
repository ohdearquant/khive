use super::*;
use rusqlite::{Connection, OpenFlags};

// ── Whole-token-average entropy dilution (issue #1044, false-negative) ──

#[test]
fn blocks_hex_credential_diluted_by_filler_path_segments() {
    // A real 40-char hex credential as one path segment among low-entropy
    // filler segments. Whole-token-average entropy is diluted below
    // ENTROPY_THRESHOLD, and the whole token is not pure hex (it has `/`
    // and `.` in it), so neither the whole-token entropy check nor the
    // whole-token hex-credential-token check catches it — only a per-run
    // check does.
    let line = "api key vault/9f8e7d6c5b4a39281706f5e4d3c2b1a09f8e7d6c/rotate.md";
    let m = scan(line);
    assert!(
        m.is_some(),
        "hex credential diluted by filler path segments must be blocked; got None"
    );
    assert_eq!(m.unwrap().detector, "hex-credential-token");
}

#[test]
fn blocks_chopped_secret_padded_by_filler_runs() {
    // A random high-entropy run planted among short filler runs. Whole-
    // token-average entropy is diluted below threshold by the filler.
    let line = "secret path a/b/c/d/Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvM/e/f.rs"; // gitleaks:allow
    let m = scan(line);
    assert!(
        m.is_some(),
        "chopped/padded secret among filler runs must be blocked; got None"
    );
    assert_eq!(m.unwrap().detector, "high-entropy-token");
}

#[test]
fn allows_real_paths_with_no_run_reaching_min_entropy_len() {
    // Regression guard: the #1040 measurement corpus's real path false
    // positives never contain a single run >= MIN_ENTROPY_LEN, so the new
    // per-run check introduced for #1044 must not newly block them.
    let contents = [
        "branch feat-session-mirror pushed, see release_notes_v2.md for the key findings",
        "password reset doc: docs/adr/ADR-055-epistemic-edge-relations.md",
        "credential handling code crates/khive-pack-session/src/mirror/ingest.rs",
        "api key handling lives in check_entropy_heuristic_impl",
    ];
    for content in contents {
        assert!(
            check(content).is_ok(),
            "real path with no long run must still pass; fired: {:?}",
            scan(content)
        );
    }
}

#[test]
#[ignore]
fn replay_against_corpus() {
    let db_path = std::env::var("KHIVE_REPLAY_DB")
        .expect("set KHIVE_REPLAY_DB=/path/to/khive.db to run the corpus replay (read-only)");
    let conn = Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .expect("open corpus DB read-only");

    let mut total = 0usize;
    let mut blocked = 0usize;
    let mut samples: Vec<String> = Vec::new();

    let mut collect = |sql: &str| {
        let mut stmt = conn.prepare(sql).expect("prepare replay query");
        let mut rows = stmt.query([]).expect("query replay rows");
        while let Some(row) = rows.next().expect("read replay row") {
            let content: Option<String> = row.get(0).unwrap_or(None);
            let Some(content) = content else { continue };
            if content.is_empty() {
                continue;
            }
            total += 1;
            if let Some(m) = scan(&content) {
                blocked += 1;
                if samples.len() < 30 {
                    samples.push(format!(
                        "{} :: {}",
                        m,
                        content.chars().take(160).collect::<String>()
                    ));
                }
            }
        }
    };

    collect("SELECT content FROM notes WHERE deleted_at IS NULL");
    collect("SELECT description FROM entities WHERE deleted_at IS NULL");

    eprintln!("corpus replay: {blocked}/{total} strings blocked");
    for s in &samples {
        eprintln!("  BLOCKED: {s}");
    }
}

/// Generates the sanitized corpus manifest at
/// `tests/data/secret_gate_corpus_manifest.md`: per-detector block counts plus a
/// sha256 of each blocked candidate, never the candidate text itself. Point-in-time
/// generator, not a CI check — re-run manually (`KHIVE_REPLAY_DB=... cargo test -p
/// khive-runtime --release -- --ignored --nocapture generate_corpus_manifest`) and
/// hand-update the checked-in file when the detector set changes.
#[test]
#[ignore]
fn generate_corpus_manifest() {
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;

    let db_path = std::env::var("KHIVE_REPLAY_DB")
        .expect("set KHIVE_REPLAY_DB=/path/to/khive.db to run the corpus replay (read-only)");
    let conn = Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .expect("open corpus DB read-only");

    let mut total = 0usize;
    let mut counts_by_detector: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut hashes_by_detector: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    let mut max_run_reaching_min_entropy_len = 0usize;

    let mut collect = |sql: &str| {
        let mut stmt = conn.prepare(sql).expect("prepare replay query");
        let mut rows = stmt.query([]).expect("query replay rows");
        while let Some(row) = rows.next().expect("read replay row") {
            let content: Option<String> = row.get(0).unwrap_or(None);
            let Some(content) = content else { continue };
            if content.is_empty() {
                continue;
            }
            total += 1;
            // Track the #1040 soundness claim directly against the same
            // corpus this replay scans: the longest run any path-shaped
            // token contributes, so the "no real path false positive
            // reaches MIN_ENTROPY_LEN" claim is checked against actual
            // data rather than asserted.
            for token in content.split_whitespace() {
                for run in token.split(|c: char| !c.is_ascii_alphanumeric()) {
                    max_run_reaching_min_entropy_len =
                        max_run_reaching_min_entropy_len.max(run.len());
                }
            }
            if let Some(m) = scan(&content) {
                *counts_by_detector.entry(m.detector).or_insert(0) += 1;
                let hash = format!("{:x}", Sha256::digest(content.as_bytes()));
                hashes_by_detector.entry(m.detector).or_default().push(hash);
            }
        }
    };

    collect("SELECT content FROM notes WHERE deleted_at IS NULL");
    collect("SELECT description FROM entities WHERE deleted_at IS NULL");

    let blocked: usize = counts_by_detector.values().sum();
    println!("total_scanned: {total}");
    println!("total_blocked: {blocked}");
    println!("longest_alphanumeric_run_in_corpus: {max_run_reaching_min_entropy_len}");
    println!("counts_by_detector:");
    for (detector, count) in &counts_by_detector {
        println!("  {detector}: {count}");
    }
    println!("blocked_content_sha256_by_detector:");
    for (detector, hashes) in &hashes_by_detector {
        println!("  {detector}:");
        for hash in hashes {
            println!("    {hash}");
        }
    }
}

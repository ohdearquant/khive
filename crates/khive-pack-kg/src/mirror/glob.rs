//! Minimal include/exclude glob matching for the workspace mirror.
//!
//! ADR-087's scope config only ever needs three pattern shapes: literal path
//! segments, a single-segment `*` wildcard (e.g. `*.ndjson`), and a
//! multi-segment `**` wildcard (e.g. `notes/**`). Pulling in an external
//! glob crate for that narrow, fully-tested need is more machinery than the
//! requirement calls for (`PI_AEP`) — this module is self-contained and has
//! no dependency on file-system state.

/// Default include patterns (ADR-087 Decision item 4), relative to the
/// `.khive/` directory root.
pub const DEFAULT_INCLUDE: &[&str] = &[
    "notes/**",
    "reports/**",
    "codex_reviews/**",
    "workspaces/*/artifacts/**",
];

/// Default exclude patterns (ADR-087 Decision item 4 + Non-goals). Exclude
/// always wins over include (see [`is_included`]).
pub const DEFAULT_EXCLUDE: &[&str] = &[
    "kg/*.ndjson",
    "kg/schema.yaml",
    "scripts/**",
    "**/target/**",
    "**/*.db",
    "**/*.db-wal",
    "**/*.db-shm",
];

/// True when `path` (a `/`-separated relative path, no leading `/`) matches
/// `pattern`.
pub fn matches(pattern: &str, path: &str) -> bool {
    let pat_segs: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let path_segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match_segments(&pat_segs, &path_segs)
}

/// True when `rel_path` matches at least one `include` pattern and none of
/// the `exclude` patterns.
pub fn is_included(rel_path: &str, include: &[String], exclude: &[String]) -> bool {
    if exclude.iter().any(|p| matches(p, rel_path)) {
        return false;
    }
    include.iter().any(|p| matches(p, rel_path))
}

fn match_segments(pat: &[&str], path: &[&str]) -> bool {
    match pat.first() {
        None => path.is_empty(),
        Some(&"**") => {
            // `**` matches zero or more whole segments: try consuming none
            // first (so a trailing `**` matches the empty remainder), then
            // fall back to consuming one path segment at a time.
            if match_segments(&pat[1..], path) {
                return true;
            }
            match path.split_first() {
                Some((_, rest)) => match_segments(pat, rest),
                None => false,
            }
        }
        Some(seg) => match path.split_first() {
            Some((first, rest)) => segment_matches(seg, first) && match_segments(&pat[1..], rest),
            None => false,
        },
    }
}

/// Single-segment glob: `*` matches any run of characters within the
/// segment (never `/`, since segments are already split on it going in).
fn segment_matches(pattern: &str, segment: &str) -> bool {
    fn helper(p: &[u8], s: &[u8]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some(b'*') => {
                for i in 0..=s.len() {
                    if helper(&p[1..], &s[i..]) {
                        return true;
                    }
                }
                false
            }
            Some(&c) => s.first() == Some(&c) && helper(&p[1..], &s[1..]),
        }
    }
    helper(pattern.as_bytes(), segment.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_segments_match_exactly() {
        assert!(matches("notes/handoffs", "notes/handoffs"));
        assert!(!matches("notes/handoffs", "notes/summaries"));
    }

    #[test]
    fn double_star_matches_nested_paths() {
        assert!(matches("notes/**", "notes/handoffs/handoff_1.md"));
        assert!(matches("notes/**", "notes/x.md"));
        // A trailing `**` also matches the directory itself with nothing
        // past it.
        assert!(matches("notes/**", "notes"));
        assert!(!matches("notes/**", "reports/x.md"));
    }

    #[test]
    fn single_star_matches_within_one_segment_only() {
        assert!(matches("kg/*.ndjson", "kg/entities.ndjson"));
        assert!(matches("kg/*.ndjson", "kg/edges.ndjson"));
        assert!(!matches("kg/*.ndjson", "kg/remotes/entities.ndjson"));
    }

    #[test]
    fn double_star_in_middle_matches_any_depth() {
        assert!(matches("**/target/**", "workspaces/foo/target/debug/build"));
        assert!(matches("**/target/**", "target/release"));
        assert!(!matches("**/target/**", "workspaces/foo/build/debug"));
    }

    #[test]
    fn glob_star_matches_full_path_wildcard() {
        assert!(matches(
            "workspaces/*/artifacts/**",
            "workspaces/20260707-topic/artifacts/report.md"
        ));
        assert!(!matches(
            "workspaces/*/artifacts/**",
            "workspaces/20260707-topic/notes/report.md"
        ));
    }

    #[test]
    fn exclude_wins_over_include() {
        let include = vec!["notes/**".to_string()];
        let exclude = vec!["notes/scratch/**".to_string()];
        assert!(is_included("notes/handoffs/h1.md", &include, &exclude));
        assert!(!is_included("notes/scratch/temp.md", &include, &exclude));
    }

    #[test]
    fn default_include_exclude_cover_adr087_examples() {
        let include: Vec<String> = DEFAULT_INCLUDE.iter().map(|s| s.to_string()).collect();
        let exclude: Vec<String> = DEFAULT_EXCLUDE.iter().map(|s| s.to_string()).collect();

        assert!(is_included(
            "notes/handoffs/handoff_20260707.md",
            &include,
            &exclude
        ));
        assert!(is_included("reports/audit.md", &include, &exclude));
        assert!(is_included(
            "codex_reviews/codex_review_pr700.md",
            &include,
            &exclude
        ));
        assert!(is_included(
            "workspaces/20260707-topic/artifacts/final.md",
            &include,
            &exclude
        ));

        assert!(!is_included("kg/entities.ndjson", &include, &exclude));
        assert!(!is_included("kg/schema.yaml", &include, &exclude));
        assert!(!is_included("scripts/audit_crate.py", &include, &exclude));
        assert!(!is_included(
            "workspaces/20260707-topic/notes/scratch.md",
            &include,
            &exclude
        ));
    }
}

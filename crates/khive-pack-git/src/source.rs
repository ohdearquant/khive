//! `git.digest` source resolution (ADR-088 Amendment 1): local paths vs.
//! `https://` remote URLs, canonicalization, and `github.com` owner/repo
//! slug derivation for `gh`-based issue/PR ingestion.

use std::path::PathBuf;

/// A digest source, resolved from the `git.digest` verb's `source` argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DigestSource {
    /// An absolute local path known to contain a `.git` entry (directory or,
    /// for worktrees, a `gitdir:` pointer file).
    Local(PathBuf),
    /// A remote `https://` URL to clone/fetch into the scratch cache.
    Remote {
        /// Canonical form used as the cache key (trailing `/` and `.git`
        /// suffix stripped) — same URL always maps to the same cache slot.
        canonical: String,
        /// `Some((owner, repo))` when the host is `github.com` — the only
        /// host `gh` can serve issues/PRs for. `None` for any other https
        /// host: the amendment's commits-only degradation applies.
        gh_slug: Option<(String, String)>,
    },
}

/// Parse and validate the `source` argument.
///
/// - `https://` URLs are accepted for any host.
/// - `ssh://`, `git://`, `http://`, and scp-like `user@host:path` shorthand
///   are rejected outright — no interactive auth in the daemon (ADR-088
///   Amendment 1, security posture).
/// - Anything else is treated as a local path: it must be absolute and must
///   contain a `.git` entry; arbitrary directory walking is not performed.
pub fn parse_source(raw: &str) -> Result<DigestSource, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("source must not be empty".to_string());
    }

    if let Some(rest) = trimmed.strip_prefix("https://") {
        if rest.is_empty() {
            return Err(format!("source {trimmed:?} is not a valid https:// URL"));
        }
        let canonical = canonicalize_https_url(trimmed);
        let gh_slug = github_slug(&canonical);
        return Ok(DigestSource::Remote { canonical, gh_slug });
    }

    for (scheme, hint) in [
        (
            "ssh://",
            "SSH URLs are rejected in v1 (no interactive auth in the daemon)",
        ),
        (
            "git://",
            "the git:// protocol is rejected in v1 -- use an https:// URL",
        ),
        ("http://", "plain http:// URLs are rejected -- use https://"),
    ] {
        if trimmed.starts_with(scheme) {
            return Err(format!("source {trimmed:?}: {hint}"));
        }
    }
    if is_scp_shorthand(trimmed) {
        return Err(format!(
            "source {trimmed:?}: SSH shorthand URLs are rejected in v1 (no interactive auth in the daemon)"
        ));
    }

    if !trimmed.starts_with('/') {
        return Err(format!(
            "source {trimmed:?} is neither an https:// URL nor an absolute local path (relative paths are rejected)"
        ));
    }
    let path = PathBuf::from(trimmed);
    if !path.join(".git").exists() {
        return Err(format!(
            "local path {trimmed:?} does not contain a .git entry"
        ));
    }
    Ok(DigestSource::Local(path))
}

/// `true` for scp-like SSH shorthand (`user@host:path`, e.g.
/// `git@github.com:org/repo.git`) — recognized by an `@` before the first
/// `:` and no leading `/`.
fn is_scp_shorthand(s: &str) -> bool {
    if s.starts_with('/') {
        return false;
    }
    let Some(at) = s.find('@') else {
        return false;
    };
    let Some(colon) = s.find(':') else {
        return false;
    };
    if colon <= at {
        return false;
    }
    let user = &s[..at];
    !user.is_empty()
        && user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

fn canonicalize_https_url(url: &str) -> String {
    let mut s = url.trim_end_matches('/').to_string();
    if let Some(stripped) = s.strip_suffix(".git") {
        s = stripped.to_string();
    }
    s
}

/// Derive `(owner, repo)` from a canonicalized `https://github.com/...` URL.
/// Returns `None` for any other host, or a github.com URL with fewer than
/// two path segments.
fn github_slug(canonical: &str) -> Option<(String, String)> {
    let rest = canonical.strip_prefix("https://")?;
    let mut parts = rest.splitn(2, '/');
    let host = parts.next()?;
    if !matches!(host, "github.com" | "www.github.com") {
        return None;
    }
    let path = parts.next()?;
    let mut segs = path.split('/').filter(|s| !s.is_empty());
    let owner = segs.next()?;
    let repo = segs.next()?;
    Some((owner.to_string(), repo.to_string()))
}

/// Basename used as the default `project` entity name and as a fallback
/// scratch-directory label.
pub fn repo_basename(source: &DigestSource) -> String {
    match source {
        DigestSource::Local(p) => p
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("repo")
            .to_string(),
        DigestSource::Remote { canonical, .. } => canonical
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("repo")
            .to_string(),
    }
}

/// Deterministic cache-key for the scratch clone directory: the first 16 hex
/// characters of `blake3(canonical_url)` -- short enough for a filesystem
/// path component, long enough that collisions are not a practical concern
/// for a handful of cached repos.
pub fn cache_key(canonical_url: &str) -> String {
    blake3::hash(canonical_url.as_bytes()).to_hex()[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_github_url_derives_slug_and_canonical_form() {
        let src = parse_source("https://github.com/ohdearquant/khive.git").unwrap();
        match src {
            DigestSource::Remote { canonical, gh_slug } => {
                assert_eq!(canonical, "https://github.com/ohdearquant/khive");
                assert_eq!(
                    gh_slug,
                    Some(("ohdearquant".to_string(), "khive".to_string()))
                );
            }
            other => panic!("expected Remote, got {other:?}"),
        }
    }

    #[test]
    fn https_github_url_trailing_slash_canonicalizes_same_as_no_slash() {
        let a = parse_source("https://github.com/org/repo/").unwrap();
        let b = parse_source("https://github.com/org/repo").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn https_non_github_host_has_no_gh_slug() {
        let src = parse_source("https://gitlab.com/org/repo").unwrap();
        match src {
            DigestSource::Remote { gh_slug, .. } => assert_eq!(gh_slug, None),
            other => panic!("expected Remote, got {other:?}"),
        }
    }

    #[test]
    fn ssh_url_rejected() {
        let err = parse_source("ssh://git@github.com/org/repo.git").unwrap_err();
        assert!(err.contains("SSH"), "{err}");
    }

    #[test]
    fn scp_shorthand_rejected() {
        let err = parse_source("git@github.com:org/repo.git").unwrap_err();
        assert!(err.contains("SSH"), "{err}");
    }

    #[test]
    fn git_protocol_rejected() {
        let err = parse_source("git://github.com/org/repo.git").unwrap_err();
        assert!(err.contains("git://"), "{err}");
    }

    #[test]
    fn plain_http_rejected() {
        let err = parse_source("http://github.com/org/repo").unwrap_err();
        assert!(err.contains("http://"), "{err}");
    }

    #[test]
    fn relative_local_path_rejected() {
        let err = parse_source("relative/path/repo").unwrap_err();
        assert!(err.contains("absolute"), "{err}");
    }

    #[test]
    fn absolute_local_path_without_git_dir_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = parse_source(dir.path().to_str().unwrap()).unwrap_err();
        assert!(err.contains(".git"), "{err}");
    }

    #[test]
    fn absolute_local_path_with_git_dir_accepted() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        let src = parse_source(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(src, DigestSource::Local(dir.path().to_path_buf()));
    }

    #[test]
    fn repo_basename_local_uses_dir_name() {
        let src = DigestSource::Local(PathBuf::from("/home/x/my-repo"));
        assert_eq!(repo_basename(&src), "my-repo");
    }

    #[test]
    fn repo_basename_remote_uses_last_path_segment() {
        let src = DigestSource::Remote {
            canonical: "https://github.com/org/my-repo".to_string(),
            gh_slug: Some(("org".to_string(), "my-repo".to_string())),
        };
        assert_eq!(repo_basename(&src), "my-repo");
    }

    #[test]
    fn cache_key_is_deterministic_and_16_hex_chars() {
        let a = cache_key("https://github.com/org/repo");
        let b = cache_key("https://github.com/org/repo");
        let c = cache_key("https://github.com/org/other");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|ch| ch.is_ascii_hexdigit()));
    }
}

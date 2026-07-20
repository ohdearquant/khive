//! `git.digest` source resolution (ADR-088 Amendment 1): local paths vs.
//! `https://` remote URLs, canonicalization, and `github.com` owner/repo
//! slug derivation for `gh`-based issue/PR ingestion.
//!
//! Also owns repo-anchor identity derivation (issue #1173): a canonical
//! `host/owner/repo` slug (or a path-derived fallback for a remote-less
//! local repo) that the same repository resolves to regardless of which
//! spelling — https URL, ssh/scp remote, local clone path — a given
//! `git.digest` call used.

use std::path::{Path, PathBuf};
use std::process::Command;

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

/// `true` for scp-like SSH shorthand (`[user@]host:path`, e.g.
/// `git@github.com:org/repo.git` or the bare `github.com:org/repo.git` —
/// the SCP user is optional in git's own remote grammar).
fn is_scp_shorthand(s: &str) -> bool {
    scp_parts(s).is_some()
}

/// Split `[user@]host:path` shorthand into `(host, path)`, or `None` when
/// `s` doesn't have that shape. A bracketed IPv6 host (`git@[::1]:path` or
/// the bare `[::1]:path`) is handled by locating the `[...]` span and
/// requiring the very next character to be `:` -- naively taking the first
/// `:` in the string (as the plain-host branch below does) would land on a
/// colon INSIDE the address instead of the host/path separator, which is
/// what let a bracketed-IPv6 SCP origin fall through to `local:<path>`
/// instead of converging with its remote slug (ADR-088 Amendment 2
/// round-2 finding). For a plain (non-bracketed) host, the segment
/// (whatever precedes the first `:`, minus any `user@` prefix) must
/// contain no `/` -- this is what keeps a local path with an embedded
/// colon, e.g. `local/path:with-colon`, from being misparsed as SCP
/// shorthand: `local/path` fails the host check because of the `/`, so
/// `scp_parts` returns `None` and the caller falls through to local-path
/// handling.
fn scp_parts(s: &str) -> Option<(String, &str)> {
    if s.starts_with('/') {
        return None;
    }

    let (user, rest) = match s.find('@') {
        Some(at) => (Some(&s[..at]), &s[at + 1..]),
        None => (None, s),
    };
    if let Some(user) = user {
        if user.is_empty()
            || !user
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return None;
        }
    }

    if let Some(inner) = rest.strip_prefix('[') {
        let end = inner.find(']')?;
        let host_inner = &inner[..end];
        if host_inner.is_empty() {
            return None;
        }
        let path = inner[end + 1..].strip_prefix(':')?;
        if path.is_empty() {
            return None;
        }
        return Some((format!("[{host_inner}]"), path));
    }

    let colon = rest.find(':')?;
    let host = &rest[..colon];
    if host.is_empty()
        || host.contains('/')
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
    {
        return None;
    }
    let path = &rest[colon + 1..];
    if path.is_empty() {
        return None;
    }
    Some((host.to_string(), path))
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

/// The repo-anchor identity property key (issue #1173) — the durable
/// matching key for `resolve_or_create_project`. `repo_url` remains display
/// metadata only; this is the one property that is matched on.
pub const REPO_SLUG_PROPERTY: &str = "repo_slug";

/// Split `host` from `path` out of a URL/remote body that has already had
/// its scheme and any `user[:pass]@` userinfo prefix stripped -- e.g.
/// `github.com/owner/repo` from `https://github.com/owner/repo`, or
/// `github.com/owner/repo` from the `host/owner/repo` remainder of an
/// `ssh://user@host/owner/repo` URL. `rfind('@')` (not the first `@`) drops
/// userinfo because a password component can itself contain `@`. Any port
/// on the authority is stripped via `strip_port` -- the slug identity is
/// host+path only, so `github.com:2222/org/repo` and `github.com/org/repo`
/// must converge.
fn split_host_path(rest: &str) -> Option<(String, String)> {
    let after_userinfo = match rest.rfind('@') {
        Some(pos) => &rest[pos + 1..],
        None => rest,
    };
    let (host, path) = after_userinfo.split_once('/')?;
    if host.is_empty() || path.is_empty() {
        return None;
    }
    let host = strip_port(host);
    if host.is_empty() {
        return None;
    }
    Some((host, path.to_string()))
}

/// Strip a trailing `:port` from a URL authority's host segment. Handles a
/// bracketed IPv6 host (`[::1]:2222` -> `[::1]`, brackets kept -- brackets
/// are what make an IPv6 literal unambiguous in a slug, since the address
/// itself contains colons) as well as a plain hostname (`github.com:2222`
/// -> `github.com`). A host with no port passes through unchanged.
fn strip_port(host: &str) -> String {
    if let Some(rest) = host.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            return format!("[{}]", &rest[..end]);
        }
        return host.to_string();
    }
    match host.split_once(':') {
        Some((h, _port)) => h.to_string(),
        None => host.to_string(),
    }
}

/// Normalize ANY git remote URL spelling -- `https://`, `ssh://`, the
/// `git://` protocol, or scp-like shorthand (`git@host:owner/repo.git`) --
/// into a lowercase-host `host/owner/repo` slug (issue #1173). This is
/// broader than `github_slug` (which only recognizes `github.com` and
/// requires the caller to have already canonicalized an `https://` URL):
/// this function accepts every spelling `git remote get-url origin` can
/// hand back, because a local clone's configured origin is not restricted
/// to the schemes `parse_source` accepts for the top-level `source`
/// argument. Returns `None` when the remainder doesn't parse into a host
/// plus at least two path segments (owner + repo).
///
/// The host is lowercased (DNS is case-insensitive); owner/repo segments
/// are preserved verbatim -- case-folding those risks collapsing two
/// genuinely distinct repos on a case-sensitive host.
pub fn remote_url_to_slug(url: &str) -> Option<String> {
    let trimmed = url.trim();
    let trimmed = strip_query_and_fragment(trimmed);
    let trimmed = trimmed.trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    if trimmed.is_empty() {
        return None;
    }

    let (host, path) = if let Some(rest) = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .or_else(|| trimmed.strip_prefix("git://"))
    {
        split_host_path(rest)?
    } else if let Some(rest) = trimmed.strip_prefix("ssh://") {
        split_host_path(rest)?
    } else if let Some((host, path)) = scp_parts(trimmed) {
        (host, path.to_string())
    } else {
        return None;
    };

    // ALL path segments are preserved (ADR-088 Amendment 2): a nested-group
    // remote like host/group/subgroup/repo keeps every segment, so two
    // repositories under one subgroup never collapse onto one slug. Fewer
    // than two segments, or any empty segment, does not normalize.
    let segs: Vec<&str> = path.split('/').collect();
    if segs.len() < 2 || segs.iter().any(|s| s.is_empty()) {
        return None;
    }
    let host = host.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    Some(format!("{host}/{}", segs.join("/")))
}

/// Strip a URL's query (`?...`) and fragment (`#...`) components, in that
/// lexical order, before any path-segment splitting or `.git`-suffix
/// stripping (ADR-088 Amendment 2) -- a caller-supplied source URL can
/// carry an access token in its query string, and that token must never
/// reach the derived slug (or, via `redact_repo_url`, storage).
fn strip_query_and_fragment(s: &str) -> &str {
    let s = match s.find('#') {
        Some(i) => &s[..i],
        None => s,
    };
    match s.find('?') {
        Some(i) => &s[..i],
        None => s,
    }
}

/// Redact credential and tracking material from a URL before it is
/// persisted as `properties.repo_url` display metadata (ADR-088 Amendment
/// 2): userinfo (`user[:pass]@`), the query string, and the fragment are
/// stripped. This is for the STORED value only -- the in-memory canonical
/// URL used for cloning/gh operations is never passed through this.
pub(crate) fn redact_repo_url(url: &str) -> String {
    let stripped = strip_query_and_fragment(url);
    for scheme in ["https://", "http://", "git://", "ssh://"] {
        if let Some(rest) = stripped.strip_prefix(scheme) {
            let authority_end = rest.find('/').unwrap_or(rest.len());
            let (authority, path) = rest.split_at(authority_end);
            let authority = match authority.rfind('@') {
                Some(pos) => &authority[pos + 1..],
                None => authority,
            };
            return format!("{scheme}{authority}{path}");
        }
    }
    stripped.to_string()
}

/// Read the local repo's configured `origin` remote URL via `git -C <path>
/// remote get-url origin`. Returns `None` for any failure (no `origin`
/// remote, `git` not on PATH, not a git repo) -- a local repo with no
/// remote is a valid, expected state (see `repo_identity`), not an error.
///
/// Runs the blocking `Command::output()` call on a `spawn_blocking` thread:
/// every caller of `repo_identity` is an async handler path, and a
/// synchronous subprocess spawn there would block the async runtime worker
/// thread it runs on.
async fn local_origin_remote_url(canonical_repo_path: &Path) -> Option<String> {
    let path = canonical_repo_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let out = Command::new("git")
            .arg("-C")
            .arg(&path)
            .args(["remote", "get-url", "origin"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if url.is_empty() {
            None
        } else {
            Some(url)
        }
    })
    .await
    .ok()
    .flatten()
}

/// Resolve the canonical repo-anchor identity (issue #1173) for a digest
/// source -- the value stored in `properties.repo_slug` and matched on by
/// `resolve_or_create_project`.
///
/// - `Remote`: the `host/owner/repo` slug of the canonical URL.
/// - `Local`: the canonicalized path's configured `origin` remote, sluggified
///   the same way -- so a repo digested once via `https://host/owner/repo`
///   and once via a local clone of that same remote converge on one anchor.
///   When the local repo has no `origin` remote (or `remote_url_to_slug`
///   can't parse it), the identity falls back to a `local:<canonical path>`
///   form -- clearly distinct from a `host/owner/repo` slug (no host name
///   contains a `:`) so it can never collide with one, but scoped only to
///   that exact path: two clones of the same remote-less repo at different
///   paths do NOT converge (there is no remote to prove they're the same
///   repository).
pub async fn repo_identity(source: &DigestSource) -> String {
    match source {
        DigestSource::Remote { canonical, .. } => {
            // An accepted URL that does not normalize (e.g. a single path
            // segment) falls back to the URL itself as identity -- but
            // credential-redacted and stripped of query/fragment, so a
            // token embedded in the source can never persist in `repo_slug`
            // and query-only spelling variants converge on one identity.
            remote_url_to_slug(canonical).unwrap_or_else(|| redact_repo_url(canonical))
        }
        DigestSource::Local(path) => {
            let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            if let Some(origin) = local_origin_remote_url(&canon).await {
                if let Some(slug) = remote_url_to_slug(&origin) {
                    return slug;
                }
            }
            format!("local:{}", canon.to_string_lossy())
        }
    }
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
        // Derived from the fully redacted URL: a root-path remote like
        // `https://user:token@example.com` has no path segment to split off,
        // so without redaction the authority -- userinfo included -- would
        // become the persisted project name.
        DigestSource::Remote { canonical, .. } => redact_repo_url(canonical)
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| "repo".to_string()),
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
    fn bare_scp_shorthand_without_userinfo_rejected() {
        let err = parse_source("github.com:org/repo.git").unwrap_err();
        assert!(err.contains("SSH"), "{err}");
    }

    #[test]
    fn scp_shorthand_bracketed_ipv6_host_rejected_as_source() {
        let err = parse_source("git@[::1]:group/repo.git").unwrap_err();
        assert!(err.contains("SSH"), "{err}");
    }

    #[test]
    fn bare_scp_shorthand_bracketed_ipv6_host_rejected_as_source() {
        let err = parse_source("[::1]:group/repo.git").unwrap_err();
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

    // -- repo identity slug (issue #1173) --------------------------------

    #[test]
    fn remote_url_to_slug_normalization_table() {
        let expected = "github.com/org/repo";
        let spellings = [
            "https://github.com/org/repo",
            "https://github.com/org/repo.git",
            "https://github.com/org/repo/",
            "https://github.com/org/repo.git/",
            "http://github.com/org/repo",
            "https://token@github.com/org/repo.git",
            "https://user:token@github.com/org/repo.git",
            "ssh://git@github.com/org/repo.git",
            "git@github.com:org/repo.git",
            "git@github.com:org/repo",
            "git://github.com/org/repo.git",
            // Bare SCP shorthand -- the SCP user is optional.
            "github.com:org/repo.git",
            "github.com:org/repo",
            // ssh:// with a port -- the port is not part of the identity.
            "ssh://git@github.com:2222/org/repo.git",
            "ssh://github.com:2222/org/repo",
            // https/http/git:// with an authority port -- likewise not part
            // of the identity (round-2 finding: only ssh:// was covered).
            "https://github.com:8443/org/repo",
            "https://github.com:8443/org/repo.git",
            "http://github.com:8080/org/repo",
            "git://github.com:9418/org/repo",
            // Host case is folded; owner/repo case is preserved separately below.
            "https://GitHub.com/org/repo",
            // A leading www. host label is folded (ADR-088 Amendment 2).
            "https://www.github.com/org/repo",
            // A query string and/or fragment must not affect normalization
            // and must never leak a token into the slug.
            "https://github.com/org/repo?ref=nightly",
            "https://github.com/org/repo.git?ref=nightly",
            "https://github.com/org/repo#readme",
            "https://github.com/org/repo.git?token=SECRET#section",
        ];
        for s in spellings {
            assert_eq!(
                remote_url_to_slug(s).as_deref(),
                Some(expected),
                "spelling {s:?} should normalize to {expected:?}"
            );
        }
    }

    #[test]
    fn remote_url_to_slug_strips_query_and_fragment_before_splitting() {
        let slug = remote_url_to_slug("https://github.com/org/repo?token=SECRETTOKEN").unwrap();
        assert_eq!(slug, "github.com/org/repo");
        assert!(!slug.contains("SECRETTOKEN"), "{slug}");
    }

    #[test]
    fn remote_url_to_slug_folds_leading_www_host_label() {
        assert_eq!(
            remote_url_to_slug("https://www.gitlab.com/org/repo").as_deref(),
            Some("gitlab.com/org/repo")
        );
    }

    #[test]
    fn scp_shorthand_bracketed_ipv6_host_normalizes_directly() {
        assert_eq!(
            remote_url_to_slug("git@[::1]:group/repo.git").as_deref(),
            Some("[::1]/group/repo")
        );
        assert_eq!(
            remote_url_to_slug("[::1]:group/repo.git").as_deref(),
            Some("[::1]/group/repo"),
            "the no-user bracketed SCP variant must also normalize"
        );
    }

    #[test]
    fn redact_repo_url_strips_userinfo_query_and_fragment() {
        assert_eq!(
            redact_repo_url("https://user:tok3n@github.com/org/repo?token=SECRET#frag"),
            "https://github.com/org/repo"
        );
        assert_eq!(
            redact_repo_url("https://github.com/org/repo"),
            "https://github.com/org/repo"
        );
    }

    #[test]
    fn remote_url_to_slug_preserves_all_nested_path_segments() {
        assert_eq!(
            remote_url_to_slug("https://gitlab.com/group/subgroup/repo.git").as_deref(),
            Some("gitlab.com/group/subgroup/repo")
        );
        assert_eq!(
            remote_url_to_slug("git@gitlab.com:group/subgroup/repo.git").as_deref(),
            Some("gitlab.com/group/subgroup/repo"),
            "scp spelling of a nested-group remote must converge with https"
        );
        assert_ne!(
            remote_url_to_slug("https://gitlab.com/group/subgroup/repo"),
            remote_url_to_slug("https://gitlab.com/group/subgroup/other"),
            "two repos under one subgroup must not collapse onto one slug"
        );
    }

    #[test]
    fn remote_url_to_slug_preserves_owner_repo_case() {
        assert_eq!(
            remote_url_to_slug("https://github.com/Org/Repo").as_deref(),
            Some("github.com/Org/Repo")
        );
    }

    #[test]
    fn remote_url_to_slug_bracketed_ipv6_host_with_port() {
        assert_eq!(
            remote_url_to_slug("ssh://git@[::1]:2222/org/repo").as_deref(),
            Some("[::1]/org/repo")
        );
    }

    #[test]
    fn remote_url_to_slug_bracketed_ipv6_host_without_port() {
        assert_eq!(
            remote_url_to_slug("ssh://git@[::1]/org/repo").as_deref(),
            Some("[::1]/org/repo")
        );
    }

    #[test]
    fn remote_url_to_slug_rejects_empty_owner_or_repo_segment() {
        assert_eq!(remote_url_to_slug("https://github.com/org//repo"), None);
        assert_eq!(remote_url_to_slug("https://github.com//repo"), None);
        assert_eq!(remote_url_to_slug("git@github.com:/repo"), None);
        assert_eq!(remote_url_to_slug("git@github.com:org/"), None);
    }

    #[test]
    fn remote_url_to_slug_does_not_misparse_local_path_with_colon() {
        assert_eq!(remote_url_to_slug("local/path:with-colon"), None);
    }

    #[test]
    fn remote_url_to_slug_rejects_unparseable_input() {
        assert_eq!(remote_url_to_slug(""), None);
        assert_eq!(remote_url_to_slug("not-a-url"), None);
        assert_eq!(remote_url_to_slug("https://github.com/onlyowner"), None);
    }

    fn init_repo_with_origin(dir: &Path, origin: &str) {
        for args in [vec!["init", "-q"], vec!["remote", "add", "origin", origin]] {
            let status = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(&args)
                .status()
                .expect("spawn git");
            assert!(status.success(), "git {args:?} failed");
        }
    }

    #[tokio::test]
    async fn repo_identity_https_and_ssh_spellings_of_same_remote_converge() {
        let https = DigestSource::Remote {
            canonical: "https://github.com/org/repo".to_string(),
            gh_slug: Some(("org".to_string(), "repo".to_string())),
        };
        assert_eq!(repo_identity(&https).await, "github.com/org/repo");
    }

    #[tokio::test]
    async fn repo_identity_unsluggable_remote_fallback_is_redacted_and_converges() {
        // A single-path-segment URL does not normalize to a slug; the raw-URL
        // identity fallback must never carry userinfo/query/fragment, and
        // query-only spelling variants must converge on one identity.
        let with_secret = DigestSource::Remote {
            canonical: "https://user:tok3n@example.com/repo?token=SECRET#frag".to_string(),
            gh_slug: None,
        };
        let identity = repo_identity(&with_secret).await;
        assert_eq!(identity, "https://example.com/repo");
        assert!(!identity.contains("tok3n") && !identity.contains("SECRET"));

        let bare = DigestSource::Remote {
            canonical: "https://example.com/repo".to_string(),
            gh_slug: None,
        };
        assert_eq!(repo_identity(&bare).await, identity);
    }

    #[test]
    fn repo_basename_strips_query_and_fragment() {
        let src = DigestSource::Remote {
            canonical: "https://example.com/org/repo?x=1#frag".to_string(),
            gh_slug: None,
        };
        assert_eq!(repo_basename(&src), "repo");
    }

    #[test]
    fn repo_basename_never_carries_userinfo_for_root_path_remotes() {
        let src = DigestSource::Remote {
            canonical: "https://user:tok3n@example.com".to_string(),
            gh_slug: None,
        };
        let name = repo_basename(&src);
        assert_eq!(name, "example.com");
        assert!(!name.contains("tok3n"));
    }

    #[tokio::test]
    async fn repo_identity_local_path_with_origin_matches_remote_identity() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo_with_origin(dir.path(), "git@github.com:org/repo.git");

        let local = DigestSource::Local(dir.path().to_path_buf());
        let remote = DigestSource::Remote {
            canonical: "https://github.com/org/repo".to_string(),
            gh_slug: Some(("org".to_string(), "repo".to_string())),
        };
        assert_eq!(repo_identity(&local).await, repo_identity(&remote).await);
        assert_eq!(repo_identity(&local).await, "github.com/org/repo");
    }

    /// Round-2 finding: a local clone whose `origin` is bracketed-IPv6 SCP
    /// shorthand must converge with the equivalent `ssh://` spelling's
    /// slug, not fall back to `local:<path>`.
    #[tokio::test]
    async fn repo_identity_local_path_with_bracketed_ipv6_scp_origin_converges() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo_with_origin(dir.path(), "git@[::1]:group/repo.git");

        let local = DigestSource::Local(dir.path().to_path_buf());
        let remote = DigestSource::Remote {
            canonical: "https://[::1]/group/repo".to_string(),
            gh_slug: None,
        };
        assert_eq!(repo_identity(&local).await, "[::1]/group/repo");
        assert_eq!(repo_identity(&local).await, repo_identity(&remote).await);
    }

    #[tokio::test]
    async fn repo_identity_local_path_without_remote_falls_back_to_path_form() {
        let dir = tempfile::tempdir().expect("tempdir");
        let status = Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["init", "-q"])
            .status()
            .expect("spawn git init");
        assert!(status.success());

        let local = DigestSource::Local(dir.path().to_path_buf());
        let identity = repo_identity(&local).await;
        assert!(identity.starts_with("local:"), "{identity}");
        let canon = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(identity, format!("local:{}", canon.to_string_lossy()));
    }
}

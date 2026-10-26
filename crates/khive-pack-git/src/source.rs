//! `git.digest` source resolution (ADR-088 Amendment 1): local paths vs.
//! `https://` remote URLs, canonicalization, and `github.com` owner/repo
//! slug derivation for repo identity and parse-time GitHub hints. Runtime
//! `gh` capability is established separately by a source-bound probe.
//!
//! Also owns repo-anchor identity derivation (issue #1173): a canonical
//! `host/owner/repo` slug (or a path-derived fallback for a remote-less
//! local repo) that the same repository resolves to regardless of which
//! spelling — https URL, ssh/scp remote, local clone path — a given
//! `git.digest` call used.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A digest source, resolved from the `git.digest` verb's `source` argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DigestSource {
    /// An absolute local path known to contain a `.git` entry (directory or,
    /// for worktrees, a `gitdir:` pointer file).
    Local(PathBuf),
    /// A remote `https://` URL, cloned/fetched only when commits are requested.
    Remote {
        /// Canonical form used as the cache key (trailing `/` and `.git`
        /// suffix stripped) — same URL always maps to the same cache slot.
        canonical: String,
        /// Parse-time hint for a syntactic `github.com/<owner>/<repo>` URL.
        /// This is not a capability decision: the ingest core uses
        /// source-bound `gh repo view <owner/repo>` and trusts only its
        /// successful `nameWithOwner` result before issue/PR ingestion
        /// (#1617), whether or not a commit clone was requested.
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
/// the bare `[::1]:path`) uses the colon immediately after the `[...]` span
/// as its host/path separator. Colons inside the address are not separators,
/// so bracketed-IPv6 SCP origins converge with their remote slug rather
/// than a local-path identity (ADR-088 Amendment 2).
/// For a plain (non-bracketed) host, the segment
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

/// Canonicalize the identity fallback for an accepted remote URL without
/// mutating the URL retained in [`DigestSource::Remote`] for clone/fetch.
/// Credential and tracking material is redacted first; only then are the
/// path decorations that do not identify a repository (`/` and `.git`)
/// removed. The order matters for spellings such as `.git?token=...` and
/// `.git/?view=...`, whose suffix is not visible until the query is gone.
/// Finally the host is lowercased (DNS is case-insensitive, so
/// `https://Example.com/repo` and `https://example.com/repo` are the same
/// remote) -- the path is left untouched because owner/repo segments are
/// case-sensitive on the host and folding them risks collapsing two
/// genuinely distinct repositories onto one anchor.
pub(crate) fn canonical_remote_identity(url: &str) -> String {
    let redacted = redact_repo_url(url);
    let without_slash = redacted.trim_end_matches('/');
    let without_git_suffix = without_slash.strip_suffix(".git").unwrap_or(without_slash);
    lowercase_host_component(without_git_suffix)
}

/// Lowercase only the host component of a `scheme://host/path` string,
/// leaving the scheme and path untouched. `parse_source` accepts only
/// `https://` remotes, so `value` is always scheme-prefixed here; a value
/// without a recognized scheme is returned unchanged.
fn lowercase_host_component(value: &str) -> String {
    for scheme in ["https://", "http://", "git://", "ssh://"] {
        if let Some(rest) = value.strip_prefix(scheme) {
            let authority_end = rest.find('/').unwrap_or(rest.len());
            let (authority, path) = rest.split_at(authority_end);
            return format!("{scheme}{}{path}", authority.to_ascii_lowercase());
        }
    }
    value.to_string()
}

/// Derive a credential-free `(owner, repo)` from a canonicalized
/// `https://github.com/...` URL. Query/fragment material is stripped by the
/// shared remote normalizer before either component can become a `gh` argv
/// value. Returns `None` for any other host or anything other than the exact
/// `github.com/<owner>/<repo>` shape.
fn github_slug(canonical: &str) -> Option<(String, String)> {
    canonical.strip_prefix("https://")?;
    let normalized = remote_url_to_slug(canonical)?;
    let mut segs = normalized.split('/');
    let (Some("github.com"), Some(owner), Some(repo), None) =
        (segs.next(), segs.next(), segs.next(), segs.next())
    else {
        return None;
    };
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
/// `ssh://user@host/owner/repo` URL. The userinfo search is confined to the
/// authority (everything before the first `/`): a path segment may itself
/// contain `@`, and searching past the authority would replace the real host
/// with text from the path. Within the authority, `rfind('@')` (not the first
/// `@`) drops userinfo because a password component can itself contain `@`.
/// Any port on the authority is stripped via `strip_port` -- the slug identity
/// is host+path only, so `github.com:2222/org/repo` and `github.com/org/repo`
/// must converge.
fn split_host_path(rest: &str) -> Option<(String, String)> {
    let (authority, path) = rest.split_once('/')?;
    let host = match authority.rfind('@') {
        Some(pos) => &authority[pos + 1..],
        None => authority,
    };
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

/// Redact credential and tracking material before a remote spelling is used
/// as stored `properties.repo_url` display metadata or as the unsluggable
/// remote identity fallback (ADR-088 Amendment 2): userinfo (`user[:pass]@`
/// for scheme URLs and `user@` for SCP shorthand), the query string, and the
/// fragment are stripped. The in-memory canonical URL used for cloning/gh
/// operations is never replaced with this value.
pub(crate) fn redact_repo_url(url: &str) -> String {
    // `remote_url_to_slug` accepts boundary whitespace around recognized
    // remote spellings. Redaction must recognize exactly those same
    // spellings or a leading space can make normalization succeed while a
    // credential-bearing raw value is written back unchanged. Do not trim
    // arbitrary values: a local path is display metadata and whitespace can
    // be part of the filesystem spelling.
    let candidate = url.trim();
    let recognized_remote = ["https://", "http://", "git://", "ssh://"]
        .iter()
        .any(|scheme| candidate.starts_with(scheme))
        || scp_parts(candidate).is_some();
    if !recognized_remote {
        return url.to_string();
    }
    let stripped = strip_query_and_fragment(candidate);
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
    if let Some((host, path)) = scp_parts(stripped) {
        return format!("{host}:{path}");
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
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&path)
            .args(["remote", "get-url", "origin"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let out = khive_runtime::process_retry::spawn_retrying_executable_busy(
            &khive_runtime::process_retry::EXECUTABLE_BUSY_BACKOFF_MS,
            || command.spawn(),
        )
        .and_then(|child| child.wait_with_output())
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
/// - `Remote`: the `host/owner/repo` slug of the canonical URL, or its shared
///   credential-redacted, query-free, slash/`.git`-normalized identity
///   fallback when an accepted HTTPS source does not satisfy the slug grammar.
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
            // credential-redacted and stripped of query/fragment plus
            // trailing slash/`.git` decorations, so secrets cannot persist
            // in `repo_slug` and equivalent spellings converge.
            remote_url_to_slug(canonical).unwrap_or_else(|| canonical_remote_identity(canonical))
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
#[path = "source_tests.rs"]
mod tests;

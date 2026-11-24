//! `git.check_log` (ADR-182 Amendment 18): the log of one completed job of the platform's own
//! runner.
//!
//! The verb reads the platform, stores the log as one blob and reports. It writes no receipt and
//! changes nothing on the platform or in the repository. Every request is a `GET`.

use std::borrow::Cow;

use khive_runtime::{NamespaceToken, RuntimeError, VerbRegistry};
use serde_json::{json, Value};

use super::pr_state::{clean, hidden, refuse};
use super::{endpoint, observed_string};
use crate::local_handlers::{required, validate_keys, Failure};
use crate::remote_transport::{redact_bytes, Download};
use crate::{credentials, GitPack};

/// The bytes kept from a log's end when `KHIVE_GIT_CHECK_LOG_MAX_BYTES` is unset.
const DEFAULT_LOG_BYTES: u64 = 8 * 1024 * 1024;
/// The log's last lines returned inline, bounded by both counts.
const TAIL_LINES: usize = 200;
const TAIL_BYTES: usize = 16 * 1024;
/// The app whose check runs are jobs of the platform's own runner, the only ones with a log.
const RUNNER_APP: &str = "github-actions";

/// The configured bound on the bytes kept, held within the blob store's whole-object bound.
fn log_limit() -> u64 {
    khive_runtime::env_parse_or("KHIVE_GIT_CHECK_LOG_MAX_BYTES", DEFAULT_LOG_BYTES)
        .clamp(1, khive_storage::MAX_BLOB_WHOLE_BYTES)
}

/// Log bytes as text a terminal or a model can be shown. Bytes that are not UTF-8 are replaced,
/// terminal escape sequences and the hidden characters `git.pr_state` removes are removed, and
/// line breaks and tabs are kept. A leading byte order mark and a carriage return before a line
/// break are dropped without counting as a change; the flag says whether anything else changed.
pub(super) fn clean_log(bytes: &[u8]) -> (String, bool) {
    let bytes = bytes.strip_prefix("\u{FEFF}".as_bytes()).unwrap_or(bytes);
    let text = String::from_utf8_lossy(bytes);
    let mut altered = matches!(text, Cow::Owned(_));
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' | '\t' => out.push(c),
            '\r' if chars.peek() == Some(&'\n') => {}
            '\u{1B}' => {
                altered = true;
                match chars.next() {
                    // A control sequence: parameter and intermediate bytes, then one final byte.
                    Some('[') => {
                        for c in chars.by_ref() {
                            if ('\u{40}'..='\u{7E}').contains(&c) {
                                break;
                            }
                        }
                    }
                    // An operating system command, ended by a bell or by escape and backslash.
                    Some(']') => {
                        while let Some(c) = chars.next() {
                            if c == '\u{07}' {
                                break;
                            }
                            if c == '\u{1B}' {
                                if chars.peek() == Some(&'\\') {
                                    chars.next();
                                }
                                break;
                            }
                        }
                    }
                    // An escape with intermediate bytes, such as a character set choice, ends at
                    // its first final byte.
                    Some(c) if ('\u{20}'..='\u{2F}').contains(&c) => {
                        for c in chars.by_ref() {
                            if ('\u{30}'..='\u{7E}').contains(&c) {
                                break;
                            }
                        }
                    }
                    // Any other escape is two characters long.
                    _ => {}
                }
            }
            c if hidden(c) => altered = true,
            c => out.push(c),
        }
    }
    (out, altered)
}

/// The text's last `TAIL_LINES` lines, cut further to its last `TAIL_BYTES` bytes at a character
/// boundary. A final line break does not start a line of its own.
pub(super) fn tail(text: &str) -> &str {
    let body = text.strip_suffix('\n').unwrap_or(text);
    let mut start = 0;
    for (seen, (at, _)) in body.rmatch_indices('\n').enumerate() {
        if seen + 1 == TAIL_LINES {
            start = at + 1;
            break;
        }
    }
    let mut start = start.max(text.len().saturating_sub(TAIL_BYTES));
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// Platform text that may span lines, cleaned as a log is and bounded in characters.
fn clean_lines(value: Option<&Value>, limit: usize, altered: &mut bool) -> Value {
    let Some(text) = value.and_then(Value::as_str) else {
        return Value::Null;
    };
    let (cleaned, changed) = clean_log(text.as_bytes());
    let kept: String = cleaned.chars().take(limit).collect();
    if changed || kept.len() != cleaned.len() {
        *altered = true;
    }
    Value::String(kept)
}

/// What the platform answered for one check run: the run, and its log read by its end.
struct CheckLog {
    run: Value,
    download: Download,
}

impl GitPack {
    #[cfg(not(unix))]
    pub(crate) async fn handle_check_log(
        &self,
        _token: &NamespaceToken,
        _registry: &VerbRegistry,
        _params: Value,
    ) -> Result<Value, RuntimeError> {
        Err(refuse(Failure::refused("actor_unmapped")))
    }

    /// Order: arguments, allowlist match, policy decision, repository mapping, the blob store,
    /// credential, platform reads, then the one write: the log into the blob store. A local
    /// mapping refuses `remote_scheme` before any credential is resolved, as `git.pr_state` does.
    #[cfg(unix)]
    pub(crate) async fn handle_check_log(
        &self,
        token: &NamespaceToken,
        registry: &VerbRegistry,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let params = crate::params::parse("git.check_log", params)?;
        let invalid = || RuntimeError::InvalidInput("invalid_params".into());
        validate_keys(&params, &["repo", "check_run"]).map_err(|_| invalid())?;
        let repo = required(&params, "repo").map_err(|_| invalid())?;
        let id = params
            .get("check_run")
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
            .ok_or_else(invalid)?;
        let (canonical, index) = self
            .read_gate(token, registry, "git.check_log", repo)
            .await?;
        let target = self.remote_repository(&canonical).map_err(refuse)?;
        if target.slug.is_empty() {
            return Err(refuse(Failure::refused("remote_scheme")));
        }
        // gh's GitHub endpoint must name the same repository as configured Git.
        if target.remote.trim_end_matches(".git") != format!("https://github.com/{}", target.slug) {
            return Err(refuse(Failure::refused("repository_identity")));
        }
        // Four platform requests are not spent on a log there is nowhere to keep.
        let store = self.runtime().require_blob_store()?;
        let config = &self.runtime().config().git_write;
        let (_, secret) = credentials::resolve_remote(config, &token.actor().label())
            .await
            .map_err(|_| refuse(Failure::refused("actor_unmapped")))?;
        let read = self
            .read_check_log(secret.value(), &target.slug, &target.visibility, id)
            .await
            .map_err(refuse)?;

        // The bytes the platform's window held are counted before the credential is replaced in
        // them, so `complete` compares like with like. A window that begins inside an occurrence
        // has that part replaced too.
        let kept = read.download.kept.len() as u64;
        let raw = redact_bytes(
            read.download.kept,
            secret.value(),
            read.download.total > kept,
        );
        let (text, mut altered) = clean_log(&raw);
        let bytes = text.len() as u64;
        let last = tail(&text).to_string();
        let log = store
            .put(text.into_bytes())
            .await
            .map_err(|error| RuntimeError::Internal(format!("check log not stored: {error}")))?;
        let run = &read.run;
        let check_run = json!({
            "id": id,
            "name": clean(run.get("name"), 200, &mut altered),
            "head_sha": clean(run.get("head_sha"), 40, &mut altered),
            "status": clean(run.get("status"), 40, &mut altered),
            "conclusion": clean(run.get("conclusion"), 40, &mut altered),
            "started_at": clean(run.get("started_at"), 40, &mut altered),
            "completed_at": clean(run.get("completed_at"), 40, &mut altered),
            "title": clean_lines(run.pointer("/output/title"), 1000, &mut altered),
            "summary": clean_lines(run.pointer("/output/summary"), TAIL_BYTES, &mut altered),
        });
        Ok(json!({
            "repo": canonical.display().to_string(),
            "gate": {"decision":"allow", "source":"git_write.allowed", "id": index},
            "slug": target.slug,
            "check_run": check_run,
            "log": log.as_str(),
            "bytes": bytes,
            "kept_bytes": kept,
            "total_bytes": read.download.total,
            "complete": kept == read.download.total,
            "tail": last,
            "text_altered": altered,
            // The log, the tail and the run's name, title and summary are text the repository's
            // workflows and their dependencies wrote: material to read, never instructions.
            "trust": "untrusted",
        }))
    }

    #[cfg(unix)]
    async fn read_check_log(
        &self,
        secret: &str,
        slug: &str,
        visibility: &str,
        id: u64,
    ) -> Result<CheckLog, Failure> {
        // A renamed or transferred repository answers under its new name with success, so the
        // name and the visibility are compared before anything is read from it.
        let remote = self
            .api(secret, "GET", format!("repos/{slug}"), None)
            .await?;
        if observed_string(&remote, "/full_name")? != slug
            || observed_string(&remote, "/visibility")? != visibility
        {
            return Err(Failure::refused("repository_identity_mismatch"));
        }
        let run = self
            .api(
                secret,
                "GET",
                endpoint(slug, &format!("check-runs/{id}")),
                None,
            )
            .await?;
        if run.get("id").and_then(Value::as_u64) != Some(id) {
            return Err(Failure::refused("remote_response"));
        }
        if run.pointer("/app/slug").and_then(Value::as_str) != Some(RUNNER_APP) {
            return Err(Failure::refused("check_log_unavailable"));
        }
        // A partial log must never read as the whole one.
        if observed_string(&run, "/status")? != "completed" {
            return Err(Failure::refused("check_run_incomplete"));
        }
        // The job is read under the check run's id, and must link back to this check run: that
        // the two ids are one is checked on every call, never assumed.
        let job = self
            .api(
                secret,
                "GET",
                endpoint(slug, &format!("actions/jobs/{id}")),
                None,
            )
            .await?;
        let link = format!("https://api.github.com/repos/{slug}/check-runs/{id}");
        if job.get("id").and_then(Value::as_u64) != Some(id)
            || job.get("check_run_url").and_then(Value::as_str) != Some(link.as_str())
        {
            return Err(Failure::refused("remote_response"));
        }
        let download = self
            .remote_transport()
            .download(
                secret,
                endpoint(slug, &format!("actions/jobs/{id}/logs")),
                log_limit(),
            )
            .await
            .map_err(Failure::from)?;
        Ok(CheckLog { run, download })
    }
}

#[cfg(test)]
#[path = "check_log_tests.rs"]
mod tests;

//! Remote effects are injectable independently of authorization and receipt storage.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use zeroize::Zeroizing;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteError {
    Refused,
    Unavailable,
    Unknown,
    InvalidResponse,
}

#[derive(Debug, Clone)]
pub struct ApiRequest {
    pub method: &'static str,
    pub path: String,
    pub body: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct PushRequest {
    pub repo: PathBuf,
    pub remote: String,
    pub branch: String,
    pub expected_local: String,
    pub expected_remote: Option<String>,
}

#[async_trait]
pub trait RemoteTransport: Send + Sync {
    async fn api(&self, token: &str, request: ApiRequest) -> Result<Value, RemoteError>;
    async fn remote_ref(
        &self,
        token: &str,
        remote: &str,
        branch: &str,
    ) -> Result<Option<String>, RemoteError>;
    async fn push(&self, token: &str, request: PushRequest) -> Result<(), RemoteError>;
}

pub struct GhTransport;

fn isolated(program: &str) -> Command {
    let mut command = Command::new(program);
    command.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

fn git_command(repo: &Path, token: Option<&str>) -> Command {
    let mut command = isolated("git");
    for setting in [
        "core.hooksPath=/dev/null",
        "core.fsmonitor=false",
        "commit.gpgsign=false",
        "credential.helper=",
        "core.sshCommand=/usr/bin/false",
        "protocol.allow=never",
        "protocol.https.allow=always",
        "http.sslVerify=true",
        "http.followRedirects=false",
        "http.extraHeader=",
        "push.followTags=false",
        "push.recurseSubmodules=no",
    ] {
        command.args(["-c", setting]);
    }
    if let Some(token) = token {
        let clear = Zeroizing::new(format!("x-access-token:{token}"));
        let encoded =
            Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(clear.as_bytes()));
        let header = Zeroizing::new(format!("Authorization: Basic {}", encoded.as_str()));
        command
            .env("KHIVE_REMOTE_AUTH_HEADER", header.as_str())
            .arg("--config-env=http.extraHeader=KHIVE_REMOTE_AUTH_HEADER");
    }
    command.arg("-C").arg(repo);
    command
}

async fn bounded(mut pipe: impl AsyncRead + Unpin, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    (&mut pipe).take(limit + 1).read_to_end(&mut bytes).await?;
    if bytes.len() as u64 > limit {
        return Err(std::io::Error::other("remote output exceeded limit"));
    }
    Ok(bytes)
}

async fn run(
    mut command: Command,
    input: Option<Vec<u8>>,
    effect: bool,
) -> Result<(bool, Vec<u8>), RemoteError> {
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn().map_err(|_| RemoteError::Unavailable)?;
    let stdout = child.stdout.take().ok_or(RemoteError::Unavailable)?;
    let stderr = child.stderr.take().ok_or(RemoteError::Unavailable)?;
    let failure = if effect {
        RemoteError::Unknown
    } else {
        RemoteError::Unavailable
    };
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        if let Some(input) = input {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| std::io::Error::other("missing stdin"))?;
            stdin.write_all(&input).await?;
        }
        let (stdout, _stderr, status) = tokio::try_join!(
            bounded(stdout, 8 * 1024 * 1024),
            bounded(stderr, 64 * 1024),
            child.wait()
        )?;
        Ok::<_, std::io::Error>((status.success(), stdout))
    })
    .await
    .map_err(|_| failure)?
    .map_err(|_| failure)?;
    Ok(result)
}

async fn bare() -> Result<tempfile::TempDir, RemoteError> {
    let dir = tempfile::tempdir().map_err(|_| RemoteError::Unavailable)?;
    let mut command = git_command(dir.path(), None);
    command.args(["init", "--bare", "--quiet", "--template="]);
    if !run(command, None, false).await?.0 {
        return Err(RemoteError::Unavailable);
    }
    Ok(dir)
}

fn valid_oid(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

#[async_trait]
impl RemoteTransport for GhTransport {
    async fn api(&self, token: &str, request: ApiRequest) -> Result<Value, RemoteError> {
        let dir = tempfile::tempdir().map_err(|_| RemoteError::Unavailable)?;
        let effect = request.method != "GET";
        let mut command = isolated("gh");
        command
            .current_dir(dir.path())
            .env("HOME", dir.path())
            .env("GH_CONFIG_DIR", dir.path())
            .env("GH_TOKEN", token)
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .args([
                "api",
                "--hostname",
                "github.com",
                "--include",
                "--method",
                request.method,
                "-H",
                "Accept: application/vnd.github+json",
                "-H",
                "X-GitHub-Api-Version: 2022-11-28",
            ]);
        let input = request.body.map(|value| value.to_string().into_bytes());
        if input.is_some() {
            command.args(["--input", "-"]);
        }
        command.arg(request.path);
        let (success, bytes) = run(command, input, effect).await?;
        parse_api_response(success, bytes, token, effect)
    }

    async fn remote_ref(
        &self,
        token: &str,
        remote: &str,
        branch: &str,
    ) -> Result<Option<String>, RemoteError> {
        let scratch = bare().await?;
        let reference = format!("refs/heads/{branch}");
        let mut command = git_command(scratch.path(), Some(token));
        command.args(["ls-remote", "--refs", "--", remote, &reference]);
        let (success, bytes) = run(command, None, false).await?;
        if !success {
            return Err(RemoteError::Unavailable);
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| RemoteError::InvalidResponse)?;
        if text.is_empty() {
            return Ok(None);
        }
        let mut rows = text.lines();
        let (sha, actual_ref) = rows
            .next()
            .and_then(|row| row.split_once('\t'))
            .ok_or(RemoteError::InvalidResponse)?;
        if actual_ref != reference || !valid_oid(sha) || rows.next().is_some() {
            return Err(RemoteError::InvalidResponse);
        }
        Ok(Some(sha.to_ascii_lowercase()))
    }

    async fn push(&self, token: &str, request: PushRequest) -> Result<(), RemoteError> {
        push_native(token, request, false).await
    }
}

pub(crate) async fn push_native(
    token: &str,
    request: PushRequest,
    allow_file: bool,
) -> Result<(), RemoteError> {
    let scratch = bare().await?;
    let objects = crate::local_git::object_directory(&request.repo)
        .await
        .map_err(|_| RemoteError::Unavailable)?;
    let reference = format!("refs/heads/{}", request.branch);
    let refspec = format!("{}:{reference}", request.expected_local);
    let lease = format!(
        "--force-with-lease={reference}:{}",
        request.expected_remote.as_deref().unwrap_or("")
    );
    let mut command = git_command(scratch.path(), Some(token));
    if allow_file {
        command.args(["-c", "protocol.file.allow=always"]);
    }
    let alternate = serde_json::to_string(&objects.display().to_string())
        .map_err(|_| RemoteError::Unavailable)?;
    command
        .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", alternate)
        .args([
            "push",
            "--porcelain",
            "--no-verify",
            "--no-follow-tags",
            "--recurse-submodules=no",
            &lease,
            "--",
            &request.remote,
            &refspec,
        ]);
    let (success, bytes) = run(command, None, true).await?;
    let text = std::str::from_utf8(&bytes).map_err(|_| RemoteError::Unknown)?;
    let status = text.lines().find_map(|line| {
        let mut parts = line.split('\t');
        let flag = parts.next()?;
        let pair = parts.next()?;
        let (_source, target) = pair.split_once(':')?;
        (target == reference).then_some(flag)
    });
    match (success, status) {
        (true, Some(" " | "*")) => Ok(()),
        (_, Some("!")) => Err(RemoteError::Refused),
        // '=' is a no-op, not evidence that this operation moved the ref.
        (true, Some("=")) => Err(RemoteError::Refused),
        _ => Err(RemoteError::Unknown),
    }
}

fn parse_api_response(
    success: bool,
    bytes: Vec<u8>,
    token: &str,
    effect: bool,
) -> Result<Value, RemoteError> {
    let raw = Zeroizing::new(bytes);
    let text = std::str::from_utf8(&raw).map_err(|_| {
        if effect {
            RemoteError::Unknown
        } else {
            RemoteError::InvalidResponse
        }
    })?;
    let (headers, body) = text
        .split_once("\r\n\r\n")
        .or_else(|| text.split_once("\n\n"))
        .ok_or(if effect {
            RemoteError::Unknown
        } else {
            RemoteError::InvalidResponse
        })?;
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok());
    if !success || !status.is_some_and(|status| (200..300).contains(&status)) {
        // Only explicit platform refusals prove there was no effect. Server or
        // transport failures can follow a committed write.
        return Err(
            if matches!(status, Some(400 | 401 | 403 | 404 | 405 | 409 | 422)) {
                RemoteError::Refused
            } else if effect {
                RemoteError::Unknown
            } else {
                RemoteError::Unavailable
            },
        );
    }
    // No native diagnostics are exposed, and echoed token material cannot
    // enter typed results or receipts even when a platform response is hostile.
    let value = serde_json::from_str(body).map_err(|_| {
        if effect {
            RemoteError::Unknown
        } else {
            RemoteError::InvalidResponse
        }
    })?;
    Ok(redact(value, token))
}

fn redact(value: Value, token: &str) -> Value {
    match value {
        Value::String(s) => Value::String(s.replace(token, "[redacted]")),
        Value::Array(a) => Value::Array(a.into_iter().map(|v| redact(v, token)).collect()),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .map(|(k, v)| (k.replace(token, "[redacted]"), redact(v, token)))
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn api_redacts_json_escaped_credentials_in_keys_and_values() {
        let response = b"HTTP/2.0 200 OK\r\n\r\n{\"s\\u0065cret\": [\"prefix secret suffix\", {\"nested\": \"s\\u0065cret\"}]}";
        let value = parse_api_response(true, response.to_vec(), "secret", true).unwrap();
        assert_eq!(
            value,
            json!({"[redacted]": ["prefix [redacted] suffix", {"nested":"[redacted]"}]})
        );
        assert!(!value.to_string().contains("secret"));
    }

    #[test]
    fn api_ambiguous_effect_replies_are_unknown_and_explicit_refusals_are_not() {
        for response in [
            b"HTTP/2.0 503 Unavailable\n\n{}".as_slice(),
            b"HTTP/2.0 200 OK\n\nnot-json",
            b"no response headers",
        ] {
            assert_eq!(
                parse_api_response(true, response.to_vec(), "secret", true),
                Err(RemoteError::Unknown)
            );
        }
        assert_eq!(
            parse_api_response(
                false,
                b"HTTP/2.0 409 Conflict\n\n{}".to_vec(),
                "secret",
                true
            ),
            Err(RemoteError::Refused)
        );
        assert_eq!(
            parse_api_response(false, b"HTTP/2.0 200 OK\n\n{}".to_vec(), "secret", true),
            Err(RemoteError::Unknown)
        );
        assert_eq!(
            parse_api_response(
                true,
                b"HTTP/2.0 200 OK\n\nnot-json".to_vec(),
                "secret",
                false
            ),
            Err(RemoteError::InvalidResponse)
        );
    }
}

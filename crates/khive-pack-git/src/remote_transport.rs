//! Remote effects are injectable independently of authorization and receipt storage.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use khive_runtime::bounded_read::read_to_end_bounded_async;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWriteExt};
use tokio::process::Command;
use zeroize::Zeroizing;

use crate::git_env;

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
    /// Read the platform's aggregate review decision and the head it describes.
    async fn review_decision(
        &self,
        token: &str,
        slug: &str,
        number: u64,
    ) -> Result<Value, RemoteError>;
    /// None is the explicitly local, credential-free Git transport; HTTPS uses Some.
    async fn remote_ref(
        &self,
        token: Option<&str>,
        remote: &str,
        branch: &str,
    ) -> Result<Option<String>, RemoteError>;
    async fn push(&self, token: Option<&str>, request: PushRequest) -> Result<(), RemoteError>;
}

pub struct GhTransport {
    program: PathBuf,
}

fn isolated(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    git_env::apply_shared_env(command.as_std_mut());
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

/// Settings only the remote transport carries, passed after `git_env::SHARED_SETTINGS`.
const TRANSPORT_SETTINGS: &[&str] = &[
    "protocol.https.allow=always",
    "http.sslVerify=true",
    "http.followRedirects=false",
    "http.extraHeader=",
    "push.followTags=false",
    "push.recurseSubmodules=no",
];

fn git_command(program: &Path, repo: &Path, token: Option<&str>) -> Command {
    let mut command = isolated(program.as_os_str());
    for setting in git_env::SHARED_SETTINGS.iter().chain(TRANSPORT_SETTINGS) {
        command.arg("-c").arg(setting);
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

async fn bounded(pipe: impl AsyncRead + Unpin, limit: u64) -> std::io::Result<Vec<u8>> {
    read_to_end_bounded_async(pipe, limit)
        .await?
        .ok_or_else(|| std::io::Error::other("remote output exceeded limit"))
}

async fn run(
    mut command: Command,
    input: Option<Vec<u8>>,
    effect: bool,
) -> Result<(bool, Vec<u8>), RemoteError> {
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = khive_runtime::process_retry::spawn_retrying_executable_busy_async(
        &khive_runtime::process_retry::EXECUTABLE_BUSY_BACKOFF_MS,
        || command.spawn(),
    )
    .await
    .map_err(|_| RemoteError::Unavailable)?;
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

async fn bare(program: &Path) -> Result<tempfile::TempDir, RemoteError> {
    let dir = tempfile::tempdir().map_err(|_| RemoteError::Unavailable)?;
    let mut command = git_command(program, dir.path(), None);
    command.args(["init", "--bare", "--quiet", "--template="]);
    if !run(command, None, false).await?.0 {
        return Err(RemoteError::Unavailable);
    }
    Ok(dir)
}

impl GhTransport {
    pub fn new(program: PathBuf) -> Self {
        Self { program }
    }

    async fn call_api(
        &self,
        token: &str,
        request: ApiRequest,
        effect: bool,
    ) -> Result<Value, RemoteError> {
        let dir = tempfile::tempdir().map_err(|_| RemoteError::Unavailable)?;
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
}

#[async_trait]
impl RemoteTransport for GhTransport {
    async fn api(&self, token: &str, request: ApiRequest) -> Result<Value, RemoteError> {
        let effect = request.method != "GET";
        self.call_api(token, request, effect).await
    }

    async fn review_decision(
        &self,
        token: &str,
        slug: &str,
        number: u64,
    ) -> Result<Value, RemoteError> {
        let (owner, name) = slug.split_once('/').ok_or(RemoteError::InvalidResponse)?;
        let number = i32::try_from(number).map_err(|_| RemoteError::InvalidResponse)?;
        let response = self.call_api(token, ApiRequest {
            method: "POST",
            path: "graphql".into(),
            body: Some(json!({
                "query": "query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { pullRequest(number: $number) { headRefOid reviewDecision } } }",
                "variables": {"owner":owner, "name":name, "number":number}
            })),
        }, false).await?;
        // GraphQL can return HTTP 200 with partial data and errors. Neither that
        // nor a missing/inaccessible PR is an affirmative review decision.
        if response.get("errors").is_some_and(|errors| {
            !errors.is_null() && errors.as_array().is_none_or(|errors| !errors.is_empty())
        }) {
            return Err(RemoteError::InvalidResponse);
        }
        response
            .pointer("/data/repository/pullRequest")
            .filter(|pr| pr.is_object())
            .cloned()
            .ok_or(RemoteError::InvalidResponse)
    }

    /// None is the explicitly local, credential-free Git transport; HTTPS uses Some.
    async fn remote_ref(
        &self,
        token: Option<&str>,
        remote: &str,
        branch: &str,
    ) -> Result<Option<String>, RemoteError> {
        let scratch = bare(&self.program).await?;
        let reference = format!("refs/heads/{branch}");
        let mut command = git_command(&self.program, scratch.path(), token);
        if token.is_none() {
            command.args([
                "-c",
                "protocol.file.allow=always",
                "-c",
                "protocol.https.allow=never",
            ]);
        }
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
        if actual_ref != reference
            || !crate::object_id::is_40_hex(sha.as_bytes())
            || rows.next().is_some()
        {
            return Err(RemoteError::InvalidResponse);
        }
        Ok(Some(sha.to_ascii_lowercase()))
    }

    async fn push(&self, token: Option<&str>, request: PushRequest) -> Result<(), RemoteError> {
        push_native(&self.program, token, request, token.is_none()).await
    }
}

pub(crate) async fn push_native(
    program: &Path,
    token: Option<&str>,
    request: PushRequest,
    allow_file: bool,
) -> Result<(), RemoteError> {
    let scratch = bare(program).await?;
    let objects = crate::local_git::object_directory(program, &request.repo)
        .await
        .map_err(|_| RemoteError::Unavailable)?;
    let reference = format!("refs/heads/{}", request.branch);
    let refspec = format!("{}:{reference}", request.expected_local);
    let lease = format!(
        "--force-with-lease={reference}:{}",
        request.expected_remote.as_deref().unwrap_or("")
    );
    let mut command = git_command(program, scratch.path(), token);
    let receive_pack;
    if allow_file {
        command.args([
            "-c",
            "protocol.file.allow=always",
            "-c",
            "protocol.https.allow=never",
        ]);
    }
    let alternate = serde_json::to_string(&objects.display().to_string())
        .map_err(|_| RemoteError::Unavailable)?;
    let mut args = vec![
        "push",
        "--porcelain",
        "--no-verify",
        "--no-follow-tags",
        "--recurse-submodules=no",
    ];
    if allow_file {
        // Local transport runs the destination's receive-pack as this process, and the
        // client-side hooks path above does not reach it: the destination's own hooks would
        // run. The receive-pack invocation carries its own hooks path instead.
        let program = program.to_str().ok_or(RemoteError::Unavailable)?;
        receive_pack = format!(
            "--receive-pack='{}' -c core.hooksPath=/dev/null receive-pack",
            program.replace('\'', "'\\''")
        );
        args.push(&receive_pack);
    }
    args.extend([
        lease.as_str(),
        "--",
        request.remote.as_str(),
        refspec.as_str(),
    ]);
    command
        .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", alternate)
        .args(args);
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
#[path = "remote_transport_tests.rs"]
mod tests;

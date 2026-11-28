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
    /// The platform refused because the credential's request allowance is spent. `reset` is
    /// the epoch second the platform says the allowance returns, when it said one.
    RateLimited {
        reset: Option<u64>,
    },
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

#[derive(Debug, Clone)]
pub struct FetchRequest {
    /// The canonical clone whose object store receives the fetched objects.
    pub repo: PathBuf,
    /// The mapped remote URL. A fetch never names a remote by name.
    pub remote: String,
    /// The remote ref read: `refs/heads/<branch>` or `refs/pull/<N>/head`.
    pub source: String,
    /// The clone's own tips, offered as haves so only objects the clone lacks are sent.
    pub haves: Vec<String>,
    /// The commit the caller expects the remote ref to name, lowercase. When the ref names another
    /// commit, nothing the transfer brought is kept.
    pub expected: Option<String>,
}

/// A text body read by its end (ADR-182 Amendment 18).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    /// The body's last bytes, at most the limit the read was given.
    pub kept: Vec<u8>,
    /// Every body byte that arrived, kept or not.
    pub total: u64,
}

/// What a fetch found at the remote, when the transfer itself completed or was refused there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    /// The commit the remote ref names; its objects are now in the clone's store.
    Commit(String),
    /// The remote ref names this commit and not the expected one. Nothing the transfer brought is
    /// in the clone's store.
    Mismatch(String),
    /// The remote holds no such ref.
    Absent,
    /// An object the remote sent failed the object check, and the transfer was refused.
    ObjectsRefused,
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
    /// Fetch `request.source` into the clone's object store. No ref of the clone is written.
    async fn fetch(
        &self,
        token: Option<&str>,
        request: FetchRequest,
    ) -> Result<Fetched, RemoteError>;
    /// `GET` a platform path the platform answers with a redirect to a text download, keeping the
    /// body's last `limit` bytes as the download location sent them: the caller removes the
    /// credential from them. The credential goes to the platform's API host only, never to the
    /// download location. A transport without this read refuses it as unavailable.
    async fn download(
        &self,
        token: &str,
        path: String,
        limit: u64,
    ) -> Result<Download, RemoteError> {
        let _ = (token, path, limit);
        Err(RemoteError::Unavailable)
    }
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

/// Header bytes accepted before a response body begins.
const HEADER_LIMIT: usize = 64 * 1024;
/// How long one download may take, end to end.
const DOWNLOAD_SECONDS: u64 = 300;

/// Where a response's header block ends and its body begins, at the first blank line.
fn header_end(bytes: &[u8]) -> Option<(usize, usize)> {
    let crlf = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|at| (at, at + 4));
    let lf = bytes
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|at| (at, at + 2));
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

/// Read a response printed as a header block, a blank line and a body. The header block is kept
/// whole up to `HEADER_LIMIT`; of the body only the last `limit` bytes are kept, and every body
/// byte is counted. Returns the header block, the kept bytes and the body's full length.
async fn read_by_end(
    mut pipe: impl AsyncRead + Unpin,
    limit: u64,
) -> std::io::Result<(Vec<u8>, Vec<u8>, u64)> {
    use tokio::io::AsyncReadExt;
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let mut head = Vec::new();
    let mut body = Vec::new();
    let mut in_body = false;
    let mut total = 0_u64;
    let mut chunk = vec![0_u8; 64 * 1024];
    loop {
        let n = pipe.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        if in_body {
            total += n as u64;
            body.extend_from_slice(&chunk[..n]);
        } else {
            head.extend_from_slice(&chunk[..n]);
            if let Some((end, start)) = header_end(&head) {
                body = head.split_off(start);
                head.truncate(end);
                total = body.len() as u64;
                in_body = true;
            } else if head.len() > HEADER_LIMIT {
                return Err(std::io::Error::other("response header exceeded limit"));
            }
        }
        // Dropping the front only once the buffer is twice the limit keeps the copying linear.
        if body.len() >= limit.saturating_mul(2).max(limit.saturating_add(64 * 1024)) {
            body.drain(..body.len() - limit);
        }
    }
    if body.len() > limit {
        body.drain(..body.len() - limit);
    }
    Ok((head, body, total))
}

/// The credential's own value replaced wherever the kept bytes hold it. When the kept window
/// begins inside an occurrence, the part of it the window holds is replaced too.
pub(crate) fn redact_bytes(kept: Vec<u8>, token: &str, cut: bool) -> Vec<u8> {
    const MARK: &[u8] = b"[redacted]";
    let token = token.as_bytes();
    if token.is_empty() {
        return kept;
    }
    let mut start = 0;
    if cut {
        // At least four bytes, so a chance match of a short ending is not taken for one.
        for k in (4..token.len()).rev() {
            if kept.starts_with(&token[token.len() - k..]) {
                start = k;
                break;
            }
        }
    }
    let mut out = Vec::with_capacity(kept.len());
    if start > 0 {
        out.extend_from_slice(MARK);
    }
    let mut at = start;
    while at < kept.len() {
        if kept[at] == token[0] && kept[at..].starts_with(token) {
            out.extend_from_slice(MARK);
            at += token.len();
        } else {
            out.push(kept[at]);
            at += 1;
        }
    }
    out
}

/// The platform's API origin. Only a request to it carries the credential.
const API_ORIGIN: &str = "https://api.github.com/";

/// The status code on the first line of a printed response.
fn status_code(headers: &str) -> Option<u16> {
    headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
}

/// A response's first header of this name, matched without regard to case.
fn header_value<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// A response that is neither the expected success nor a redirect, read as
/// `parse_api_response` reads one: a spent allowance names itself, a refusal is the platform's,
/// and anything else is unavailable.
fn status_failure(status: Option<u16>, headers: &str) -> RemoteError {
    if matches!(status, Some(403 | 429)) {
        if let Some(reset) = spent_allowance(headers) {
            return RemoteError::RateLimited { reset };
        }
    }
    // A log the platform no longer keeps answers 410.
    if matches!(status, Some(400 | 401 | 403 | 404 | 410 | 422)) {
        RemoteError::Refused
    } else {
        RemoteError::Unavailable
    }
}

/// The downloaded log's response; a body is returned only under a 2xx status.
fn parse_download(
    success: bool,
    headers: &[u8],
    kept: Vec<u8>,
    total: u64,
) -> Result<Download, RemoteError> {
    let headers = std::str::from_utf8(headers).map_err(|_| RemoteError::InvalidResponse)?;
    let status = status_code(headers);
    if !success || !status.is_some_and(|status| (200..300).contains(&status)) {
        return Err(status_failure(status, headers));
    }
    Ok(Download { kept, total })
}

/// Text that can stand inside a quoted curl configuration value as it is: printable ASCII with
/// no space, quote or backslash.
fn plain(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\')
}

/// The location the platform's reply to the log request redirects to. The platform answers that
/// request with a redirect to a short-lived location on another host, and the redirect is never
/// followed by the process that holds the credential.
fn parse_redirect(success: bool, reply: &[u8]) -> Result<String, RemoteError> {
    let end = header_end(reply).map_or(reply.len(), |(end, _)| end);
    let headers = std::str::from_utf8(&reply[..end]).map_err(|_| RemoteError::InvalidResponse)?;
    if !success {
        return Err(RemoteError::Unavailable);
    }
    match status_code(headers) {
        Some(301 | 302 | 303 | 307 | 308) => header_value(headers, "location")
            .filter(|location| location.starts_with("https://") && plain(location))
            .map(str::to_string)
            .ok_or(RemoteError::InvalidResponse),
        Some(status) if (200..300).contains(&status) => Err(RemoteError::InvalidResponse),
        status => Err(status_failure(status, headers)),
    }
}

/// A curl that reads no configuration file of the user's, speaks only HTTPS, follows no
/// redirect, prints the response's headers before its body, and takes its URL and headers from
/// its standard input, so neither the credential nor a signed location is in its arguments.
fn curl() -> Command {
    let mut command = isolated("curl");
    command
        .args([
            "-q",
            "--silent",
            "--show-error",
            "--include",
            "--proto",
            "=https",
            "--max-redirs",
            "0",
            "--config",
            "-",
        ])
        .stdin(Stdio::piped());
    command
}

/// The configuration a `curl()` reads: the URL, and the credential only when one is given.
fn curl_config(url: &str, token: Option<&str>) -> Result<Zeroizing<String>, RemoteError> {
    if !plain(url) || token.is_some_and(|token| !plain(token)) {
        return Err(RemoteError::Unavailable);
    }
    let mut config = Zeroizing::new(format!("url = \"{url}\"\nheader = \"User-Agent: khive\"\n"));
    if let Some(token) = token {
        config.push_str("header = \"X-GitHub-Api-Version: 2022-11-28\"\n");
        config.push_str("header = \"Authorization: Bearer ");
        config.push_str(token);
        config.push_str("\"\n");
    }
    Ok(config)
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

    /// Two processes. The first sends the credential to the platform's API host and reads the
    /// redirect without following it. The second fetches the location that redirect names, and is
    /// never given the credential.
    async fn download(
        &self,
        token: &str,
        path: String,
        limit: u64,
    ) -> Result<Download, RemoteError> {
        let config = curl_config(&format!("{API_ORIGIN}{path}"), Some(token))?;
        let (success, reply) = run(curl(), Some(config.as_bytes().to_vec()), false).await?;
        let location = parse_redirect(success, &reply)?;
        let config = curl_config(&location, None).map_err(|_| RemoteError::InvalidResponse)?;
        let mut command = curl();
        let mut child = khive_runtime::process_retry::spawn_retrying_executable_busy_async(
            &khive_runtime::process_retry::EXECUTABLE_BUSY_BACKOFF_MS,
            || command.spawn(),
        )
        .await
        .map_err(|_| RemoteError::Unavailable)?;
        let mut stdin = child.stdin.take().ok_or(RemoteError::Unavailable)?;
        let stdout = child.stdout.take().ok_or(RemoteError::Unavailable)?;
        let stderr = child.stderr.take().ok_or(RemoteError::Unavailable)?;
        let (success, (headers, kept, total)) =
            tokio::time::timeout(Duration::from_secs(DOWNLOAD_SECONDS), async {
                stdin.write_all(config.as_bytes()).await?;
                drop(stdin);
                let (read, _stderr, status) = tokio::try_join!(
                    read_by_end(stdout, limit),
                    bounded(stderr, 64 * 1024),
                    child.wait()
                )?;
                Ok::<_, std::io::Error>((status.success(), read))
            })
            .await
            .map_err(|_| RemoteError::Unavailable)?
            .map_err(|_| RemoteError::Unavailable)?;
        parse_download(success, &headers, kept, total)
    }

    async fn fetch(
        &self,
        token: Option<&str>,
        request: FetchRequest,
    ) -> Result<Fetched, RemoteError> {
        fetch_native(&self.program, token, request, token.is_none()).await
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

/// A fetch is bounded longer than other remote calls: a first fetch of a large branch transfers
/// its whole history.
const FETCH_TIMEOUT: Duration = Duration::from_secs(300);
/// The only ref a fetch writes, and it lives in the scratch repository, never in the clone.
const FETCH_STAGED: &str = "refs/khive/fetch/staged";
/// Name prefix of the directory a fetch receives objects into. It sits inside the clone's object
/// directory, so moving an object into place stays on one filesystem; git reads only the two-hex
/// directories, `pack` and `info` there, so it never reads this one as part of the store.
const FETCH_INCOMING_PREFIX: &str = "khive-incoming-";
/// Settings a fetch child carries after the shared and transport settings. Objects are checked as
/// they arrive because a pull request's head brings in objects from whoever opened it.
const FETCH_SETTINGS: &[&str] = &[
    "transfer.fsckObjects=true",
    "fetch.prune=false",
    "fetch.pruneTags=false",
    "fetch.recurseSubmodules=false",
    "fetch.writeCommitGraph=false",
    "submodule.recurse=false",
    "gc.auto=0",
    "maintenance.auto=false",
];

/// Run `command` in its own process group, bounded by `limit`. On expiry the whole group is
/// killed: a remote helper or `index-pack` left running would keep writing into the clone's object
/// store after the call has answered. The group is signalled only while its leader is unreaped, so
/// the id cannot name another group. Returns success and the bounded stderr.
async fn run_group(mut command: Command, limit: Duration) -> Result<(bool, Vec<u8>), RemoteError> {
    #[cfg(unix)]
    command.process_group(0);
    let mut child = khive_runtime::process_retry::spawn_retrying_executable_busy_async(
        &khive_runtime::process_retry::EXECUTABLE_BUSY_BACKOFF_MS,
        || command.spawn(),
    )
    .await
    .map_err(|_| RemoteError::Unavailable)?;
    #[cfg(unix)]
    let group = child.id().and_then(|id| i32::try_from(id).ok());
    let stdout = child.stdout.take().ok_or(RemoteError::Unavailable)?;
    let stderr = child.stderr.take().ok_or(RemoteError::Unavailable)?;
    let completed = tokio::time::timeout(limit, async {
        let (_stdout, stderr, status) = tokio::try_join!(
            bounded(stdout, 64 * 1024),
            bounded(stderr, 64 * 1024),
            child.wait()
        )?;
        Ok::<_, std::io::Error>((status.success(), stderr))
    })
    .await;
    match completed {
        Ok(Ok(result)) => Ok(result),
        _ => {
            #[cfg(unix)]
            if let Some(group) = group {
                let _ = khive_runtime::process_group::signal_process_group(group, libc::SIGKILL);
            }
            let _ = child.kill().await;
            Err(RemoteError::Unavailable)
        }
    }
}

/// Fetch one remote ref into the clone's object store from a scratch repository.
///
/// The fetch runs in a fresh bare repository whose configuration the pack writes, so the clone's
/// configuration, hooks and remotes are never read by it: a `url.<base>.insteadOf`, an `http.<url>.*` key or
/// an `include.path` in the clone cannot change where the fetch goes or what it sends. The remote
/// is named by its URL and the refspec is explicit, with `--refmap=` so no configured mapping
/// applies. The clone's tips are created in the scratch first, so they are offered as haves.
/// The only ref written is the scratch's staging ref, dropped with the scratch.
///
/// What the transfer brings lands in a directory of its own inside the clone's object directory.
/// Every git child that touches the scratch names that directory as its object store and the
/// clone's store as an alternate (`GIT_OBJECT_DIRECTORY`, `GIT_ALTERNATE_OBJECT_DIRECTORIES`), so it
/// reads the clone's objects and writes none of them. The objects move into the clone's store only
/// after the transfer completed and the remote ref names the expected commit. A refused, failed or
/// killed transfer, and a mismatch, leave the clone's store as it was: the directory is removed
/// when the call returns.
pub(crate) async fn fetch_native(
    program: &Path,
    token: Option<&str>,
    request: FetchRequest,
    allow_file: bool,
) -> Result<Fetched, RemoteError> {
    let scratch = bare(program).await?;
    let objects = crate::local_git::object_directory(program, &request.repo)
        .await
        .map_err(|_| RemoteError::Unavailable)?;
    let incoming = tempfile::Builder::new()
        .prefix(FETCH_INCOMING_PREFIX)
        .tempdir_in(&objects)
        .map_err(|_| RemoteError::Unavailable)?;
    std::fs::create_dir(incoming.path().join("pack")).map_err(|_| RemoteError::Unavailable)?;
    let alternate = serde_json::to_string(&objects.display().to_string())
        .map_err(|_| RemoteError::Unavailable)?;
    let quarantined = |mut command: Command| {
        command
            .env("GIT_OBJECT_DIRECTORY", incoming.path())
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", &alternate);
        command
    };
    if !request.haves.is_empty() {
        let mut input = String::new();
        for (index, sha) in request.haves.iter().enumerate() {
            if !crate::object_id::is_40_hex(sha.as_bytes()) {
                return Err(RemoteError::Unavailable);
            }
            input.push_str(&format!("create refs/khive/have/{index} {sha}\n"));
        }
        let mut command = quarantined(git_command(program, scratch.path(), None));
        command.args(["update-ref", "--stdin"]);
        if !run(command, Some(input.into_bytes()), false).await?.0 {
            return Err(RemoteError::Unavailable);
        }
    }
    let refspec = format!("{}:{FETCH_STAGED}", request.source);
    let mut command = git_command(program, scratch.path(), token);
    for setting in FETCH_SETTINGS {
        command.arg("-c").arg(setting);
    }
    if allow_file {
        command.args([
            "-c",
            "protocol.file.allow=always",
            "-c",
            "protocol.https.allow=never",
        ]);
    }
    let mut command = quarantined(command);
    // `--no-write-fetch-head` and `--no-auto-maintenance` both need git 2.29 or later, so a git
    // that takes this command line takes the newer spelling; `--no-auto-gc` is its older name.
    command.args([
        "fetch",
        "--quiet",
        "--no-tags",
        "--no-recurse-submodules",
        "--no-write-fetch-head",
        "--no-auto-maintenance",
        "--refmap=",
        "--",
        request.remote.as_str(),
        refspec.as_str(),
    ]);
    let (success, stderr) = run_group(command, FETCH_TIMEOUT).await?;
    if !success {
        // The two failures this verb names; every other one is the transport's.
        let stderr = String::from_utf8_lossy(&stderr);
        if stderr.contains("couldn't find remote ref") {
            return Ok(Fetched::Absent);
        }
        if stderr.contains("fsck error") {
            return Ok(Fetched::ObjectsRefused);
        }
        return Err(RemoteError::Unavailable);
    }
    let staged = format!("{FETCH_STAGED}^{{commit}}");
    let mut command = quarantined(git_command(program, scratch.path(), None));
    command.args(["rev-parse", "--verify", "--end-of-options", staged.as_str()]);
    let (success, bytes) = run(command, None, false).await?;
    if !success {
        return Err(RemoteError::InvalidResponse);
    }
    let sha = std::str::from_utf8(&bytes)
        .map_err(|_| RemoteError::InvalidResponse)?
        .trim();
    if !crate::object_id::is_40_hex(sha.as_bytes()) {
        return Err(RemoteError::InvalidResponse);
    }
    let sha = sha.to_ascii_lowercase();
    if request
        .expected
        .as_deref()
        .is_some_and(|expected| !expected.eq_ignore_ascii_case(&sha))
    {
        return Ok(Fetched::Mismatch(sha));
    }
    let from = incoming.path().to_path_buf();
    tokio::task::spawn_blocking(move || migrate_incoming(&from, &objects))
        .await
        .map_err(|_| RemoteError::Unavailable)?
        .map_err(|_| RemoteError::Unavailable)?;
    Ok(Fetched::Commit(sha))
}

/// Move what a completed transfer received into the clone's object store: loose objects, then
/// packs with each pack's index last, so no reader finds an index before its pack. An object name
/// already present is kept, since the name is the content. Temporary files and `.keep` files stay
/// behind and are removed with the directory.
fn migrate_incoming(incoming: &Path, objects: &Path) -> std::io::Result<()> {
    let hex = |name: &str, len: usize| {
        name.len() == len && name.bytes().all(|byte| byte.is_ascii_hexdigit())
    };
    for entry in std::fs::read_dir(incoming)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !hex(name, 2) || !entry.file_type()?.is_dir() {
            continue;
        }
        let target = objects.join(name);
        std::fs::create_dir_all(&target)?;
        for object in std::fs::read_dir(entry.path())? {
            let object = object?;
            let file = object.file_name();
            if let Some(file) = file.to_str().filter(|file| hex(file, 38)) {
                place(&object.path(), &target.join(file))?;
            }
        }
    }
    let packs = incoming.join("pack");
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&packs)? {
        if let Some(name) = entry?.file_name().to_str() {
            names.push(name.to_string());
        }
    }
    let names = pack_files_in_order(names);
    if !names.is_empty() {
        std::fs::create_dir_all(objects.join("pack"))?;
    }
    for name in names {
        place(&packs.join(&name), &objects.join("pack").join(&name))?;
    }
    Ok(())
}

/// The pack files to move, each index after every other file: an index is what makes a pack
/// visible to a reader.
fn pack_files_in_order(mut names: Vec<String>) -> Vec<String> {
    names.retain(|name| name.starts_with("pack-") && !name.ends_with(".keep"));
    names.sort_by(|a, b| (a.ends_with(".idx"), a).cmp(&(b.ends_with(".idx"), b)));
    names
}

/// Link `from` into the store at `to`. A name already there is the same object and stays.
fn place(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::hard_link(from, to) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
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
        // A spent allowance is refused before the platform acts on the request, and the
        // response says so itself: a caller told only "refused" cannot tell it from a
        // missing permission and has no reason to stop asking.
        if matches!(status, Some(403 | 429)) {
            if let Some(reset) = spent_allowance(headers) {
                return Err(RemoteError::RateLimited { reset });
            }
        }
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

/// `Some(reset)` when the response headers report a remaining allowance of zero.
fn spent_allowance(headers: &str) -> Option<Option<u64>> {
    let field = |name: &str| {
        headers.lines().skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
        })
    };
    (field("x-ratelimit-remaining")? == "0")
        .then(|| field("x-ratelimit-reset").and_then(|at| at.parse().ok()))
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

//! `bridge.share` (ADR-220 Amendment 1): named files under the configured
//! share root, sent to a share target as blobs and one tree, then shared with a
//! receiving actor by that target's `computer.share`.
//!
//! The process that resolved the caller's configuration answers it, as it
//! answers `bridge.diagnostics`: the stdio bridge, and `kkernel exec` through
//! [`answer_bridge_share_for_exec`]. The daemon never sees the verb, a file's
//! bytes or a target's key, and the store records nothing: the target's share
//! record is the record.
#![cfg(unix)]

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use khive_fs::opened_file::{open_regular_file_nofollow, opened_file_path, ContainedOpenError};
use khive_request::{parse_request, ArgValue, ExecutionMode, ParsedOp, ParsedRequest};
use khive_runtime::{ConfigError, DomainDisposition, KhiveConfig};
use rmcp::ErrorData as McpError;
use serde_json::{json, Value};

use super::computer_client::{
    ComputerClient, Connector, ForwardedOptions, HttpConnector, RoutedFailure, COMPUTER_UNREACHABLE,
};
use super::computer_routing::{failed_envelope, render_unit, COMPUTER_PROTOCOL};
use super::{serialize_response_value, validate_request_overrides, KhiveMcpServer};
use crate::tools::request::RequestParams;

/// The verb this module answers.
pub(super) const SHARE_VERB: &str = "bridge.share";
const SHARE_NOT_CONFIGURED: &str = "share_not_configured";
const SHARE_TARGET_UNKNOWN: &str = "share_target_unknown";
const SHARE_PATH_REFUSED: &str = "share_path_refused";
const SHARE_TOO_LARGE: &str = "share_too_large";
const SHARE_FILE_CHANGED: &str = "share_file_changed";
const INVALID_INPUT: &str = "invalid_input";
/// The most bytes `note` may hold.
const MAX_NOTE_BYTES: usize = 512;
/// The mode of every file a share puts in its tree.
const SHARED_FILE_MODE: u32 = 644;
/// The arguments `bridge.share` takes, besides `help`.
const ARGUMENTS: &[&str] = &["target", "receiver", "paths", "note"];

/// The paragraph the `request` tool's description gains when the caller's
/// configuration has a `[share]` table.
pub(super) const SHARE_DESCRIPTION: &str = "Sharing: `bridge.share(target=..., receiver=..., \
paths=[...], note=...)` sends named files under this process's [share] root to a share target as \
one tree and shares it with the receiving actor there. It must be the only operation in the \
request; `bridge.share(help=true)` describes it.";

/// Whether `parsed` names `bridge.share` anywhere.
pub(super) fn parsed_contains_bridge_share(parsed: &ParsedRequest) -> bool {
    parsed.ops.iter().any(|op| op.tool == SHARE_VERB)
}

fn refused(kind: &str, message: String) -> RoutedFailure {
    RoutedFailure {
        error: json!({ "kind": kind, "message": message }),
        disposition: DomainDisposition::NotCommitted,
    }
}

fn invalid(message: &str) -> RoutedFailure {
    refused(INVALID_INPUT, format!("{message}; nothing was sent"))
}

fn path_refused(path: &str, rule: &str) -> RoutedFailure {
    refused(
        SHARE_PATH_REFUSED,
        format!("{path:?} is refused: {rule}; nothing was sent"),
    )
}

/// The share bridge of a process, and the configuration file it was read
/// from, which a refusal names when the file has no `[share]` table.
#[derive(Clone, Default)]
pub(super) struct ShareSetup {
    pub(super) bridge: Option<Arc<ShareBridge>>,
    config_file: Option<PathBuf>,
}

impl ShareSetup {
    /// The setup for `config`, read from `config_file`. Nothing is read from
    /// disk and nothing is connected here.
    pub(super) fn from_config(
        config: &KhiveConfig,
        config_file: Option<&Path>,
        connector: Arc<dyn Connector>,
    ) -> Result<Self, ConfigError> {
        Ok(Self {
            bridge: ShareBridge::from_config(config, connector)?.map(Arc::new),
            config_file: config_file.map(Path::to_path_buf),
        })
    }

    /// The result of a request that names `bridge.share`, or the refusal
    /// every operation of it fails with.
    pub(super) async fn answer(&self, parsed: &ParsedRequest) -> Result<Value, RoutedFailure> {
        if parsed.mode != ExecutionMode::Single || parsed.ops.len() != 1 {
            return Err(invalid(
                "bridge.share must be the only operation in an unbatched request",
            ));
        }
        let op = &parsed.ops[0];
        if op.args.len() == 1
            && matches!(
                op.args.get("help"),
                Some(ArgValue::Value(Value::Bool(true)))
            )
        {
            return Ok(share_help());
        }
        let Some(bridge) = &self.bridge else {
            let file = match &self.config_file {
                Some(path) => path.display().to_string(),
                None => "a configuration file, and this process loaded none".into(),
            };
            return Err(refused(
                SHARE_NOT_CONFIGURED,
                format!("bridge.share needs a [share] table in {file}; no file was read"),
            ));
        };
        bridge.share(op).await
    }
}

/// The `[share]` table as the process that resolved it holds it.
pub(super) struct ShareBridge {
    root: PathBuf,
    max_file_bytes: u64,
    max_total_bytes: u64,
    max_files: usize,
    targets: Vec<ShareTarget>,
    #[cfg(test)]
    pub(super) seams: ShareSeams,
}

/// One `[[share.targets]]` entry and the client that reaches it.
struct ShareTarget {
    name: String,
    /// `name (host:port)`, the way every message names the target.
    label: String,
    client: ComputerClient,
    /// Shares the target recorded for this process.
    shares: AtomicU64,
}

/// The test seams of a share.
#[cfg(test)]
#[derive(Default)]
pub(super) struct ShareSeams {
    /// Runs with each file's path after every check has passed and before the
    /// file is opened.
    #[allow(clippy::type_complexity)]
    pub(super) before_read: std::sync::Mutex<Option<Box<dyn Fn(&Path) + Send>>>,
    /// Drops every symbolic-link check, for the arm that shows they decide.
    pub(super) skip_link_check: std::sync::atomic::AtomicBool,
}

/// A path that passed every check, with the size that was checked.
struct Checked {
    path: String,
    absolute: PathBuf,
    size: u64,
}

/// The arguments of one `bridge.share`.
struct ShareRequest {
    target: String,
    receiver: String,
    paths: Vec<String>,
    note: Option<String>,
}

impl ShareRequest {
    fn parse(op: &ParsedOp) -> Result<Self, RoutedFailure> {
        if let Some(name) = op
            .args
            .keys()
            .find(|name| !ARGUMENTS.contains(&name.as_str()))
        {
            return Err(invalid(&format!(
                "bridge.share takes target, receiver, paths and note, not {name}"
            )));
        }
        let target =
            string_arg(op, "target")?.ok_or_else(|| invalid("bridge.share needs target"))?;
        let receiver = string_arg(op, "receiver")?
            .filter(|receiver| !receiver.is_empty())
            .ok_or_else(|| invalid("bridge.share needs a nonempty receiver"))?;
        let paths = match literal_arg(op, "paths")? {
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| invalid("each of bridge.share's paths must be a string"))
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err(invalid("bridge.share paths must be an array of strings")),
            None => return Err(invalid("bridge.share needs paths")),
        };
        if paths.is_empty() {
            return Err(invalid("bridge.share paths is empty; it names no file"));
        }
        let mut seen = BTreeSet::new();
        if let Some(repeated) = paths.iter().find(|path| !seen.insert(path.as_str())) {
            return Err(invalid(&format!(
                "bridge.share paths names {repeated:?} more than once"
            )));
        }
        let note = string_arg(op, "note")?;
        if let Some(note) = &note {
            if note.len() > MAX_NOTE_BYTES {
                return Err(invalid(&format!(
                    "bridge.share note is {} bytes, more than {MAX_NOTE_BYTES}",
                    note.len()
                )));
            }
        }
        Ok(Self {
            target,
            receiver,
            paths,
            note,
        })
    }
}

fn literal_arg<'a>(op: &'a ParsedOp, name: &str) -> Result<Option<&'a Value>, RoutedFailure> {
    match op.args.get(name) {
        None => Ok(None),
        Some(arg) => arg.as_value().map(Some).ok_or_else(|| {
            invalid(&format!(
                "bridge.share {name} must be a literal, not a $prev reference"
            ))
        }),
    }
}

fn string_arg(op: &ParsedOp, name: &str) -> Result<Option<String>, RoutedFailure> {
    match literal_arg(op, name)? {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(invalid(&format!("bridge.share {name} must be a string"))),
    }
}

/// The rule of the target's tree that `path` breaks, if it breaks one. The
/// tree's own check decides; the wording names which part of it.
fn tree_rule(path: &str) -> Option<&'static str> {
    if khive_runtime::blob_tree::validate_relative_path(path, "share").is_ok() {
        return None;
    }
    Some(if path.is_empty() {
        "the path is empty"
    } else if path.starts_with('/') {
        "it is absolute, and a path is relative to the share root"
    } else if path.contains('\0') {
        "it holds a NUL byte, which a tree path cannot hold"
    } else if path.contains('\\') {
        "it holds a backslash, which a tree path cannot hold"
    } else {
        "it has an empty, '.' or '..' component"
    })
}

impl ShareBridge {
    /// The bridge for `config`'s `[share]` table, or `None` without one.
    fn from_config(
        config: &KhiveConfig,
        connector: Arc<dyn Connector>,
    ) -> Result<Option<Self>, ConfigError> {
        let Some(share) = &config.share else {
            return Ok(None);
        };
        share.validate()?;
        let actor = config
            .actor
            .id
            .clone()
            .ok_or_else(|| ConfigError::InvalidShareConfig {
                key: "targets".into(),
                reason: "needs [actor].id beside it: each target's whoami is checked against \
                         that id"
                    .into(),
            })?;
        let mut targets = Vec::with_capacity(share.targets.len());
        for target in &share.targets {
            let endpoint = target.endpoint()?;
            let host = if endpoint.host.contains(':') {
                format!("[{}]", endpoint.host)
            } else {
                endpoint.host
            };
            let label = format!("{} ({host}:{})", target.name, endpoint.port);
            targets.push(ShareTarget {
                name: target.name.clone(),
                client: ComputerClient::for_share_target(
                    target,
                    label.clone(),
                    actor.clone(),
                    Arc::clone(&connector),
                ),
                label,
                shares: AtomicU64::new(0),
            });
        }
        Ok(Some(Self {
            root: share.root.clone(),
            max_file_bytes: share.max_file_bytes,
            max_total_bytes: share.max_total_bytes,
            max_files: share.max_files,
            targets,
            #[cfg(test)]
            seams: ShareSeams::default(),
        }))
    }

    /// Each target with the shares it recorded for this process, for
    /// `bridge.diagnostics`.
    pub(super) fn diagnostics(&self) -> Value {
        Value::Array(
            self.targets
                .iter()
                .map(|target| {
                    json!({
                        "target": target.name,
                        "label": target.label,
                        "shares": target.shares.load(Ordering::Relaxed),
                    })
                })
                .collect(),
        )
    }

    fn skips_link_check(&self) -> bool {
        #[cfg(test)]
        {
            self.seams.skip_link_check.load(Ordering::Relaxed)
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    async fn share(&self, op: &ParsedOp) -> Result<Value, RoutedFailure> {
        let request = ShareRequest::parse(op)?;
        let target = self
            .targets
            .iter()
            .find(|target| target.name == request.target)
            .ok_or_else(|| {
                refused(
                    SHARE_TARGET_UNKNOWN,
                    format!(
                        "{:?} names no [[share.targets]] entry; nothing was sent",
                        request.target
                    ),
                )
            })?;
        if request.paths.len() > self.max_files {
            return Err(refused(
                SHARE_TOO_LARGE,
                format!(
                    "{} paths name more files than max_files ({}); nothing was sent",
                    request.paths.len(),
                    self.max_files
                ),
            ));
        }
        let root = self.canonical_root()?;
        let files = self.check_all(&root, &request.paths)?;

        let mut sent = Vec::with_capacity(files.len());
        for file in &files {
            let bytes = self.read(&root, file)?;
            let digest = khive_pack_exec::tree::digest_hex(&bytes);
            upload(target, &bytes, &digest)
                .await
                .map_err(|failure| before_share(failure, None))?;
            sent.push((file.path.clone(), digest, file.size));
        }

        let entries = sent
            .iter()
            .map(|(path, digest, _)| json!({ "path": path, "ref": digest, "mode": SHARED_FILE_MODE }))
            .collect();
        let built = call(
            target,
            "exec.tree",
            vec![("entries", Value::Array(entries))],
        )
        .await
        .map_err(|error| before_share(error.into_failure(), None))?;
        let tree = built
            .get("tree")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                before_share(protocol(target, "answered exec.tree without a tree"), None)
            })?;

        let mut arguments = vec![
            ("tree", json!(tree)),
            ("receiver", json!(request.receiver)),
            (
                "files",
                Value::Array(
                    sent.iter()
                        .map(|(path, digest, _)| json!({ "path": path, "digest": digest }))
                        .collect(),
                ),
            ),
        ];
        if let Some(note) = &request.note {
            arguments.push(("note", json!(note)));
        }
        let recorded = match call(target, "computer.share", arguments).await {
            Ok(recorded) => recorded,
            Err(CallError::Refused(failure)) => return Err(failure),
            Err(CallError::Failed(failure))
                if matches!(failure.disposition, DomainDisposition::NotCommitted) =>
            {
                return Err(before_share(failure, Some(&tree)))
            }
            Err(CallError::Failed(failure)) => return Err(after_share(failure, &tree)),
        };
        let Some(share_id) = recorded.get("share_id").and_then(Value::as_str) else {
            let mut failure = protocol(
                target,
                &format!("answered computer.share for tree {tree} without a share_id"),
            );
            failure.error["tree"] = json!(tree);
            return Err(failure);
        };
        target.shares.fetch_add(1, Ordering::Relaxed);
        Ok(json!({
            "share_id": share_id,
            "target": target.name,
            "receiver": request.receiver,
            "tree": tree,
            "files": sent
                .iter()
                .map(|(path, digest, size)| json!({ "path": path, "digest": digest, "size": size }))
                .collect::<Vec<_>>(),
        }))
    }

    /// `root`, canonicalized now, which must be a directory.
    fn canonical_root(&self) -> Result<PathBuf, RoutedFailure> {
        let root = std::fs::canonicalize(&self.root).map_err(|error| {
            refused(
                SHARE_NOT_CONFIGURED,
                format!("[share].root cannot be read ({error}); no file was read"),
            )
        })?;
        if !root.is_dir() {
            return Err(refused(
                SHARE_NOT_CONFIGURED,
                "[share].root is not a directory; no file was read".into(),
            ));
        }
        Ok(root)
    }

    /// Every path checked, with the total, before anything is sent.
    fn check_all(&self, root: &Path, paths: &[String]) -> Result<Vec<Checked>, RoutedFailure> {
        let mut total: u64 = 0;
        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            let file = self.check(root, path)?;
            total = total.saturating_add(file.size);
            if total > self.max_total_bytes {
                return Err(refused(
                    SHARE_TOO_LARGE,
                    format!(
                        "the files through {path:?} total {total} bytes, more than \
                         max_total_bytes ({}); nothing was sent",
                        self.max_total_bytes
                    ),
                ));
            }
            files.push(file);
        }
        Ok(files)
    }

    /// The `blob.import` rules and the tree's path rules for one path.
    fn check(&self, root: &Path, path: &str) -> Result<Checked, RoutedFailure> {
        if let Some(rule) = tree_rule(path) {
            return Err(path_refused(path, rule));
        }
        let mut absolute = root.to_path_buf();
        for component in path.split('/') {
            absolute.push(component);
            let metadata = std::fs::symlink_metadata(&absolute)
                .map_err(|error| path_refused(path, &format!("it cannot be read ({error})")))?;
            if metadata.file_type().is_symlink() && !self.skips_link_check() {
                let link = absolute.strip_prefix(root).unwrap_or(&absolute);
                return Err(path_refused(
                    path,
                    &format!("{:?} is a symbolic link", link.display().to_string()),
                ));
            }
        }
        let metadata = std::fs::metadata(&absolute)
            .map_err(|error| path_refused(path, &format!("it cannot be read ({error})")))?;
        if !metadata.is_file() {
            return Err(path_refused(path, "it is not a regular file"));
        }
        if metadata.len() > self.max_file_bytes {
            return Err(refused(
                SHARE_TOO_LARGE,
                format!(
                    "{path:?} is {} bytes, more than max_file_bytes ({}); nothing was sent",
                    metadata.len(),
                    self.max_file_bytes
                ),
            ));
        }
        Ok(Checked {
            path: path.to_owned(),
            absolute,
            size: metadata.len(),
        })
    }

    /// The bytes of one checked file, read once from a handle opened without
    /// following links.
    fn read(&self, root: &Path, file: &Checked) -> Result<Vec<u8>, RoutedFailure> {
        #[cfg(test)]
        if let Some(seam) = self
            .seams
            .before_read
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            seam(&file.absolute);
        }
        let opened = self.open(root, file)?;
        let mut bytes = Vec::with_capacity(usize::try_from(file.size).unwrap_or_default());
        (&opened)
            .take(file.size.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| path_refused(&file.path, &format!("it cannot be read ({error})")))?;
        if bytes.len() as u64 != file.size {
            return Err(refused(
                SHARE_FILE_CHANGED,
                format!(
                    "{:?} was {} bytes when it was checked and a different size when it was \
                     read; it was not sent",
                    file.path, file.size
                ),
            ));
        }
        Ok(bytes)
    }

    fn open(&self, root: &Path, file: &Checked) -> Result<File, RoutedFailure> {
        let refuse = |rule: String| path_refused(&file.path, &rule);
        if self.skips_link_check() {
            return File::open(&file.absolute)
                .map_err(|error| refuse(format!("it cannot be opened ({error})")));
        }
        let opened = open_regular_file_nofollow(&file.absolute).map_err(|error| match error {
            ContainedOpenError::Open(error) if error.raw_os_error() == Some(libc::ELOOP) => {
                refuse("it is a symbolic link".into())
            }
            ContainedOpenError::NotRegular => refuse("it is not a regular file".into()),
            ContainedOpenError::Open(error)
            | ContainedOpenError::Metadata(error)
            | ContainedOpenError::Resolve(error) => {
                refuse(format!("it cannot be opened ({error})"))
            }
            ContainedOpenError::Escapes { .. } => refuse("it is outside the share root".into()),
        })?;
        // The handle is judged, not the name: a link put in place after the
        // checks above is refused by where the opened file really is.
        let resolved = opened_file_path(&opened)
            .map_err(|error| refuse(format!("the path behind it cannot be read ({error})")))?;
        if !resolved.starts_with(root) {
            return Err(refuse("it resolves outside the share root".into()));
        }
        Ok(opened)
    }
}

/// Why one call to the target did not return a result.
enum CallError {
    /// The target answered, refusing the operation.
    Refused(RoutedFailure),
    /// No answer: the target could not be reached, or did not answer in time.
    Failed(RoutedFailure),
}

impl CallError {
    fn into_failure(self) -> RoutedFailure {
        match self {
            Self::Refused(failure) | Self::Failed(failure) => failure,
        }
    }
}

fn protocol(target: &ShareTarget, what: &str) -> RoutedFailure {
    RoutedFailure {
        error: json!({
            "kind": COMPUTER_PROTOCOL,
            "message": format!("computer {} {what}", target.label),
        }),
        disposition: DomainDisposition::Unknown,
    }
}

fn disposition_named(value: &Value) -> DomainDisposition {
    match value.as_str() {
        Some("not_committed") => DomainDisposition::NotCommitted,
        Some("committed") => DomainDisposition::Committed,
        _ => DomainDisposition::Unknown,
    }
}

/// Sends one operation to `target` and returns its result.
async fn call(
    target: &ShareTarget,
    verb: &str,
    arguments: Vec<(&str, Value)>,
) -> Result<Value, CallError> {
    let op = ParsedOp {
        tool: verb.to_owned(),
        args: arguments
            .into_iter()
            .map(|(name, value)| (name.to_owned(), ArgValue::Value(value)))
            .collect(),
    };
    let ops = render_unit(std::slice::from_ref(&op)).map_err(|error| {
        CallError::Failed(invalid(&format!(
            "{verb} cannot be written as a request: {error}"
        )))
    })?;
    let options = ForwardedOptions {
        presentation: Some("verbose".into()),
        format: Some("json".into()),
        ..ForwardedOptions::default()
    };
    let text = target
        .client
        .send(ops, &options)
        .await
        .map_err(CallError::Failed)?;
    let envelope: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let entry = &envelope["results"][0];
    match entry.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(entry.get("result").cloned().unwrap_or(Value::Null)),
        Some(false) => Err(CallError::Refused(RoutedFailure {
            error: entry.get("error").cloned().unwrap_or(Value::Null),
            disposition: disposition_named(&entry["domain_disposition"]),
        })),
        None => Err(CallError::Failed(protocol(
            target,
            &format!("answered {verb} with no result for it"),
        ))),
    }
}

/// Puts `bytes` in the target's blob store under `digest`, skipping a blob
/// the target already holds.
async fn upload(target: &ShareTarget, bytes: &[u8], digest: &str) -> Result<(), RoutedFailure> {
    let begun = call(
        target,
        "blob.begin",
        vec![("size", json!(bytes.len())), ("content_ref", json!(digest))],
    )
    .await
    .map_err(CallError::into_failure)?;
    if let Some(stored) = begun.get("content_ref") {
        return expect_ref(target, "blob.begin", stored, digest);
    }
    let upload_id = begun
        .get("upload_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            protocol(
                target,
                "answered blob.begin with neither a content_ref nor an upload_id",
            )
        })?
        .to_owned();
    let part_limit = begun
        .get("part_limit")
        .and_then(Value::as_u64)
        .filter(|limit| *limit > 0)
        .ok_or_else(|| protocol(target, "answered blob.begin without a part_limit"))?;
    let part_len = usize::try_from(part_limit).unwrap_or(usize::MAX);
    for (index, part) in bytes.chunks(part_len).enumerate() {
        call(
            target,
            "blob.put_part",
            vec![
                ("upload_id", json!(upload_id)),
                ("index", json!(index)),
                ("bytes", json!(BASE64.encode(part))),
            ],
        )
        .await
        .map_err(CallError::into_failure)?;
    }
    let committed = call(target, "blob.commit", vec![("upload_id", json!(upload_id))])
        .await
        .map_err(CallError::into_failure)?;
    expect_ref(
        target,
        "blob.commit",
        committed.get("content_ref").unwrap_or(&Value::Null),
        digest,
    )
}

fn expect_ref(
    target: &ShareTarget,
    verb: &str,
    stored: &Value,
    digest: &str,
) -> Result<(), RoutedFailure> {
    if stored.as_str() == Some(digest) {
        Ok(())
    } else {
        Err(protocol(
            target,
            &format!(
                "answered {verb} with content_ref {stored} for a file whose digest is {digest}"
            ),
        ))
    }
}

/// A failure before `computer.share` was sent: no share exists. Blobs and a
/// tree already on the target stay there, and the tree is named.
fn before_share(failure: RoutedFailure, tree: Option<&str>) -> RoutedFailure {
    let mut error = match failure.error {
        Value::Object(error) => Value::Object(error),
        other => json!({ "kind": "computer_error", "message": other.to_string() }),
    };
    let note = match tree {
        Some(tree) => {
            error["tree"] = json!(tree);
            format!("; tree {tree} was built and no share was sent")
        }
        None => "; no share was sent".into(),
    };
    if let Some(Value::String(message)) = error.get_mut("message") {
        message.push_str(&note);
    }
    RoutedFailure {
        error,
        disposition: DomainDisposition::NotCommitted,
    }
}

/// A failure after `computer.share` was sent: whether the share was recorded
/// is the target's to answer, and it is never sent again.
fn after_share(failure: RoutedFailure, tree: &str) -> RoutedFailure {
    let cause = failure
        .error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("the target did not answer")
        .to_owned();
    RoutedFailure {
        error: json!({
            "kind": COMPUTER_UNREACHABLE,
            "message": format!(
                "{cause}; computer.share for tree {tree} was sent and not answered, so whether \
                 the share was recorded is the target's to answer; it was not sent again"
            ),
            "tree": tree,
        }),
        disposition: DomainDisposition::Unknown,
    }
}

fn share_help() -> Value {
    json!({
        "verb": SHARE_VERB,
        "pack": "bridge-control",
        "description": "Send named files under this process's [share] root to a share target as one tree, and share that tree with an actor there. Answered by this process; it must be the only operation in its request.",
        "category": "Directive",
        "params": [
            {"name": "target", "type": "string", "required": true, "description": "The name of a [[share.targets]] entry."},
            {"name": "receiver", "type": "string", "required": true, "description": "The actor id the share is addressed to; it must hold a key on the target."},
            {"name": "paths", "type": "array", "required": true, "description": "Distinct paths of regular files, relative to the share root."},
            {"name": "note", "type": "string", "required": false, "description": "At most 512 bytes, passed to the target with the share."}
        ],
        "result_schema": {
            "type": "object",
            "required": ["share_id", "target", "receiver", "tree", "files"],
            "properties": {
                "share_id": {"type": "string", "description": "The target's id for the share."},
                "tree": {"type": "string", "description": "The tree on the target that holds the files."},
                "files": {"type": "array", "description": "Each file's path, the BLAKE3 digest of the bytes sent, and its size."}
            }
        }
    })
}

impl KhiveMcpServer {
    /// Attach the `[share]` table of the configuration this process resolved,
    /// read from `config_file` (ADR-220 Amendment 1). Without the table,
    /// `bridge.share` is refused naming that file.
    pub(crate) fn with_share(
        mut self,
        config: &KhiveConfig,
        config_file: Option<&Path>,
    ) -> Result<Self, ConfigError> {
        self.share = ShareSetup::from_config(config, config_file, Arc::new(HttpConnector))?;
        Ok(self)
    }

    /// The answer to a request that names `bridge.share`.
    pub(super) async fn bridge_share_response(
        &self,
        p: &RequestParams,
        parsed: &ParsedRequest,
    ) -> Result<String, McpError> {
        let failed = |failure: RoutedFailure| {
            Ok(serialize_response_value(&failed_envelope(
                parsed,
                failure.error,
                failure.disposition,
            )))
        };
        if !self.stdio_bridge {
            return failed(invalid(
                "bridge.share is answered only by the stdio bridge and kkernel exec",
            ));
        }
        if p.save_to.is_some() {
            return failed(invalid("bridge.share does not accept save_to"));
        }
        validate_request_overrides(p, parsed.ops.len())?;
        match self.share.answer(parsed).await {
            Ok(result) => self.bridge_result_envelope(p, SHARE_VERB, result),
            Err(failure) => failed(failure),
        }
    }
}

/// The answer to `ops` when it names `bridge.share`, for `kkernel exec`: the
/// share runs in this process with `config`'s `[share]` table, which was read
/// from `config_file`. `None` when `ops` does not parse or does not name the
/// verb.
pub async fn answer_bridge_share_for_exec(
    config: &KhiveConfig,
    config_file: Option<&Path>,
    ops: &str,
) -> Option<Result<String, ConfigError>> {
    let parsed = parse_request(ops).ok()?;
    if !parsed_contains_bridge_share(&parsed) {
        return None;
    }
    let setup = match ShareSetup::from_config(config, config_file, Arc::new(HttpConnector)) {
        Ok(setup) => setup,
        Err(error) => return Some(Err(error)),
    };
    let envelope = match setup.answer(&parsed).await {
        Ok(result) => json!({
            "results": [{"ok": true, "tool": SHARE_VERB, "result": result}],
            "summary": {"total": 1, "succeeded": 1, "failed": 0, "aborted": 0},
            "status": "success",
        }),
        Err(failure) => failed_envelope(&parsed, failure.error, failure.disposition),
    };
    Some(Ok(serialize_response_value(&envelope)))
}

#[cfg(test)]
#[path = "bridge_share_tests.rs"]
mod tests;

//! Verb handlers for the exec pack. `run` is the pipeline of ADR-181 with
//! Amendment 1: policy and identity checks first (every refusal writes a
//! receipt and touches no disk), then materialize, sandbox, bound, capture,
//! receipt.

use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::process::Command;
use uuid::Uuid;

use khive_pack_tool::policy::{actor_label, decide};
use khive_runtime::{micros_to_iso, KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::ContentRef;

use crate::capture::{drain, walk, Tail};
use crate::receipts;
use crate::sandbox::{self, check_binary, render_profile, Resolved};
use crate::tree::{self, digest_hex, Change, TreeEntry};

// ── parameter helpers ────────────────────────────────────────────────────────

fn opt_str(params: &Value, key: &str) -> Result<Option<String>, RuntimeError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.to_string())),
        Some(other) => Err(RuntimeError::InvalidInput(format!(
            "{key} must be a string; got {other}"
        ))),
    }
}

fn req_str(params: &Value, key: &str) -> Result<String, RuntimeError> {
    match opt_str(params, key)? {
        Some(s) if !s.trim().is_empty() => Ok(s),
        _ => Err(RuntimeError::InvalidInput(format!("{key} is required"))),
    }
}

fn opt_limit(params: &Value, key: &str, default: u32, max: u32) -> Result<u32, RuntimeError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_u64()
            .map(|n| (n as u32).clamp(1, max))
            .ok_or_else(|| RuntimeError::InvalidInput(format!("{key} must be a positive integer"))),
    }
}

fn opt_str_list(params: &Value, key: &str) -> Result<Option<Vec<String>>, RuntimeError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str().map(str::to_string).ok_or_else(|| {
                    RuntimeError::InvalidInput(format!("{key} must be an array of strings"))
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(RuntimeError::InvalidInput(format!(
            "{key} must be an array of strings"
        ))),
    }
}

// ── tree verbs ───────────────────────────────────────────────────────────────

pub async fn tree_store(rt: &KhiveRuntime, params: Value) -> Result<Value, RuntimeError> {
    let entries = params
        .get("entries")
        .ok_or_else(|| RuntimeError::InvalidInput("entries is required".into()))?;
    let tree_ref = tree::store_from_value(rt, entries).await?;
    Ok(json!({ "tree": tree_ref }))
}

/// One validated edit, held until every sibling has validated too. Content bytes are carried
fn edit_mode(edit: &Value, index: usize) -> Result<Option<u64>, RuntimeError> {
    match edit.get("mode") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let mode = value.as_u64().ok_or_else(|| {
                RuntimeError::InvalidInput(format!("edits[{index}].mode must be an integer"))
            })?;
            // Manifest modes are decimal spellings, not octal permission literals.
            if mode != 644 && mode != 755 && mode != 120000 {
                return Err(RuntimeError::InvalidInput(format!(
                    "edits[{index}].mode must be 644, 755 or 120000, got {mode}"
                )));
            }
            Ok(Some(mode))
        }
    }
}

/// Apply a list of edits to a tree and return the new tree. The tree is an immutable manifest
/// blob, so this mints a new one and never mutates the input; a single-path edit is a list of one.
///
/// The atomicity promised is a property of the RESULT: one call yields exactly one new tree
/// reference or none, and a refusal on any entry leaves the blob store with no new object from
/// the call, including objects for entries that were fine. That is why content bytes are hashed
/// rather than written during validation. `digest_hex` is the same BLAKE3 the blob store keys on,
/// so a content edit's reference is known before the byte is stored, the whole candidate manifest
/// is validated through `tree::parse_entries`, and only a manifest that will parse causes a write.
pub async fn tree_put(rt: &KhiveRuntime, params: Value) -> Result<Value, RuntimeError> {
    let tree_ref = req_str(&params, "tree")?;
    let edits = params
        .get("edits")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            RuntimeError::InvalidInput("edits is required and must be an array".into())
        })?;
    // An empty list refuses rather than returning the input tree. An empty edit is almost always
    // a caller bug, and handing back the input would make a no-op look like work, which is the
    // same failure shape as silently accepting a delete of a path the tree does not hold.
    if edits.is_empty() {
        return Err(RuntimeError::InvalidInput(
            "edits is empty; an empty edit list is refused rather than returning the input tree"
                .into(),
        ));
    }
    let base = tree::load(rt, &tree_ref).await?;
    let mut entries: BTreeMap<String, (String, u64)> = base
        .iter()
        .map(|entry| {
            (
                entry.path.clone(),
                (entry.content_ref.clone(), u64::from(entry.mode)),
            )
        })
        .collect();

    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut pending: Vec<(String, Vec<u8>)> = Vec::new();
    let mut supplied_refs: Vec<String> = Vec::new();
    for (index, edit) in edits.iter().enumerate() {
        if !edit.is_object() {
            return Err(RuntimeError::InvalidInput(format!(
                "edits[{index}] must be an object"
            )));
        }
        let path = edit.get("path").and_then(Value::as_str).ok_or_else(|| {
            RuntimeError::InvalidInput(format!("edits[{index}].path is required"))
        })?;
        let path = tree::validate_relative_path(path, &format!("edits[{index}]"))?;
        // Duplicates refuse rather than last-one-wins: these lists are built by generated code and
        // by models, both of which produce duplicates, and last-one-wins makes the caller's second
        // intent vanish where nothing downstream can see that it happened.
        if let Some(first) = seen.get(&path) {
            return Err(RuntimeError::InvalidInput(format!(
                "edits[{first}] and edits[{index}] both name path {path:?}; duplicate paths are refused"
            )));
        }
        seen.insert(path.clone(), index);

        let deleting = matches!(edit.get("delete"), Some(Value::Bool(true)));
        let reference = opt_str(edit, "ref")?;
        let content = opt_str(edit, "content")?;
        let named = usize::from(deleting)
            + usize::from(reference.is_some())
            + usize::from(content.is_some());
        if named != 1 {
            return Err(RuntimeError::InvalidInput(format!(
                "edits[{index}] names {named} of ref, content and delete; exactly one is required"
            )));
        }
        if deleting {
            if edit.get("mode").is_some_and(|mode| !mode.is_null()) {
                return Err(RuntimeError::InvalidInput(format!(
                    "edits[{index}] is a delete and cannot carry a mode"
                )));
            }
            // A delete of a path the tree does not hold refuses rather than succeeding quietly,
            // because a silent no-op is how a caller comes to believe it removed something.
            if entries.remove(&path).is_none() {
                return Err(RuntimeError::InvalidInput(format!(
                    "edits[{index}] deletes path {path:?}, which the tree does not hold"
                )));
            }
            continue;
        }
        let mode = edit_mode(edit, index)?
            .or_else(|| entries.get(&path).map(|(_, mode)| *mode))
            .unwrap_or(644);
        let content_ref = match (reference, content) {
            (Some(reference), None) => {
                ContentRef::from_hex(&reference).map_err(|e| {
                    RuntimeError::InvalidInput(format!("edits[{index}].ref {reference:?}: {e}"))
                })?;
                supplied_refs.push(reference.clone());
                reference
            }
            (None, Some(content)) => {
                let bytes = content.into_bytes();
                let computed = digest_hex(&bytes);
                pending.push((computed.clone(), bytes));
                computed
            }
            _ => unreachable!("the exactly-one check above admits only these two shapes"),
        };
        entries.insert(path, (content_ref, mode));
    }

    // Validate the whole candidate manifest through the pack's one entry validator, which is what
    // enforces the file-versus-directory rule this verb could otherwise violate by construction.
    let candidate = Value::Array(
        entries
            .iter()
            .map(|(path, (content_ref, mode))| json!({"path": path, "ref": content_ref, "mode": mode}))
            .collect(),
    );
    let next = tree::parse_entries(&candidate)?;
    // A caller-supplied ref that names no object refuses here, still before any write.
    let referenced: Vec<TreeEntry> = next
        .iter()
        .filter(|entry| supplied_refs.contains(&entry.content_ref))
        .cloned()
        .collect();
    tree::verify_blobs(rt, &referenced).await?;

    // Validation is complete, so from here every write is one the whole call has earned.
    let blobs = tree::blob_store(rt)?;
    for (expected, bytes) in pending {
        let stored = blobs.put(bytes).await?;
        debug_assert_eq!(
            stored.as_str(),
            expected,
            "blob store keys on a different digest"
        );
    }
    let next_ref = tree::store(rt, &next).await?;
    let changed: Vec<Value> = tree::diff(&base, &next)
        .iter()
        .map(Change::to_json)
        .collect();
    Ok(json!({ "tree": next_ref, "base": tree_ref, "entries": next.len(), "changed": changed }))
}

pub async fn tree_get(rt: &KhiveRuntime, params: Value) -> Result<Value, RuntimeError> {
    let tree_ref = req_str(&params, "tree")?;
    let entries = tree::load(rt, &tree_ref).await?;
    Ok(json!({ "tree": tree_ref, "entries": tree::entries_json(&entries) }))
}

pub async fn tree_diff(rt: &KhiveRuntime, params: Value) -> Result<Value, RuntimeError> {
    let base_ref = req_str(&params, "base")?;
    let head_ref = req_str(&params, "head")?;
    let base = tree::load(rt, &base_ref).await?;
    let head = tree::load(rt, &head_ref).await?;
    let changed: Vec<Value> = tree::diff(&base, &head)
        .iter()
        .map(Change::to_json)
        .collect();
    Ok(json!({ "base": base_ref, "head": head_ref, "changed": changed }))
}

// ── receipts ─────────────────────────────────────────────────────────────────

pub async fn receipt(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let id = req_str(&params, "id")?;
    receipts::get(rt, token.namespace().as_str(), &id).await
}

pub async fn runs(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let actor = req_str(&params, "actor")?;
    let tool = opt_str(&params, "tool")?;
    let session_id = opt_str(&params, "session_id")?;
    let limit = opt_limit(&params, "limit", 20, 500)?;
    let rows = receipts::list(
        rt,
        token.namespace().as_str(),
        &actor,
        tool.as_deref(),
        session_id.as_deref(),
        limit,
    )
    .await?;
    Ok(json!({ "runs": rows, "count": rows.len() }))
}

pub async fn events(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let run_id = opt_str(&params, "run_id")?;
    let limit = opt_limit(&params, "limit", 200, 5000)?;
    let rows = receipts::events(rt, token.namespace().as_str(), run_id.as_deref(), limit).await?;
    Ok(json!({ "events": rows, "count": rows.len() }))
}

pub fn identity(cfg: &Resolved) -> Value {
    let roots: Vec<String> = cfg
        .read_roots
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    json!({
        "root": cfg.root.to_string_lossy(),
        "read_roots": roots,
        "read_roots_serialization": "compact JSON array of the sorted canonical read roots; digest = BLAKE3 hex",
        "read_roots_digest": sandbox::read_roots_digest(&cfg.read_roots),
        "profile_template_digest": sandbox::template_digest(),
        "system_read_roots": sandbox::SYSTEM_READ_ROOTS,
        "env_keys": cfg.env_keys,
        "never": cfg.never.iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>(),
        "max_output_bytes": cfg.max_output_bytes,
        "timeout_default_s": cfg.timeout_default_s,
        "timeout_max_s": cfg.timeout_max_s,
        "keep": cfg.keep,
        "limits": cfg.limits.to_json(),
        "digest": "blake3-hex",
    })
}

// ── run ──────────────────────────────────────────────────────────────────────

/// Everything a receipt carries; serialized once for the row and the wire.
struct Receipt {
    id: String,
    actor: String,
    tool: String,
    argv: Vec<String>,
    tree_in: String,
    tree_out: Option<String>,
    exit_code: Option<i64>,
    exit_signal: Option<i64>,
    timed_out: bool,
    denied: bool,
    success: bool,
    reason: Option<String>,
    decision: Option<Value>,
    stdout_ref: Option<String>,
    stderr_ref: Option<String>,
    stdout_produced: u64,
    stderr_produced: u64,
    stdout_retained: u64,
    stderr_retained: u64,
    stdout_capture: &'static str,
    stderr_capture: &'static str,
    changed: Vec<Change>,
    undeclared: Vec<String>,
    skipped: Vec<String>,
    cwd: String,
    env_keys: Vec<String>,
    session_id: Option<String>,
    seq: Option<i64>,
    sandbox: Option<Value>,
    profile_ref: Option<String>,
    limits: Value,
    pids: Option<Value>,
    started_at: Option<i64>,
    finished_at: Option<i64>,
}

impl Receipt {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "actor": self.actor,
            "tool": self.tool,
            "argv": self.argv,
            "tree_in": self.tree_in,
            "tree_out": self.tree_out,
            "exit_code": self.exit_code,
            "exit_signal": self.exit_signal,
            "timed_out": self.timed_out,
            "denied": self.denied,
            "success": self.success,
            "reason": self.reason,
            "decision": self.decision,
            "stdout_ref": self.stdout_ref,
            "stderr_ref": self.stderr_ref,
            "stdout_produced_bytes": self.stdout_produced,
            "stderr_produced_bytes": self.stderr_produced,
            "stdout_retained_bytes": self.stdout_retained,
            "stderr_retained_bytes": self.stderr_retained,
            "stdout_capture": self.stdout_capture,
            "stderr_capture": self.stderr_capture,
            "changed": self.changed.iter().map(Change::to_json).collect::<Vec<_>>(),
            "undeclared_changes": self.undeclared,
            "skipped": self.skipped,
            "cwd": self.cwd,
            "env_keys": self.env_keys,
            "session_id": self.session_id,
            "seq": self.seq,
            "sandbox": self.sandbox,
            "profile_ref": self.profile_ref,
            "limits": self.limits,
            "pids": self.pids,
            "started_at": self.started_at.map(micros_to_iso),
            "finished_at": self.finished_at.map(micros_to_iso),
            "duration_ms": match (self.started_at, self.finished_at) {
                (Some(s), Some(f)) => Some((f - s) / 1000),
                _ => None,
            },
        })
    }
}

/// Parsed and validated run request, before any policy decision.
struct Request {
    tree_in: String,
    tool: String,
    args: Vec<String>,
    actor: String,
    cwd: String,
    env: BTreeMap<String, String>,
    timeout: Duration,
    session_id: Option<String>,
    declared: Option<Vec<String>>,
}

fn parse_request(params: &Value, cfg: &Resolved) -> Result<Request, RuntimeError> {
    let tree_in = req_str(params, "tree")?;
    let tool = req_str(params, "tool")?;
    let actor = req_str(params, "actor")?;
    let args = opt_str_list(params, "args")?.unwrap_or_default();
    let cwd = opt_str(params, "cwd")?.unwrap_or_else(|| ".".into());
    let env = match params.get("env") {
        None | Some(Value::Null) => BTreeMap::new(),
        Some(Value::Object(map)) => {
            let mut out = BTreeMap::new();
            for (k, v) in map {
                let value = v.as_str().ok_or_else(|| {
                    RuntimeError::InvalidInput(format!("env[{k:?}] must be a string"))
                })?;
                out.insert(k.clone(), value.to_string());
            }
            out
        }
        Some(_) => return Err(RuntimeError::InvalidInput("env must be an object".into())),
    };
    let timeout_s = match params.get("timeout_s") {
        None | Some(Value::Null) => cfg.timeout_default_s,
        Some(v) => v.as_f64().filter(|t| *t > 0.0).ok_or_else(|| {
            RuntimeError::InvalidInput("timeout_s must be a positive number".into())
        })?,
    };
    if timeout_s > cfg.timeout_max_s {
        return Err(RuntimeError::InvalidInput(format!(
            "timeout_s {timeout_s} exceeds the configured ceiling {}",
            cfg.timeout_max_s
        )));
    }
    let session_id = opt_str(params, "session_id")?.filter(|s| !s.is_empty());
    let declared = opt_str_list(params, "declared_write_paths")?;
    if let Some(list) = &declared {
        for p in list {
            tree::validate_relative_path(p, "declared_write_paths")?;
        }
    }
    Ok(Request {
        tree_in,
        tool,
        args,
        actor,
        cwd,
        env,
        timeout: Duration::from_secs_f64(timeout_s),
        session_id,
        declared,
    })
}

fn tool_binary(entity: &khive_storage::Entity) -> Result<String, RuntimeError> {
    let props = entity.properties.clone().unwrap_or(Value::Null);
    if entity.entity_type.as_deref() != Some("tool") {
        return Err(RuntimeError::InvalidInput(format!(
            "registry object {:?} is a {}, not a tool",
            entity.name,
            entity.entity_type.as_deref().unwrap_or("unknown kind")
        )));
    }
    let source = props
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match source.strip_prefix("exec:") {
        Some(path) if path.starts_with('/') => Ok(path.to_string()),
        _ => Err(RuntimeError::InvalidInput(format!(
            "tool {:?} has source {source:?}; exec.run needs source exec:<absolute path>",
            entity.name
        ))),
    }
}

fn refusal_error(reason: &str, id: &str) -> RuntimeError {
    RuntimeError::InvalidInput(format!("exec.run refused: {reason} (receipt_id={id})"))
}

pub async fn run(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    cfg: &Resolved,
    params: Value,
) -> Result<Value, RuntimeError> {
    let ns = token.namespace().as_str().to_string();
    let id = Uuid::new_v4().to_string();
    // Parse failures before the actor is known cannot be attributed; they
    // are plain invalid input, not refusals.
    let req = parse_request(&params, cfg)?;
    let mut receipt = Receipt {
        id: id.clone(),
        actor: req.actor.clone(),
        tool: req.tool.clone(),
        argv: vec![],
        tree_in: req.tree_in.clone(),
        tree_out: None,
        exit_code: None,
        exit_signal: None,
        timed_out: false,
        denied: false,
        success: false,
        reason: None,
        decision: None,
        stdout_ref: None,
        stderr_ref: None,
        stdout_produced: 0,
        stderr_produced: 0,
        stdout_retained: 0,
        stderr_retained: 0,
        stdout_capture: "none",
        stderr_capture: "none",
        changed: vec![],
        undeclared: vec![],
        skipped: vec![],
        cwd: req.cwd.clone(),
        env_keys: vec![],
        session_id: req.session_id.clone(),
        seq: None,
        sandbox: None,
        profile_ref: None,
        limits: json!({ "requested": cfg.limits.to_json(), "enforced": Value::Null }),
        pids: None,
        started_at: None,
        finished_at: None,
    };

    match preflight(rt, token, cfg, &req, &mut receipt).await {
        Ok(ready) => {
            execute(rt, &ns, cfg, &req, ready, &mut receipt).await?;
            let mut value = receipt.to_json();
            let seq = receipts::insert(rt, &ns, &value).await?;
            value["seq"] = seq.map_or(Value::Null, Value::from);
            Ok(json!({
                "receipt": value,
                "changed": receipt.changed.iter().map(Change::to_json).collect::<Vec<_>>(),
            }))
        }
        Err(reason) => {
            receipt.denied = true;
            receipt.success = false;
            receipt.reason = Some(reason.clone());
            let value = receipt.to_json();
            receipts::insert(rt, &ns, &value).await?;
            Err(refusal_error(&reason, &id))
        }
    }
}

/// What preflight hands to execution once every refusal rule passed.
struct Ready {
    binary: PathBuf,
    registered: String,
    entries: Vec<TreeEntry>,
}

/// Every rule that refuses before the disk is touched. `Err(reason)` is the
/// refusal reason; the receipt is filled with whatever was decided so far.
async fn preflight(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    cfg: &Resolved,
    req: &Request,
    receipt: &mut Receipt,
) -> Result<Ready, String> {
    // Identity: the actor parameter must be the authenticated caller.
    let caller = actor_label(token);
    if !token.actor().is_anonymous() && caller != req.actor {
        return Err(format!(
            "actor {:?} does not match the authenticated caller {caller:?}",
            req.actor
        ));
    }
    // Registration.
    let entity = khive_pack_tool::resolve_registered(rt, token, &req.tool)
        .await
        .map_err(|e| format!("tool {:?} is not registered: {e}", req.tool))?;
    let registered = tool_binary(&entity).map_err(|e| e.to_string())?;
    // Binary identity (Amendment 1 item 8) before policy: a forbidden binary
    // is refused whatever the policy says.
    let binary = check_binary(&registered, &cfg.never).map_err(|e| e.to_string())?;
    receipt.argv = std::iter::once(registered.clone())
        .chain(req.args.iter().cloned())
        .collect();
    // Policy.
    let side_effect = serde_json::to_value(&entity.properties).ok().and_then(|p| {
        p.get("side_effect")
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    let decision = decide(
        rt,
        token.namespace().as_str(),
        &req.actor,
        &entity.name,
        side_effect.as_deref(),
    )
    .await
    .map_err(|e| format!("policy evaluation failed: {e}"))?;
    let decision_json = json!({
        "decision": decision.decision,
        "source": decision.source,
        "id": decision.grant_id.clone().or(decision.policy_id.clone()),
    });
    receipt.decision = Some(decision_json);
    if decision.decision != "allow" {
        return Err(format!(
            "tool.check({:?}, {:?}) = {} from {}",
            req.actor, entity.name, decision.decision, decision.source
        ));
    }
    // Tree and cwd.
    let entries = tree::load(rt, &req.tree_in)
        .await
        .map_err(|e| format!("tree: {e}"))?;
    tree::verify_blobs(rt, &entries)
        .await
        .map_err(|e| format!("tree: {e}"))?;
    let cwd = tree::resolve_cwd(rt, &entries, &req.cwd)
        .await
        .map_err(|e| e.to_string())?;
    receipt.cwd = cwd;
    Ok(Ready {
        binary,
        registered,
        entries,
    })
}

fn materialize(
    run_dir: &Path,
    entries: &[TreeEntry],
    bytes: &BTreeMap<String, Vec<u8>>,
) -> std::io::Result<()> {
    std::fs::create_dir(run_dir)?;
    let result = materialize_entries(run_dir, entries, bytes);
    if result.is_err() {
        // Remove only the fresh root we own, never a pre-existing root whose
        // create_dir failed. A partial input tree is not a keepable run.
        let _ = std::fs::remove_dir_all(run_dir);
    }
    result
}

fn materialize_entries(
    run_dir: &Path,
    entries: &[TreeEntry],
    bytes: &BTreeMap<String, Vec<u8>>,
) -> std::io::Result<()> {
    use std::io::Write;

    // Populate directories and files before creating any links. Filesystem aliases
    // (including case-insensitive names) must not redirect a later materialization write.
    for entry in entries {
        let target = run_dir.join(&entry.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if entry.mode == 120000 {
            continue;
        }
        let data = bytes
            .get(&entry.content_ref)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)?;
        file.write_all(data)?;
        let mode = if entry.mode == 755 { 0o755 } else { 0o644 };
        #[cfg(unix)]
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        #[cfg(not(unix))]
        let _ = mode;
    }
    for entry in entries.iter().filter(|entry| entry.mode == 120000) {
        let data = bytes
            .get(&entry.content_ref)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            std::os::unix::fs::symlink(
                std::ffi::OsStr::from_bytes(data),
                run_dir.join(&entry.path),
            )?;
        }
        #[cfg(not(unix))]
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "symlink materialization requires a unix host",
        ));
    }
    Ok(())
}

fn declared_covers(declared: &[String], path: &str) -> bool {
    declared
        .iter()
        .any(|d| d == path || path.starts_with(&format!("{d}/")))
}

/// The resource identifier `setrlimit` takes: an enum-typed integer on glibc,
/// a plain `c_int` on every other unix libc.
#[cfg(all(unix, target_os = "linux", target_env = "gnu"))]
type RlimitResource = libc::__rlimit_resource_t;
#[cfg(all(unix, not(all(target_os = "linux", target_env = "gnu"))))]
type RlimitResource = libc::c_int;

/// Launching a tool needs a process group, resource limits, a close-on-exec
/// pipe for the limit report and the sandbox: unix facilities. On any other
/// host `exec.run` refuses before touching the store or the filesystem.
#[cfg(not(unix))]
async fn execute(
    _rt: &KhiveRuntime,
    _ns: &str,
    _cfg: &Resolved,
    _req: &Request,
    _ready: Ready,
    _receipt: &mut Receipt,
) -> Result<(), RuntimeError> {
    Err(RuntimeError::Unconfigured(
        "exec.run launches tools on unix hosts only; this host provides none of the process-group, resource-limit and sandbox facilities the run contract requires".into(),
    ))
}

#[cfg(unix)]
async fn execute(
    rt: &KhiveRuntime,
    ns: &str,
    cfg: &Resolved,
    req: &Request,
    ready: Ready,
    receipt: &mut Receipt,
) -> Result<(), RuntimeError> {
    let store = tree::blob_store(rt)?;
    // Hydrate every input blob before creating the run directory so a store
    // failure leaves no directory behind.
    let mut bytes: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for entry in &ready.entries {
        if bytes.contains_key(&entry.content_ref) {
            continue;
        }
        let content_ref = ContentRef::from_hex(&entry.content_ref)
            .map_err(|e| RuntimeError::InvalidInput(format!("entry {:?} ref: {e}", entry.path)))?;
        let data = store
            .get_bounded_verified(&content_ref, khive_storage::MAX_BLOB_WHOLE_BYTES)
            .await?;
        bytes.insert(entry.content_ref.clone(), data);
    }

    std::fs::create_dir_all(&cfg.root).map_err(|e| {
        RuntimeError::Unconfigured(format!("exec root {}: {e}", cfg.root.display()))
    })?;
    let root = std::fs::canonicalize(&cfg.root).map_err(|e| {
        RuntimeError::Unconfigured(format!("exec root {}: {e}", cfg.root.display()))
    })?;
    let run_dir = root.join(&receipt.id);
    if let Err(error) = materialize(&run_dir, &ready.entries, &bytes) {
        receipt.success = false;
        receipt.reason = Some(format!("materialize {}: {error}", run_dir.display()));
        receipt.finished_at = Some(receipts::now_micros());
        // No profile or child exists yet. Return through run's receipt insertion.
        return Ok(());
    }
    receipts::event(
        rt,
        ns,
        &receipt.id,
        "materialized",
        json!({ "run_dir": run_dir.to_string_lossy(), "entries": ready.entries.len() }),
    )
    .await?;

    // Profile: rendered per run, stored as a blob, written beside the run
    // directory for sandbox-exec to read, removed with it.
    let profile = render_profile(&run_dir, &cfg.read_roots, &cfg.never);
    let profile_ref = store.put(profile.clone().into_bytes()).await?;
    let profile_path = root.join(format!("{}.sb", receipt.id));
    std::fs::write(&profile_path, &profile).map_err(|e| {
        RuntimeError::Unconfigured(format!("profile {}: {e}", profile_path.display()))
    })?;
    let binary_bytes = std::fs::read(&ready.binary).unwrap_or_default();
    receipt.sandbox = Some(json!({
        "profile_digest": profile_ref.as_str(),
        "tool_binary_digest": digest_hex(&binary_bytes),
        "read_roots_digest": sandbox::read_roots_digest(&cfg.read_roots),
    }));
    receipt.profile_ref = Some(profile_ref.as_str().to_string());

    // Environment: caller values for allow-listed keys only, plus HOME.
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in &req.env {
        if cfg.env_keys.iter().any(|allowed| allowed == k) {
            env.insert(k.clone(), v.clone());
        }
    }
    env.insert("HOME".into(), run_dir.to_string_lossy().to_string());
    receipt.env_keys = env.keys().cloned().collect();

    let work_dir = if receipt.cwd == "." {
        run_dir.clone()
    } else {
        run_dir.join(&receipt.cwd)
    };
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command
        .arg("-f")
        .arg(&profile_path)
        .arg(&ready.registered)
        .args(&req.args)
        .env_clear()
        .envs(&env)
        .current_dir(&work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let limits = cfg.limits.clone();
    let (limit_reader, limit_writer) = limit_pipe()?;
    // SAFETY: the closure runs in the forked child before exec and only
    // calls async-signal-safe libc functions.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut report = String::from("{");
            let mut first = true;
            let mut apply =
                |name: &str, resource: RlimitResource, value: u64| -> std::io::Result<()> {
                    let lim = libc::rlimit {
                        rlim_cur: value as libc::rlim_t,
                        rlim_max: value as libc::rlim_t,
                    };
                    if libc::setrlimit(resource, &lim) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    let mut back = libc::rlimit {
                        rlim_cur: 0,
                        rlim_max: 0,
                    };
                    if libc::getrlimit(resource, &mut back) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if !first {
                        report.push(',');
                    }
                    first = false;
                    report.push_str(&format!("\"{name}\":{}", back.rlim_cur));
                    Ok(())
                };
            if let Some(v) = limits.cpu_seconds {
                apply("cpu_seconds", libc::RLIMIT_CPU, v)?;
            }
            if let Some(v) = limits.file_size {
                apply("file_size", libc::RLIMIT_FSIZE, v)?;
            }
            #[cfg(not(target_os = "macos"))]
            {
                if let Some(v) = limits.address_space {
                    apply("address_space", libc::RLIMIT_AS, v)?;
                }
                if let Some(v) = limits.nproc {
                    apply("nproc", libc::RLIMIT_NPROC, v)?;
                }
            }
            report.push('}');
            let bytes = report.as_bytes();
            libc::write(
                limit_writer,
                bytes.as_ptr() as *const libc::c_void,
                bytes.len(),
            );
            libc::close(limit_writer);
            Ok(())
        });
    }

    let started = Instant::now();
    receipt.started_at = Some(receipts::now_micros());
    let spawn = command.spawn();
    // Parent side of the pipe: close the writer, read the child's report.
    unsafe {
        libc::close(limit_writer);
    }
    let mut child = match spawn {
        Ok(c) => c,
        Err(e) => {
            unsafe {
                libc::close(limit_reader);
            }
            cleanup(&run_dir, &profile_path, cfg.keep);
            return Err(RuntimeError::Unconfigured(format!(
                "spawning sandbox-exec for {}: {e}",
                ready.registered
            )));
        }
    };
    let enforced = read_limit_report(limit_reader);
    receipt.limits = json!({ "requested": cfg.limits.to_json(), "enforced": enforced });
    let pid = child.id().unwrap_or_default() as i32;
    receipt.pids = Some(json!({ "child": pid, "pgid": pid }));
    receipts::event(
        rt,
        ns,
        &receipt.id,
        "launched",
        json!({ "pid": pid, "pgid": pid, "argv": receipt.argv }),
    )
    .await?;

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let cap = cfg.max_output_bytes;
    let out_task = tokio::spawn(async move { drain(stdout, cap).await });
    let err_task = tokio::spawn(async move { drain(stderr, cap).await });

    let status = match tokio::time::timeout(req.timeout, child.wait()).await {
        Ok(Ok(status)) => Some(status),
        Ok(Err(_)) => None,
        Err(_) => {
            receipt.timed_out = true;
            kill_group(pid);
            let _ = child.wait().await;
            None
        }
    };
    // Whatever the child left behind in its group ends with the run.
    kill_group(pid);
    let out: Tail = out_task.await.unwrap_or_else(|_| Tail::new(cap));
    let err: Tail = err_task.await.unwrap_or_else(|_| Tail::new(cap));
    receipt.finished_at = Some(receipts::now_micros());
    receipts::event(
        rt,
        ns,
        &receipt.id,
        "exited",
        json!({ "pid": pid, "timed_out": receipt.timed_out, "elapsed_ms": started.elapsed().as_millis() as u64 }),
    )
    .await?;

    if let Some(status) = status {
        use std::os::unix::process::ExitStatusExt;
        receipt.exit_code = status.code().map(i64::from);
        receipt.exit_signal = status.signal().map(i64::from);
    }

    // Outputs.
    receipt.stdout_produced = out.produced();
    receipt.stderr_produced = err.produced();
    let out_bytes = out.retained();
    let err_bytes = err.retained();
    receipt.stdout_retained = out_bytes.len() as u64;
    receipt.stderr_retained = err_bytes.len() as u64;
    receipt.stdout_capture = if out.complete() {
        "complete"
    } else {
        "incomplete"
    };
    receipt.stderr_capture = if err.complete() {
        "complete"
    } else {
        "incomplete"
    };
    receipt.stdout_ref = Some(store.put(out_bytes).await?.as_str().to_string());
    receipt.stderr_ref = Some(store.put(err_bytes).await?.as_str().to_string());

    // Capture errors describe a completed, unsuccessful run. Finalize its
    // receipt rather than propagating past receipt insertion in `run`.
    let captured: Result<(), RuntimeError> = async {
        let (found, skipped) =
            walk(&run_dir).map_err(|e| RuntimeError::Unconfigured(format!("capture tree: {e}")))?;
        receipt.skipped = skipped;
        let input: BTreeMap<&str, &TreeEntry> =
            ready.entries.iter().map(|e| (e.path.as_str(), e)).collect();
        let mut out_entries: Vec<TreeEntry> = Vec::new();
        let mut changes: Vec<Change> = Vec::new();
        let mut undeclared: BTreeSet<String> = BTreeSet::new();
        for (path, file) in &found {
            let data = file
                .read_content()
                .map_err(|e| RuntimeError::Unconfigured(format!("capture entry {path:?}: {e}")))?;
            let digest = digest_hex(&data);
            match input.get(path.as_str()) {
                Some(old) if old.content_ref == digest && old.mode == file.mode => {
                    out_entries.push((*old).clone());
                }
                existing => {
                    let allowed = req
                        .declared
                        .as_ref()
                        .is_none_or(|d| declared_covers(d, path));
                    if !allowed {
                        undeclared.insert(path.clone());
                        if let Some(old) = existing {
                            out_entries.push((*old).clone());
                        }
                        continue;
                    }
                    let stored = store.put(data).await?;
                    let entry = TreeEntry {
                        path: path.clone(),
                        content_ref: stored.as_str().to_string(),
                        mode: file.mode,
                    };
                    changes.push(Change {
                        path: path.clone(),
                        op: if existing.is_some() {
                            "modified"
                        } else {
                            "added"
                        },
                        content_ref: Some(entry.content_ref.clone()),
                        base_ref: existing.map(|e| e.content_ref.clone()),
                    });
                    out_entries.push(entry);
                }
            }
        }
        for (path, old) in &input {
            if found.contains_key(*path) {
                continue;
            }
            let allowed = req
                .declared
                .as_ref()
                .is_none_or(|d| declared_covers(d, path));
            if !allowed {
                undeclared.insert(path.to_string());
                out_entries.push((*old).clone());
                continue;
            }
            changes.push(Change {
                path: path.to_string(),
                op: "deleted",
                content_ref: None,
                base_ref: Some(old.content_ref.clone()),
            });
        }
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        receipt.changed = changes;
        receipt.undeclared = undeclared.into_iter().collect();
        receipt.tree_out = Some(tree::store(rt, &out_entries).await?);
        receipt.success =
            !receipt.timed_out && receipt.exit_code == Some(0) && receipt.undeclared.is_empty();

        Ok(())
    }
    .await;
    if let Err(error) = captured {
        receipt.success = false;
        receipt.reason = Some(error.to_string());
        receipt.tree_out = None;
        receipt.changed.clear();
        receipt.undeclared.clear();
        // A partial capture is never retained as a purported output tree,
        // including when successful runs would otherwise be kept.
        cleanup(&run_dir, &profile_path, false);
        return Ok(());
    }

    cleanup(&run_dir, &profile_path, cfg.keep);
    Ok(())
}

fn cleanup(run_dir: &Path, profile_path: &Path, keep: bool) {
    let _ = std::fs::remove_file(profile_path);
    if !keep {
        let _ = std::fs::remove_dir_all(run_dir);
    }
}

#[cfg(unix)]
fn kill_group(pid: i32) {
    if pid <= 0 {
        return;
    }
    unsafe {
        libc::killpg(pid, libc::SIGKILL);
    }
}

#[cfg(unix)]
fn limit_pipe() -> Result<(libc::c_int, libc::c_int), RuntimeError> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: plain pipe creation; both ends are marked close-on-exec so the
    // writer closes in the child at exec and the reader never leaks.
    unsafe {
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return Err(RuntimeError::Unconfigured(format!(
                "pipe: {}",
                std::io::Error::last_os_error()
            )));
        }
        for fd in fds {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
    Ok((fds[0], fds[1]))
}

#[cfg(unix)]
fn read_limit_report(reader: libc::c_int) -> Value {
    use std::io::Read;
    use std::os::unix::io::FromRawFd;
    // SAFETY: we own the descriptor and close it exactly once through File.
    let mut file = unsafe { std::fs::File::from_raw_fd(reader) };
    let mut text = String::new();
    let _ = file.read_to_string(&mut text);
    serde_json::from_str(&text).unwrap_or_else(|_| json!({}))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn materialize_preserves_literal_symlink_targets() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("run");
        let targets: &[(&str, &[u8])] = &[
            ("inside", b"sub/../file"),
            ("escape", b"../../x"),
            ("absolute", b"/absolute/missing"),
            ("non_utf8", b"target-\xff"),
        ];
        let mut entries = vec![TreeEntry {
            path: "file".into(),
            content_ref: digest_hex(b"content"),
            mode: 644,
        }];
        let mut blobs = BTreeMap::from([(digest_hex(b"content"), b"content".to_vec())]);
        for (path, target) in targets {
            let content_ref = digest_hex(target);
            entries.push(TreeEntry {
                path: (*path).into(),
                content_ref: content_ref.clone(),
                mode: 120000,
            });
            blobs.insert(content_ref, target.to_vec());
        }
        materialize(&root, &entries, &blobs).unwrap();
        assert_eq!(std::fs::read(root.join("file")).unwrap(), b"content");
        for (path, expected) in targets {
            let path = root.join(path);
            assert!(std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(
                std::fs::read_link(path).unwrap().as_os_str().as_bytes(),
                *expected
            );
        }
    }

    #[test]
    fn materialize_symlink_aliases_never_redirect_file_writes() {
        for descendant in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let outside = dir.path().join("outside");
            std::fs::create_dir(&outside).unwrap();
            let sentinel = outside.join("child");
            std::fs::write(&sentinel, b"unchanged").unwrap();
            let link_target = if descendant {
                outside.clone()
            } else {
                sentinel.clone()
            };
            let target = link_target.as_os_str().as_bytes().to_vec();
            let link_ref = digest_hex(&target);
            let file_ref = digest_hex(b"replacement");
            let entries = vec![
                TreeEntry {
                    path: "A".into(),
                    content_ref: link_ref.clone(),
                    mode: 120000,
                },
                TreeEntry {
                    path: if descendant { "a/child" } else { "a" }.into(),
                    content_ref: file_ref.clone(),
                    mode: 644,
                },
            ];
            let bytes = BTreeMap::from([(link_ref, target), (file_ref, b"replacement".to_vec())]);
            let root = dir.path().join("run");
            let result = materialize(&root, &entries, &bytes);
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"unchanged");
            match result {
                Ok(()) => {
                    // A case-sensitive filesystem can represent both paths safely.
                    assert!(std::fs::symlink_metadata(root.join("A"))
                        .unwrap()
                        .file_type()
                        .is_symlink());
                    assert_eq!(
                        std::fs::read(root.join(if descendant { "a/child" } else { "a" })).unwrap(),
                        b"replacement"
                    );
                }
                Err(error) => {
                    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
                    assert!(!root.exists(), "the partial input tree must be removed");
                }
            }
        }
    }

    #[test]
    fn materialize_refuses_an_existing_root_before_writes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("run");
        std::fs::create_dir(&root).unwrap();
        let existing = root.join("file");
        std::fs::write(&existing, b"unchanged").unwrap();
        let content_ref = digest_hex(b"replacement");
        let entries = vec![TreeEntry {
            path: "file".into(),
            content_ref: content_ref.clone(),
            mode: 644,
        }];
        let bytes = BTreeMap::from([(content_ref, b"replacement".to_vec())]);
        assert_eq!(
            materialize(&root, &entries, &bytes).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(existing).unwrap(), b"unchanged");
    }
}

//! Lossless, span-based editing of the node section of a khive config file.
//!
//! Design: `.khive/inputs/SETUP-DESIGN.md` lines 245-259 (editing, crash and
//! concurrency contract) and 260-338 (refusals and exit codes). The file is
//! parsed with `toml_edit::ImDocument` so original spans are retained; only
//! named value spans, one empty `pins = []` statement and insertion points are
//! ever touched. Untouched bytes are copied from the original, never
//! re-serialized.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::ops::Range;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use khive_fs::atomic_create::create_only;
use khive_fs::atomic_publish::{stage_atomic_at_detailed, AtomicPublishOptions, StaleTmp};
use khive_fs::fd_relative::rename_at;
use khive_runtime::node_config::{NodeChannelConfig, NodePinConfig};
use khive_runtime::KhiveConfig;
use toml_edit::{ImDocument, Item, Table};

use super::NodeSetupError;

const CONFIG_LIMIT: u64 = 4_194_304;

const MSG_TOML: &str = "configuration is not valid TOML";
const MSG_SCHEMA: &str = "configuration does not satisfy the khive configuration schema";
const MSG_LAYOUT: &str = "node configuration must use normal tables and array-of-table pins";
const MSG_UNSAFE: &str =
    "configuration must be an owned regular file without links, extended attributes or an extended ACL";
const MSG_NO_DIR: &str = "the configuration directory does not exist";
const MSG_NO_FILE: &str = "the selected configuration file does not exist";
const MSG_OVERSIZE: &str = "configuration exceeds 4194304 bytes";
const MSG_GUARD: &str = "configuration edit would change unrelated content";
const MSG_BUSY: &str = "another node setup command is using this configuration";
const MSG_CHANGED: &str = "configuration changed during setup; run the command again";
const MSG_READ: &str = "could not read the selected file";
const MSG_SAVE: &str = "could not save configuration; the previous file remains in place";
const MSG_DURABILITY: &str =
    "configuration was replaced but durability could not be confirmed; run node status before retrying";
const MSG_OUT_EXISTS: &str = "the output file already exists";
const MSG_OUT_FAILED: &str = "could not write public output; use node status to export it again";
const MSG_RECOVER: &str = "recovery requires an incomplete disabled initialization";
const MSG_ASSIGN_FIRST: &str = "assign this node before using this command";

fn err(exit: u8, message: &str) -> NodeSetupError {
    NodeSetupError {
        exit,
        message: message.to_string(),
    }
}

fn layout_err() -> NodeSetupError {
    err(2, MSG_LAYOUT)
}

fn guard_err() -> NodeSetupError {
    err(2, MSG_GUARD)
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no arguments and cannot fail.
    unsafe { libc::geteuid() }
}

/// True when the open file carries at least one extended attribute.
fn has_xattrs(file: &File) -> bool {
    use std::os::fd::AsRawFd;
    #[cfg(target_os = "macos")]
    // SAFETY: a null buffer with size 0 only asks for the required length.
    let n = unsafe { libc::flistxattr(file.as_raw_fd(), std::ptr::null_mut(), 0, 0) };
    #[cfg(target_os = "linux")]
    // SAFETY: a null buffer with size 0 only asks for the required length.
    let n = unsafe { libc::flistxattr(file.as_raw_fd(), std::ptr::null_mut(), 0) };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let n = {
        let _ = file;
        0isize
    };
    n > 0
}

/// A parsed, validated configuration file and the exact bytes it came from.
pub(crate) struct ConfigSnapshot {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub config: KhiveConfig,
}

/// Identity of the observed target: device, inode and mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct FileId {
    dev: u64,
    ino: u64,
    mode: u32,
}

/// Holds the parent directory, the advisory lock, and the observed preimage.
pub(crate) struct ConfigGuard {
    dir: File,
    _lock: File,
    path: PathBuf,
    name: std::ffi::OsString,
    preimage: Vec<u8>,
    id: Option<FileId>,
}

/// One byte-range replacement against the original bytes.
pub(crate) struct TextEdit {
    pub range: Range<usize>,
    pub replacement: String,
}

/// The node-level edits setup may request.
pub(crate) enum NodeEdit {
    Prepare(NodeChannelConfig),
    Assign(NodeChannelConfig),
    AppendPin(NodePinConfig),
    RecoverFingerprint(String),
}

fn parent_of(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

fn parse_and_validate(bytes: &[u8]) -> Result<KhiveConfig, NodeSetupError> {
    let text = std::str::from_utf8(bytes).map_err(|_| err(2, MSG_TOML))?;
    ImDocument::parse(text).map_err(|_| err(2, MSG_TOML))?;
    let config: KhiveConfig = toml::from_str(text).map_err(|_| err(2, MSG_SCHEMA))?;
    config.validate().map_err(|e| err(2, &e.to_string()))?;
    Ok(config)
}

/// Reads the target with no-follow style checks. Returns the bytes and the
/// observed identity (None when the file is absent and `allow_missing`).
fn read_target(
    path: &Path,
    allow_missing: bool,
) -> Result<(Vec<u8>, Option<FileId>), NodeSetupError> {
    let lmeta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !parent_of(path).is_dir() {
                return Err(err(2, MSG_NO_DIR));
            }
            return if allow_missing {
                Ok((Vec::new(), None))
            } else {
                Err(err(2, MSG_NO_FILE))
            };
        }
        Err(_) => return Err(err(6, MSG_READ)),
    };
    if !lmeta.file_type().is_file() || lmeta.nlink() > 1 || lmeta.uid() != effective_uid() {
        return Err(err(2, MSG_UNSAFE));
    }
    let file = File::open(path).map_err(|_| err(6, MSG_READ))?;
    let meta = file.metadata().map_err(|_| err(6, MSG_READ))?;
    if meta.dev() != lmeta.dev() || meta.ino() != lmeta.ino() {
        return Err(err(6, MSG_CHANGED));
    }
    if has_xattrs(&file) {
        return Err(err(2, MSG_UNSAFE));
    }
    if meta.len() > CONFIG_LIMIT {
        return Err(err(2, MSG_OVERSIZE));
    }
    let mut bytes = Vec::new();
    (&file)
        .take(CONFIG_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| err(6, MSG_READ))?;
    if bytes.len() as u64 > CONFIG_LIMIT {
        return Err(err(2, MSG_OVERSIZE));
    }
    Ok((
        bytes,
        Some(FileId {
            dev: meta.dev(),
            ino: meta.ino(),
            mode: meta.mode(),
        }),
    ))
}

/// Reads, parses and fully validates the configuration at `path`.
pub(crate) fn read_config(
    path: &Path,
    allow_missing: bool,
) -> Result<ConfigSnapshot, NodeSetupError> {
    let (bytes, _) = read_target(path, allow_missing)?;
    let config = parse_and_validate(&bytes)?;
    Ok(ConfigSnapshot {
        path: path.to_path_buf(),
        bytes,
        config,
    })
}

/// Takes the sibling advisory lock and re-observes the preimage under it.
pub(crate) fn lock_config(snapshot: ConfigSnapshot) -> Result<ConfigGuard, NodeSetupError> {
    let path = snapshot.path;
    let parent = parent_of(&path);
    let dir = File::open(parent).map_err(|_| err(2, MSG_NO_DIR))?;
    if !dir.metadata().map_err(|_| err(2, MSG_NO_DIR))?.is_dir() {
        return Err(err(2, MSG_NO_DIR));
    }
    let name = path
        .file_name()
        .ok_or_else(|| err(2, MSG_NO_FILE))?
        .to_os_string();
    let lock_path = parent.join(format!("{}.node-setup.lock", name.to_string_lossy()));
    if let Ok(m) = fs::symlink_metadata(&lock_path) {
        if m.file_type().is_symlink() {
            return Err(err(2, MSG_UNSAFE));
        }
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|_| err(6, MSG_SAVE))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Err(err(6, MSG_BUSY)),
        Err(_) => return Err(err(6, MSG_SAVE)),
    }
    let (bytes, id) = read_target(&path, true)?;
    if bytes != snapshot.bytes {
        return Err(err(6, MSG_CHANGED));
    }
    Ok(ConfigGuard {
        dir,
        _lock: lock,
        path,
        name,
        preimage: bytes,
        id,
    })
}

/// The first observed newline convention; LF when the file has none.
fn newline(raw: &str) -> &'static str {
    match raw.find('\n') {
        Some(i) if i > 0 && raw.as_bytes()[i - 1] == b'\r' => "\r\n",
        _ => "\n",
    }
}

fn locate_node(root: &Table) -> Result<Option<&Table>, NodeSetupError> {
    let Some((_, channel)) = root.get_key_value("channel") else {
        return Ok(None);
    };
    let channel = channel.as_table().ok_or_else(layout_err)?;
    if channel.is_dotted() {
        return Err(layout_err());
    }
    let Some((_, node)) = channel.get_key_value("node") else {
        return Ok(None);
    };
    let node = node.as_table().ok_or_else(layout_err)?;
    if node.is_dotted() {
        return Err(layout_err());
    }
    // Pins must be array-of-table blocks for every edit, not only when a pin is
    // appended; an empty inline `pins = []` is the one inline form accepted.
    let inline_pins = node
        .get("pins")
        .and_then(Item::as_value)
        .and_then(|v| v.as_array())
        .is_some_and(|a| !a.is_empty());
    if inline_pins {
        return Err(layout_err());
    }
    Ok(Some(node))
}

fn is_scalar(item: &Item) -> bool {
    matches!(item.as_value(), Some(v) if !v.is_array() && !v.is_inline_table())
}

fn to_pairs(table: toml::Table) -> Vec<(String, String)> {
    table
        .iter()
        .filter(|(k, v)| {
            k.as_str() != "pins" && !matches!(v, toml::Value::Table(_) | toml::Value::Array(_))
        })
        .map(|(k, v)| (k.clone(), v.to_string()))
        .collect()
}

/// Position just after the header line that contains `start`, plus whether a
/// newline must be added first (header on the last line without newline).
fn after_header_line(raw: &str, start: usize) -> Result<(usize, bool), NodeSetupError> {
    let line_start = raw[..start].rfind('\n').map_or(0, |i| i + 1);
    let line = &raw[line_start..];
    let trimmed = line.trim_start();
    if !trimmed.starts_with('[') || trimmed.starts_with("[[") {
        return Err(layout_err());
    }
    Ok(match line.find('\n') {
        Some(i) => (line_start + i + 1, false),
        None => (raw.len(), true),
    })
}

fn eof_prefix(raw: &str, nl: &str) -> String {
    if raw.is_empty() {
        String::new()
    } else if raw.ends_with('\n') {
        nl.to_string()
    } else {
        format!("{nl}{nl}")
    }
}

fn set_members(
    raw: &str,
    node: Option<&Table>,
    members: &[(String, String)],
    nl: &str,
    edits: &mut Vec<TextEdit>,
) -> Result<(), NodeSetupError> {
    let Some(node) = node else {
        let mut text = eof_prefix(raw, nl);
        text.push_str(&format!("[channel.node]{nl}"));
        for (k, v) in members {
            text.push_str(&format!("{k} = {v}{nl}"));
        }
        edits.push(TextEdit {
            range: raw.len()..raw.len(),
            replacement: text,
        });
        return Ok(());
    };
    let mut missing = String::new();
    for (k, v) in members {
        match node.get_key_value(k) {
            Some((_, item)) => {
                if !is_scalar(item) {
                    return Err(layout_err());
                }
                let span = item.span().ok_or_else(layout_err)?;
                edits.push(TextEdit {
                    range: span,
                    replacement: v.clone(),
                });
            }
            None => missing.push_str(&format!("{k} = {v}{nl}")),
        }
    }
    if !missing.is_empty() {
        let span = node.span().ok_or_else(layout_err)?;
        let (pos, needs_nl) = after_header_line(raw, span.start)?;
        let replacement = if needs_nl {
            format!("{nl}{missing}")
        } else {
            missing
        };
        edits.push(TextEdit {
            range: pos..pos,
            replacement,
        });
    }
    Ok(())
}

fn append_pin(
    raw: &str,
    node: &Table,
    pin: &NodePinConfig,
    nl: &str,
    edits: &mut Vec<TextEdit>,
) -> Result<(), NodeSetupError> {
    if let Some((key, item)) = node.get_key_value("pins") {
        if item.as_array_of_tables().is_none() {
            let empty = item
                .as_value()
                .and_then(|v| v.as_array())
                .is_some_and(|a| a.is_empty());
            if !empty {
                return Err(layout_err());
            }
            let key_span = key.span().ok_or_else(layout_err)?;
            let value_span = item.span().ok_or_else(layout_err)?;
            let line_start = raw[..key_span.start].rfind('\n').map_or(0, |i| i + 1);
            if !raw[line_start..key_span.start].trim().is_empty() {
                return Err(layout_err());
            }
            let (rest_end, line_end) = match raw[value_span.end..].find('\n') {
                Some(i) => (value_span.end + i, value_span.end + i + 1),
                None => (raw.len(), raw.len()),
            };
            let rest = raw[value_span.end..rest_end].trim();
            let replacement = if rest.starts_with('#') {
                format!("{rest}{nl}")
            } else if rest.is_empty() {
                String::new()
            } else {
                return Err(layout_err());
            };
            edits.push(TextEdit {
                range: line_start..line_end,
                replacement,
            });
        }
    }
    let table = toml::Table::try_from(pin).map_err(|_| err(2, MSG_SCHEMA))?;
    let mut block = eof_prefix(raw, nl);
    block.push_str(&format!("[[channel.node.pins]]{nl}"));
    for (k, v) in to_pairs(table) {
        block.push_str(&format!("{k} = {v}{nl}"));
    }
    edits.push(TextEdit {
        range: raw.len()..raw.len(),
        replacement: block,
    });
    Ok(())
}

/// Plans the byte edits for `edit`, then reparses and validates the candidate.
pub(crate) fn plan_edit(
    snapshot: &ConfigSnapshot,
    edit: &NodeEdit,
) -> Result<Vec<TextEdit>, NodeSetupError> {
    let raw = std::str::from_utf8(&snapshot.bytes).map_err(|_| err(2, MSG_TOML))?;
    let doc = ImDocument::parse(raw).map_err(|_| err(2, MSG_TOML))?;
    let node = locate_node(doc.as_table())?;
    let nl = newline(raw);
    let mut edits = Vec::new();
    match edit {
        NodeEdit::Prepare(c) | NodeEdit::Assign(c) => {
            let pairs = to_pairs(toml::Table::try_from(c).map_err(|_| err(2, MSG_SCHEMA))?);
            set_members(raw, node, &pairs, nl, &mut edits)?;
        }
        NodeEdit::RecoverFingerprint(fp) => {
            if node.is_none() {
                return Err(err(2, MSG_RECOVER));
            }
            let pairs = vec![(
                "key_fingerprint".to_string(),
                toml::Value::String(fp.clone()).to_string(),
            )];
            set_members(raw, node, &pairs, nl, &mut edits)?;
        }
        NodeEdit::AppendPin(pin) => {
            let Some(node) = node else {
                return Err(err(2, MSG_ASSIGN_FIRST));
            };
            append_pin(raw, node, pin, nl, &mut edits)?;
        }
    }
    let candidate = apply_edits(&snapshot.bytes, &edits)?;
    parse_and_validate(&candidate).map_err(|e| {
        if e.message == MSG_TOML {
            guard_err()
        } else {
            e
        }
    })?;
    Ok(edits)
}

/// Applies non-overlapping edits, copying every untouched byte unchanged.
pub(crate) fn apply_edits(original: &[u8], edits: &[TextEdit]) -> Result<Vec<u8>, NodeSetupError> {
    let text = std::str::from_utf8(original).map_err(|_| guard_err())?;
    let mut order: Vec<&TextEdit> = edits.iter().collect();
    order.sort_by_key(|e| (e.range.start, e.range.end));
    let mut out = Vec::with_capacity(original.len());
    let mut cursor = 0usize;
    for e in order {
        let Range { start, end } = e.range.clone();
        if start > end
            || end > original.len()
            || start < cursor
            || !text.is_char_boundary(start)
            || !text.is_char_boundary(end)
        {
            return Err(guard_err());
        }
        out.extend_from_slice(&original[cursor..start]);
        out.extend_from_slice(e.replacement.as_bytes());
        cursor = end;
    }
    out.extend_from_slice(&original[cursor..]);
    Ok(out)
}

fn save_err<E>(_: E) -> NodeSetupError {
    err(6, MSG_SAVE)
}

/// Publishes `bytes` as the new configuration. Compares the observed preimage
/// again before and after staging; never deletes first and never copies.
pub(crate) fn publish_config(guard: &mut ConfigGuard, bytes: &[u8]) -> Result<(), NodeSetupError> {
    let (now, id) = read_target(&guard.path, true)?;
    if now != guard.preimage || id != guard.id {
        return Err(err(6, MSG_CHANGED));
    }
    match guard.id {
        None => match create_only(&guard.path, |f: &mut File| f.write_all(bytes)) {
            Ok(true) => {}
            Ok(false) => return Err(err(6, MSG_CHANGED)),
            Err(_) => return Err(err(6, MSG_SAVE)),
        },
        Some(orig) => {
            let tmp = format!(".node-setup-{}.tmp", uuid::Uuid::new_v4());
            let options = AtomicPublishOptions {
                stale: StaleTmp::Refuse,
                mode: orig.mode & 0o777,
            };
            stage_atomic_at_detailed(&guard.dir, OsStr::new(&tmp), options, |f: &mut File| {
                f.write_all(bytes)
            })
            .map_err(save_err)?;
            let (again, id2) = read_target(&guard.path, true)?;
            if again != guard.preimage || id2 != guard.id {
                return Err(err(6, MSG_CHANGED));
            }
            rename_at(&guard.dir, OsStr::new(&tmp), &guard.dir, &guard.name).map_err(save_err)?;
            guard.dir.sync_all().map_err(|_| err(6, MSG_DURABILITY))?;
        }
    }
    // The guard now describes the file this call wrote, so a second publish
    // under the same lock compares against that file, not the first preimage.
    let (written, written_id) = read_target(&guard.path, true)?;
    if written != bytes || written_id.is_none() {
        return Err(err(6, MSG_CHANGED));
    }
    guard.preimage = written;
    guard.id = written_id;
    Ok(())
}

/// Writes a public output file create-only; an existing file is never replaced.
pub(crate) fn save_public_file(path: &Path, bytes: &[u8]) -> Result<(), NodeSetupError> {
    match create_only(path, |f: &mut File| f.write_all(bytes)) {
        Ok(true) => Ok(()),
        Ok(false) => Err(err(6, MSG_OUT_EXISTS)),
        Err(_) => Err(err(6, MSG_OUT_FAILED)),
    }
}

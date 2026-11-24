//! The `[computer]` table (ADR-220): the one computer a caller's command
//! verbs run on, and the key that reaches it.
//!
//! The table is parsed and validated with the rest of the configuration file.
//! The key is never part of the parsed configuration: it is read from
//! `key_file` by [`read_computer_key`] only when a routed operation needs it.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use zeroize::Zeroizing;

use super::ConfigError;

/// Every pack a `[computer]` table may route.
pub const COMPUTER_ROUTABLE_PACKS: [&str; 7] = [
    "computer",
    "exec",
    "git",
    "code",
    "workspace",
    "blob",
    "tool",
];

/// The packs that run commands. With a `[computer]` table none of these
/// dispatches on the store's machine: a listed one is routed, an unlisted one
/// is refused with `runs_on_computer`.
pub const COMMAND_RUNNING_PACKS: [&str; 5] = ["exec", "git", "code", "workspace", "tool"];

/// `call_timeout_s` when the table does not set it.
pub const DEFAULT_COMPUTER_CALL_TIMEOUT_S: u64 = 3700;

/// The largest accepted `call_timeout_s`.
pub const MAX_COMPUTER_CALL_TIMEOUT_S: u64 = 7200;

/// The largest accepted key, in bytes, not counting a trailing newline.
pub const MAX_COMPUTER_KEY_BYTES: usize = 4096;

const MAX_COMPUTER_NAME_BYTES: usize = 64;

/// `[computer]`: which computer this caller's routed packs go to.
///
/// ```toml
/// [computer]
/// name = "build-1"
/// url = "http://127.0.0.1:8700/mcp"
/// key_file = "/home/example/.secrets/computer-build-1.key"
/// packs = ["computer", "exec", "git", "blob", "tool"]
/// call_timeout_s = 3700
/// ```
///
/// The table is closed. No value in it is a secret: `key_file` is a path, and
/// `Debug` prints it as one without reading the file.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ComputerSectionConfig {
    /// Labels the computer in the catalog and in refusals.
    pub name: String,
    /// Where the computer serves MCP over streamable HTTP.
    pub url: String,
    /// Absolute path of the file holding this caller's computer key.
    pub key_file: PathBuf,
    /// The packs served by the computer for this caller.
    pub packs: Vec<String>,
    /// How long a routed request may take, in seconds.
    #[serde(default = "default_call_timeout_s")]
    pub call_timeout_s: u64,
}

fn default_call_timeout_s() -> u64 {
    DEFAULT_COMPUTER_CALL_TIMEOUT_S
}

/// Where a validated `[computer].url` connects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputerEndpoint {
    /// `true` for `https`, `false` for plain `http` (loopback only).
    pub https: bool,
    /// The host as written, without the brackets of an IPv6 literal.
    pub host: String,
    /// The explicit port, or the scheme's default.
    pub port: u16,
    /// Whether the host is `localhost`, in `127.0.0.0/8`, or `::1`.
    pub loopback: bool,
}

impl ComputerSectionConfig {
    /// Check every rule of the table. Reads nothing from disk.
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_name(&self.name)?;
        self.endpoint()?;
        if !self.key_file.is_absolute() {
            return Err(invalid("key_file", "is not an absolute path"));
        }
        validate_packs(&self.packs)?;
        if self.call_timeout_s == 0 || self.call_timeout_s > MAX_COMPUTER_CALL_TIMEOUT_S {
            return Err(invalid(
                "call_timeout_s",
                format!(
                    "must be between 1 and {MAX_COMPUTER_CALL_TIMEOUT_S} seconds; got {}",
                    self.call_timeout_s
                ),
            ));
        }
        Ok(())
    }

    /// Parse `url` into the endpoint the client connects to.
    pub fn endpoint(&self) -> Result<ComputerEndpoint, ConfigError> {
        parse_computer_url(&self.url)
    }
}

/// ADR-220 section 2: `[actor].computer` names the computer of the
/// `[computer]` table. Each is refused without the other, and the two names
/// must be equal.
pub(super) fn validate_actor_computer(
    table: Option<&ComputerSectionConfig>,
    named: Option<&str>,
) -> Result<(), ConfigError> {
    let refusal = |reason: String| ConfigError::InvalidActorComputer { reason };
    match (table, named) {
        (None, None) => Ok(()),
        (Some(table), Some(named)) if table.name == named => Ok(()),
        (Some(table), Some(named)) => Err(refusal(format!(
            "is {named:?} but [computer].name is {:?}; the two must be equal",
            table.name
        ))),
        (Some(table), None) => Err(refusal(format!(
            "is required with a [computer] table; set it to {:?}",
            table.name
        ))),
        (None, Some(named)) => Err(refusal(format!(
            "is {named:?} but there is no [computer] table; remove the field or add the table"
        ))),
    }
}

fn invalid(key: &str, reason: impl Into<String>) -> ConfigError {
    ConfigError::InvalidComputerConfig {
        key: key.to_owned(),
        reason: reason.into(),
    }
}

pub(super) fn validate_name(name: &str) -> Result<(), ConfigError> {
    let well_formed = (1..=MAX_COMPUTER_NAME_BYTES).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if well_formed {
        Ok(())
    } else {
        Err(invalid(
            "name",
            format!(
                "must be 1 to {MAX_COMPUTER_NAME_BYTES} bytes of lowercase ASCII letters, digits and '-'"
            ),
        ))
    }
}

fn validate_packs(packs: &[String]) -> Result<(), ConfigError> {
    if packs.is_empty() {
        return Err(invalid("packs", "must name at least one pack"));
    }
    for (index, pack) in packs.iter().enumerate() {
        if !COMPUTER_ROUTABLE_PACKS.contains(&pack.as_str()) {
            return Err(invalid(
                "packs",
                format!(
                    "{pack:?} is not a routable pack; each entry is one of {}",
                    COMPUTER_ROUTABLE_PACKS.join(", ")
                ),
            ));
        }
        if packs[..index].contains(pack) {
            return Err(invalid("packs", format!("{pack:?} is listed twice")));
        }
    }
    Ok(())
}

/// Parse a `[computer].url`. A refusal never repeats the URL: it may hold a
/// user name or password, and a refusal names only the host when it must.
pub(super) fn parse_computer_url(url: &str) -> Result<ComputerEndpoint, ConfigError> {
    if !url.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(invalid(
            "url",
            "must be ASCII with no spaces or control characters",
        ));
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(invalid("url", "is not an absolute http or https URL"));
    };
    let https = if scheme.eq_ignore_ascii_case("https") {
        true
    } else if scheme.eq_ignore_ascii_case("http") {
        false
    } else {
        return Err(invalid("url", "must use the http or https scheme"));
    };
    let (authority, path) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
    if authority.contains('@') {
        return Err(invalid(
            "url",
            "carries a user name or password; the key is read only from key_file",
        ));
    }
    if !path.starts_with('/') {
        return Err(invalid("url", "must have a path, such as /mcp"));
    }
    if path.contains(['?', '#']) {
        return Err(invalid("url", "must not carry a query or a fragment"));
    }

    let (host, port_text) = if let Some(bracketed) = authority.strip_prefix('[') {
        let Some((host, tail)) = bracketed.split_once(']') else {
            return Err(invalid(
                "url",
                "has an IPv6 address with no closing bracket",
            ));
        };
        if host.parse::<Ipv6Addr>().is_err() {
            return Err(invalid(
                "url",
                "brackets a host that is not an IPv6 address",
            ));
        }
        let port_text = match tail {
            "" => None,
            tail => match tail.strip_prefix(':') {
                Some(port) => Some(port),
                None => return Err(invalid("url", "has text after its IPv6 address")),
            },
        };
        (host, port_text)
    } else {
        let (host, port_text) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        };
        if !host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'.')
        {
            return Err(invalid(
                "url",
                "has a host that is not a DNS name, an IPv4 address or a bracketed IPv6 address",
            ));
        }
        (host, port_text)
    };
    if host.is_empty() {
        return Err(invalid("url", "names no host"));
    }
    let port = match port_text {
        None if https => 443,
        None => 80,
        Some(text) => match text.parse::<u16>() {
            Ok(port) if port != 0 && text.bytes().all(|byte| byte.is_ascii_digit()) => port,
            _ => {
                return Err(invalid(
                    "url",
                    "has a port that is not a number from 1 to 65535",
                ))
            }
        },
    };
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<Ipv4Addr>()
            .is_ok_and(|address| address.is_loopback())
        || host
            .parse::<Ipv6Addr>()
            .is_ok_and(|address| address.is_loopback());
    if !https && !loopback {
        return Err(invalid(
            "url",
            format!(
                "plain http is accepted only for a loopback host (localhost, 127.0.0.0/8, ::1), \
                 because the key travels in a request header; host {host:?} requires https"
            ),
        ));
    }
    Ok(ComputerEndpoint {
        https,
        host: host.to_owned(),
        port,
        loopback,
    })
}

/// A computer key read from `[computer].key_file`.
///
/// The buffer is cleared on drop and `Debug` never prints it. The token is
/// visible ASCII, so it is always a valid `Authorization` header value.
pub struct ComputerKey(Zeroizing<String>);

impl ComputerKey {
    /// The token, for the `Authorization` header of requests to `url` only.
    pub fn token(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ComputerKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ComputerKey(<redacted>)")
    }
}

/// Why `[computer].key_file` could not supply a key. No variant carries the
/// path or any byte of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComputerKeyRefusal {
    /// The file could not be opened or read.
    Unreadable(std::io::ErrorKind),
    /// A symbolic link, a directory, a device or anything but a regular file.
    NotRegularFile,
    /// The file is owned by another user.
    NotOwnedByProcessUser,
    /// The file's mode grants the group or others any permission.
    GroupOrOtherPermissionBits { mode: u32 },
    /// More than [`MAX_COMPUTER_KEY_BYTES`] bytes before a trailing newline.
    TooLarge,
    /// Nothing but an optional trailing newline.
    Empty,
    /// Whitespace, control bytes or non-ASCII bytes in the token.
    NotOneToken,
    /// The platform offers no owner-only permission check.
    UnsupportedPlatform,
}

impl std::fmt::Display for ComputerKeyRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(kind) => write!(formatter, "cannot be read ({kind})"),
            Self::NotRegularFile => formatter.write_str(
                "is not a regular file; a symbolic link, a directory or a device is refused",
            ),
            Self::NotOwnedByProcessUser => {
                formatter.write_str("is not owned by the user running this process")
            }
            Self::GroupOrOtherPermissionBits { mode } => write!(
                formatter,
                "has group or other permission bits (mode {mode:04o}); it must be owner-only"
            ),
            Self::TooLarge => write!(
                formatter,
                "holds more than {MAX_COMPUTER_KEY_BYTES} bytes before a trailing newline"
            ),
            Self::Empty => formatter.write_str("holds no key"),
            Self::NotOneToken => formatter.write_str(
                "must hold one token of visible ASCII characters with no inner whitespace",
            ),
            Self::UnsupportedPlatform => {
                formatter.write_str("cannot be checked for owner-only permissions on this platform")
            }
        }
    }
}

/// The refusal behind `computer_key_unavailable`. Its message names the table
/// key, never the path and never the contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("computer_key_unavailable: [computer].key_file {refusal}")]
pub struct ComputerKeyError {
    pub refusal: ComputerKeyRefusal,
}

impl ComputerKeyError {
    /// The refusal kind a routed operation reports.
    pub const KIND: &'static str = "computer_key_unavailable";
}

/// Read the key from `key_file`.
///
/// The file must be a regular file, reached without following a symbolic
/// link, owned by this process's user, with no group or other permission bits,
/// holding one nonempty token of at most [`MAX_COMPUTER_KEY_BYTES`] visible
/// ASCII bytes. One trailing newline (`\n` or `\r\n`) is ignored.
pub fn read_computer_key(key_file: &Path) -> Result<ComputerKey, ComputerKeyError> {
    read_key(key_file).map_err(|refusal| ComputerKeyError { refusal })
}

#[cfg(unix)]
fn read_key(key_file: &Path) -> Result<ComputerKey, ComputerKeyRefusal> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(key_file)
        .map_err(|error| {
            if error.raw_os_error() == Some(libc::ELOOP) {
                ComputerKeyRefusal::NotRegularFile
            } else {
                ComputerKeyRefusal::Unreadable(error.kind())
            }
        })?;
    let metadata = file
        .metadata()
        .map_err(|error| ComputerKeyRefusal::Unreadable(error.kind()))?;
    if !metadata.file_type().is_file() {
        return Err(ComputerKeyRefusal::NotRegularFile);
    }
    // SAFETY: geteuid takes no arguments, has no preconditions and cannot fail.
    let process_user = unsafe { libc::geteuid() };
    if metadata.uid() != process_user {
        return Err(ComputerKeyRefusal::NotOwnedByProcessUser);
    }
    let mode = metadata.mode() & 0o7777;
    if mode & 0o077 != 0 {
        return Err(ComputerKeyRefusal::GroupOrOtherPermissionBits { mode });
    }

    // Room for the largest token, `\r\n` and one byte more: a file that fills it
    // is too large. The buffer never grows, so no copy of the key is left
    // behind unzeroed.
    let mut buffer = Zeroizing::new(vec![0u8; MAX_COMPUTER_KEY_BYTES + 3]);
    let mut filled = 0;
    loop {
        if filled == buffer.len() {
            return Err(ComputerKeyRefusal::TooLarge);
        }
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(ComputerKeyRefusal::Unreadable(error.kind())),
        }
    }
    token_from(&buffer[..filled])
}

#[cfg(not(unix))]
fn read_key(_key_file: &Path) -> Result<ComputerKey, ComputerKeyRefusal> {
    Err(ComputerKeyRefusal::UnsupportedPlatform)
}

fn token_from(content: &[u8]) -> Result<ComputerKey, ComputerKeyRefusal> {
    let token = match content.strip_suffix(b"\n") {
        Some(line) => line.strip_suffix(b"\r").unwrap_or(line),
        None => content,
    };
    if token.is_empty() {
        return Err(ComputerKeyRefusal::Empty);
    }
    if token.len() > MAX_COMPUTER_KEY_BYTES {
        return Err(ComputerKeyRefusal::TooLarge);
    }
    if !token.iter().all(u8::is_ascii_graphic) {
        return Err(ComputerKeyRefusal::NotOneToken);
    }
    let text = std::str::from_utf8(token).map_err(|_| ComputerKeyRefusal::NotOneToken)?;
    let mut owned = Zeroizing::new(String::with_capacity(text.len()));
    owned.push_str(text);
    Ok(ComputerKey(owned))
}

#[cfg(test)]
#[path = "engine_config_computer_tests.rs"]
mod tests;

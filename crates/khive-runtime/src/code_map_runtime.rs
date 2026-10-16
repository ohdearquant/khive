//! Dedicated code-map runtime construction.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use khive_db::StorageBackend;
use uuid::Uuid;

use crate::{CodeMapConfig, KhiveRuntime, RuntimeConfig, RuntimeError, RuntimeResult};

/// Device and inode recorded from an opened code-map file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    /// Return the device component of this identity.
    pub fn device(self) -> u64 {
        self.device
    }

    /// Return the inode component of this identity.
    pub fn inode(self) -> u64 {
        self.inode
    }

    fn from_metadata(metadata: &fs::Metadata) -> Result<Self, String> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            let device = metadata
                .volume_serial_number()
                .ok_or_else(|| "the file system did not report a device identity".to_string())?;
            let inode = metadata
                .file_index()
                .ok_or_else(|| "the file system did not report a file identity".to_string())?;
            Ok(Self {
                device: u64::from(device),
                inode,
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = metadata;
            Err("device and inode identity is unsupported on this platform".to_string())
        }
    }
}

/// A registration lifecycle value unique within one process run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RegistrationGeneration {
    boot_id: Uuid,
    counter: u64,
}

impl RegistrationGeneration {
    /// Return the identifier generated for the current process run.
    pub fn boot_id(self) -> Uuid {
        self.boot_id
    }

    /// Return the monotonic counter assigned to this registration.
    pub fn counter(self) -> u64 {
        self.counter
    }
}

impl std::fmt::Display for RegistrationGeneration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}:{}", self.boot_id, self.counter)
    }
}

/// A validated map declaration held by the runtime registry.
#[derive(Debug, Clone)]
pub struct RegisteredCodeMap {
    name: String,
    path: PathBuf,
    source_root: PathBuf,
    identity: FileIdentity,
    generation: RegistrationGeneration,
}

impl RegisteredCodeMap {
    /// Return the configured map name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Return the canonical database path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return the declared source tree path.
    pub fn source_root(&self) -> &Path {
        &self.source_root
    }

    /// Return the identity captured from the opened database file.
    pub fn identity(&self) -> FileIdentity {
        self.identity
    }

    /// Return the generation assigned when this entry was registered.
    pub fn generation(&self) -> RegistrationGeneration {
        self.generation
    }
}

/// Validated code maps loaded from daemon configuration.
#[derive(Debug, Default)]
pub struct CodeMapRegistry {
    entries: HashMap<String, RegisteredCodeMap>,
}

impl CodeMapRegistry {
    /// Load valid entries and log each rejected entry without aborting the load.
    ///
    /// `protected_databases` lists the shared production database paths whose
    /// main files and SQLite companions must remain outside the map surface.
    pub fn load(
        code_map_root: Option<&str>,
        entries: &[CodeMapConfig],
        protected_databases: &[PathBuf],
    ) -> Self {
        let root = canonical_root(code_map_root);
        if entries.is_empty() {
            if code_map_root.is_some() {
                if let Err(check) = root {
                    tracing::warn!(check = %check, "code_map_root is invalid");
                }
            }
            return Self::default();
        }

        let protected = collect_protected_identities(protected_databases);
        let mut name_counts = HashMap::<&str, usize>::new();
        for entry in entries {
            if let Some(name) = entry.name.as_deref() {
                *name_counts.entry(name).or_default() += 1;
            }
        }
        let duplicate_names: HashSet<&str> = name_counts
            .into_iter()
            .filter_map(|(name, count)| (count > 1).then_some(name))
            .collect();
        let mut registered = HashMap::new();

        for entry in entries {
            let result = match &root {
                Err(reason) => Err(reason.clone()),
                Ok(_)
                    if entry
                        .name
                        .as_deref()
                        .is_some_and(|name| name.trim().is_empty()) =>
                {
                    Err("name must not be empty".to_string())
                }
                Ok(_)
                    if entry
                        .name
                        .as_deref()
                        .is_some_and(|name| duplicate_names.contains(name)) =>
                {
                    Err("name is duplicated".to_string())
                }
                Ok(root) => match &protected {
                    Err(reason) => Err(reason.clone()),
                    Ok(protected) => validate_code_map(entry, root, protected),
                },
            };

            match result {
                Ok(map) => {
                    registered.insert(map.name.clone(), map);
                }
                Err(check) => {
                    tracing::warn!(
                        map = %entry.name.as_deref().unwrap_or("<missing>"),
                        check = %check,
                        "code map entry rejected during configuration load"
                    );
                }
            }
        }

        Self {
            entries: registered,
        }
    }

    /// Resolve a registered map name without exposing rejected-entry details.
    pub fn resolve(&self, name: &str) -> Result<&RegisteredCodeMap, RuntimeError> {
        self.entries
            .get(name)
            .ok_or_else(|| RuntimeError::InvalidInput("unknown code map".to_string()))
    }

    /// Return the number of registered maps.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return whether no maps were registered.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn load_for_runtime(
        config: &RuntimeConfig,
        runtime_db_path: Option<PathBuf>,
    ) -> Self {
        let mut production_databases = config.code_map_protected_database_paths.clone();
        if let Some(path) = crate::config::resolve_db_anchor(None) {
            production_databases.push(path);
        }
        if let Some(path) = &config.db_path {
            production_databases.push(path.clone());
        }
        if config.db_path.is_none() {
            if let Ok(path) = std::env::var("KHIVE_DB") {
                if !path.is_empty() {
                    production_databases.push(PathBuf::from(path));
                }
            }
        }
        if let Some(path) = runtime_db_path {
            production_databases.push(path);
        }
        production_databases.sort();
        production_databases.dedup();

        let mut protected_databases = Vec::with_capacity(production_databases.len() * 2);
        for path in production_databases {
            protected_databases.push(crate::events_split::events_db_path_beside(&path));
            protected_databases.push(path);
        }

        Self::load(
            config.code_map_root.as_deref(),
            &config.code_maps,
            &protected_databases,
        )
    }
}

fn canonical_root(root: Option<&str>) -> Result<PathBuf, String> {
    let root = root.ok_or_else(|| "code_map_root is missing".to_string())?;
    if has_uri_syntax(root) {
        return Err("code_map_root uses URI syntax".to_string());
    }
    let path = Path::new(root);
    if !path.is_absolute() {
        return Err("code_map_root must be absolute".to_string());
    }
    let canonical = fs::canonicalize(path)
        .map_err(|_| "code_map_root does not exist or cannot be canonicalized".to_string())?;
    if !canonical.is_dir() {
        return Err("code_map_root is not a directory".to_string());
    }
    Ok(canonical)
}

fn validate_code_map(
    entry: &CodeMapConfig,
    root: &Path,
    protected: &HashSet<FileIdentity>,
) -> Result<RegisteredCodeMap, String> {
    if let Some(field) = entry.unknown_fields.keys().next() {
        return Err(format!("unknown field {field}"));
    }
    let name = entry
        .name
        .as_deref()
        .ok_or_else(|| "missing name".to_string())?;
    let path = entry
        .path
        .as_deref()
        .ok_or_else(|| "missing path".to_string())?;
    let source_root = entry
        .source_root
        .as_deref()
        .ok_or_else(|| "missing source_root".to_string())?;

    let source_root_path = Path::new(source_root);
    if has_uri_syntax(source_root) {
        return Err("source_root uses URI syntax".to_string());
    }
    if !source_root_path.is_absolute() {
        return Err("source_root must be absolute".to_string());
    }

    if has_uri_syntax(path) {
        return Err("path uses URI syntax".to_string());
    }
    let configured_path = Path::new(path);
    if !configured_path.is_absolute() {
        return Err("path must be absolute".to_string());
    }
    let metadata = fs::symlink_metadata(configured_path)
        .map_err(|_| "path does not exist or cannot be inspected".to_string())?;
    if metadata.file_type().is_symlink() {
        return Err("path is a symlink".to_string());
    }
    if !metadata.file_type().is_file() {
        return Err("path is not a regular file".to_string());
    }
    let canonical_path = fs::canonicalize(configured_path)
        .map_err(|_| "path cannot be canonicalized".to_string())?;
    if !canonical_path.starts_with(root) {
        return Err("canonical path is outside code_map_root".to_string());
    }

    let file = open_identity_file(&canonical_path)
        .map_err(|_| "path could not be opened without following a symlink".to_string())?;
    let file_metadata = file
        .metadata()
        .map_err(|_| "opened path metadata could not be read".to_string())?;
    if !file_metadata.file_type().is_file() {
        return Err("opened path is not a regular file".to_string());
    }
    let identity = FileIdentity::from_metadata(&file_metadata)?;
    if protected.contains(&identity) {
        return Err("file identity matches a production database or companion".to_string());
    }
    drop(file);

    let generation = next_registration_generation()
        .ok_or_else(|| "registration generation counter is exhausted".to_string())?;
    Ok(RegisteredCodeMap {
        name: name.to_string(),
        path: canonical_path,
        source_root: source_root_path.to_path_buf(),
        identity,
        generation,
    })
}

fn collect_protected_identities(paths: &[PathBuf]) -> Result<HashSet<FileIdentity>, String> {
    let mut identities = HashSet::new();
    for path in paths {
        match fs::metadata(path) {
            Ok(metadata) => {
                identities.insert(FileIdentity::from_metadata(&metadata)?);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err("production database identity could not be inspected".to_string());
            }
        }
        for suffix in ["-journal", "-wal", "-shm"] {
            let companion = append_suffix(path, suffix);
            match fs::metadata(&companion) {
                Ok(metadata) => {
                    identities.insert(FileIdentity::from_metadata(&metadata)?);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    return Err(
                        "production database companion identity could not be inspected".to_string(),
                    );
                }
            }
        }
    }
    Ok(identities)
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn has_uri_syntax(value: &str) -> bool {
    let Some((scheme, _)) = value.split_once(':') else {
        return false;
    };
    if scheme.is_empty()
        || !scheme.as_bytes()[0].is_ascii_alphabetic()
        || !scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'.' | b'-'))
    {
        return false;
    }
    #[cfg(windows)]
    if scheme.len() == 1 {
        if value
            .as_bytes()
            .get(2)
            .is_some_and(|byte| matches!(byte, b'/' | b'\\'))
        {
            return false;
        }
    }
    true
}

fn open_identity_file(path: &Path) -> std::io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        File::open(path)
    }
}

fn next_registration_generation() -> Option<RegistrationGeneration> {
    static BOOT_ID: OnceLock<Uuid> = OnceLock::new();
    static NEXT_COUNTER: AtomicU64 = AtomicU64::new(1);
    let counter = NEXT_COUNTER
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .ok()?;
    Some(RegistrationGeneration {
        boot_id: *BOOT_ID.get_or_init(Uuid::new_v4),
        counter,
    })
}

impl KhiveRuntime {
    /// Construct a dedicated code-map runtime. Every connection in its pool
    /// selects the native handle-proving VFS before the first schema read or
    /// migration. The caller prepares the omitted-db parent; an explicit
    /// target receives no path transform or directory creation here, so its
    /// parent directory must already exist. A code-map database runs in
    /// rollback-journal mode, so a configured WAL ceiling does not apply to it.
    pub fn new_code_map(
        mut config: RuntimeConfig,
        protected_main: Vec<std::path::PathBuf>,
        protected_events: Vec<std::path::PathBuf>,
    ) -> RuntimeResult<Self> {
        if config.db_path.is_none() {
            return Err(RuntimeError::InvalidInput(
                "code-map runtime requires a guarded file-backed database".into(),
            ));
        }
        let disk_guard = config
            .resolve_disk_guard_policy(false)?
            .expect("code-map has a file-backed disk policy");
        let volume_lock_dir = config.volume_lock_dir.clone();
        Self::new_with_file_backend(config, false, move |path| {
            StorageBackend::sqlite_code_map_with_policies(
                path,
                &protected_main,
                &protected_events,
                disk_guard,
                khive_db::require_volume_lock_dir(volume_lock_dir)?,
            )
        })
    }
}

#[cfg(test)]
#[path = "code_map_runtime_tests.rs"]
mod tests;

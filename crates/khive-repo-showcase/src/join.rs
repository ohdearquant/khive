use std::fs;
use std::io::Read;
use std::path::Component;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::ExportError;

const MAX_CARGO_MANIFEST_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RustModuleKey {
    pub(crate) source_project: String,
    pub(crate) module_path: String,
    pub(crate) source_path: String,
}

#[derive(Debug, Clone)]
struct CargoProject {
    root: PathBuf,
    name: String,
}

pub(crate) fn natural_id(kind: &str, components: &[&str]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"khive.repo.v1\0");
    digest.update(kind.as_bytes());
    for component in components {
        digest.update(b"\0");
        digest.update(component.as_bytes());
    }
    format!("khive:{kind}:sha256:{}", hex::encode(digest.finalize()))
}

pub(crate) fn git_output(repo: &Path, args: &[&str]) -> Result<Vec<u8>, ExportError> {
    let output = crate::git_safety::hardened_git_command_for_repo(repo)
        .map_err(|source| ExportError::GitSpawn {
            args: "config --get-regexp ^filter\\.".into(),
            source,
        })?
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|source| ExportError::GitSpawn {
            args: args.join(" "),
            source,
        })?;
    if !output.status.success() {
        return Err(ExportError::Git {
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(output.stdout)
}

pub(crate) fn git_text(repo: &Path, args: &[&str]) -> Result<String, ExportError> {
    String::from_utf8(git_output(repo, args)?)
        .map(|value| value.trim().to_string())
        .map_err(|_| {
            ExportError::InvalidData(format!("git {} returned non-UTF-8 text", args.join(" ")))
        })
}

pub(crate) fn head_sha(repo: &Path) -> Result<String, ExportError> {
    let value = git_text(repo, &["rev-parse", "--verify", "HEAD"])?;
    if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ExportError::InvalidData(format!(
            "HEAD must be a 40-character hexadecimal commit id, got {value:?}"
        )));
    }
    Ok(value.to_ascii_lowercase())
}

pub(crate) fn head_committed_at(repo: &Path) -> Result<String, ExportError> {
    git_text(repo, &["show", "-s", "--format=%cI", "HEAD"])
}

pub(crate) fn tracked_paths(repo: &Path, head: &str) -> Result<Vec<String>, ExportError> {
    let mut paths = decode_nul_paths(
        git_output(repo, &["ls-tree", "-r", "-z", "--name-only", head])?,
        "tracked repository path",
    )?;
    paths.sort();
    paths.dedup();
    Ok(paths)
}

pub(crate) fn changed_paths_fallback(repo: &Path, sha: &str) -> Result<Vec<String>, ExportError> {
    let bytes = git_output(
        repo,
        &[
            "diff-tree",
            "--root",
            "--no-commit-id",
            "--name-only",
            "--no-renames",
            "--diff-merges=first-parent",
            "-r",
            "-z",
            sha,
        ],
    )?;
    let mut paths = decode_nul_paths(bytes, "changed repository path")?;
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn decode_nul_paths(bytes: Vec<u8>, label: &str) -> Result<Vec<String>, ExportError> {
    let text = String::from_utf8(bytes)
        .map_err(|_| ExportError::InvalidData(format!("{label} is not valid UTF-8")))?;
    Ok(text
        .split_terminator('\0')
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect())
}

pub(crate) fn release_tags(
    repo: &Path,
) -> Result<Vec<(String, String, Option<String>)>, ExportError> {
    let bytes = git_output(
        repo,
        &[
            "for-each-ref",
            "--sort=refname",
            "--format=%(refname:strip=2)%00%(if)%(*objectname)%(then)%(*objectname)%(else)%(objectname)%(end)%00%(objecttype)%00%(*objecttype)%00%(if)%(*committerdate)%(then)%(*committerdate:iso-strict)%(else)%(committerdate:iso-strict)%(end)",
            "refs/tags",
        ],
    )?;
    let text = String::from_utf8(bytes)
        .map_err(|_| ExportError::InvalidData("git tag output is not valid UTF-8".into()))?;
    let mut tags = Vec::new();
    for line in text.split_terminator('\n').filter(|line| !line.is_empty()) {
        let fields = line.split('\0').map(str::to_string).collect::<Vec<_>>();
        let [name, sha, object_type, peeled_type, date] = fields.as_slice() else {
            return Err(ExportError::InvalidData(
                "git returned a malformed tag record".into(),
            ));
        };
        let target_type = if peeled_type.is_empty() {
            object_type
        } else {
            peeled_type
        };
        if target_type != "commit" {
            return Err(ExportError::InvalidData(format!(
                "tag {name:?} targets a {target_type} object rather than a commit"
            )));
        }
        if sha.len() != 40 || !khive_types::is_lowercase_hex(sha) {
            return Err(ExportError::InvalidData(format!(
                "tag {name:?} has invalid commit target {sha:?}"
            )));
        }
        tags.push((
            name.clone(),
            sha.clone(),
            (!date.is_empty()).then(|| date.clone()),
        ));
    }
    tags.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(tags)
}

fn discover_cargo_projects(
    repo: &Path,
    tracked_paths: &[String],
) -> Result<Vec<CargoProject>, ExportError> {
    let root = fs::canonicalize(repo).map_err(|source| ExportError::Io {
        path: repo.to_path_buf(),
        source,
    })?;
    let mut projects = Vec::new();
    for raw_path in tracked_paths.iter().filter(|path| {
        Path::new(path)
            .file_name()
            .is_some_and(|name| name == "Cargo.toml")
    }) {
        let relative = Path::new(raw_path);
        if relative.is_absolute()
            || !relative
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
        {
            return Err(ExportError::InvalidData(format!(
                "Cargo manifest path {raw_path:?} is not a safe repository-relative path"
            )));
        }
        let path = root.join(relative);
        let metadata = fs::symlink_metadata(&path).map_err(|source| ExportError::Io {
            path: path.clone(),
            source,
        })?;
        if metadata.file_type().is_symlink() {
            return Err(ExportError::InvalidData(format!(
                "Cargo manifest {raw_path:?} must not be a symlink"
            )));
        }
        if !metadata.file_type().is_file() {
            return Err(ExportError::InvalidData(format!(
                "Cargo manifest {raw_path:?} must be a regular file"
            )));
        }
        if metadata.len() > MAX_CARGO_MANIFEST_BYTES {
            return Err(ExportError::InvalidData(format!(
                "Cargo manifest {raw_path:?} is {} bytes, exceeding the {MAX_CARGO_MANIFEST_BYTES}-byte limit",
                metadata.len()
            )));
        }
        let canonical = fs::canonicalize(&path).map_err(|source| ExportError::Io {
            path: path.clone(),
            source,
        })?;
        if !canonical.starts_with(&root) {
            return Err(ExportError::InvalidData(format!(
                "Cargo manifest {raw_path:?} resolves outside the repository root"
            )));
        }
        let mut file = fs::File::open(&canonical).map_err(|source| ExportError::Io {
            path: canonical.clone(),
            source,
        })?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.by_ref()
            .take(MAX_CARGO_MANIFEST_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|source| ExportError::Io {
                path: canonical.clone(),
                source,
            })?;
        if bytes.len() as u64 > MAX_CARGO_MANIFEST_BYTES {
            return Err(ExportError::InvalidData(format!(
                "Cargo manifest {raw_path:?} grew beyond the {MAX_CARGO_MANIFEST_BYTES}-byte limit while being read"
            )));
        }
        let text = String::from_utf8(bytes).map_err(|_| {
            ExportError::InvalidData(format!("Cargo manifest {raw_path:?} is not valid UTF-8"))
        })?;
        let value: toml::Value = text.parse().map_err(|source| ExportError::CargoManifest {
            path: canonical.clone(),
            source,
        })?;
        if let Some(name) = value
            .get("package")
            .and_then(|package| package.get("name"))
            .and_then(toml::Value::as_str)
        {
            let manifest_dir = canonical.parent().expect("Cargo.toml has a parent");
            let relative_root = manifest_dir
                .strip_prefix(&root)
                .expect("manifest containment was validated")
                .to_path_buf();
            projects.push(CargoProject {
                root: relative_root,
                name: name.to_string(),
            });
        }
    }
    projects.sort_by(|left, right| {
        right
            .root
            .components()
            .count()
            .cmp(&left.root.components().count())
            .then_with(|| left.root.cmp(&right.root))
    });
    Ok(projects)
}

pub(crate) fn derive_rust_module_keys(
    repo: &Path,
    paths: &[String],
) -> Result<Vec<(String, Option<RustModuleKey>, String)>, ExportError> {
    let projects = discover_cargo_projects(repo, paths)?;
    let mut derived = Vec::new();
    for source_path in paths.iter().filter(|path| path.ends_with(".rs")) {
        let path = Path::new(source_path);
        let Some(project) = projects
            .iter()
            .find(|project| path.starts_with(&project.root))
        else {
            derived.push((
                source_path.clone(),
                None,
                "no governing Cargo.toml with a package name".into(),
            ));
            continue;
        };
        let rel = path.strip_prefix(&project.root).unwrap_or(path);
        let mut components = rel
            .components()
            .filter_map(|component| component.as_os_str().to_str().map(str::to_string))
            .collect::<Vec<_>>();
        if components.first().map(String::as_str) == Some("src") {
            components.remove(0);
        }
        let Some(last) = components.last_mut() else {
            derived.push((
                source_path.clone(),
                None,
                "empty path under crate root".into(),
            ));
            continue;
        };
        let Some(stem) = last.strip_suffix(".rs").map(str::to_string) else {
            derived.push((source_path.clone(), None, "not a Rust source file".into()));
            continue;
        };
        *last = stem.clone();
        let module_path = if components.len() == 1 && stem == "lib" {
            "crate".to_string()
        } else if components.len() == 1 && stem == "main" {
            "crate::main".to_string()
        } else {
            if stem == "mod" {
                components.pop();
            }
            if components.is_empty() {
                "crate".to_string()
            } else {
                components.join("::")
            }
        };
        derived.push((
            source_path.clone(),
            Some(RustModuleKey {
                source_project: project.name.clone(),
                module_path,
                source_path: source_path.clone(),
            }),
            String::new(),
        ));
    }
    derived.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(derived)
}

#[cfg(test)]
#[path = "join_tests.rs"]
mod tests;

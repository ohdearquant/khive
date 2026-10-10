//! Initializer output must be discoverable without replacing project settings.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use khive_runtime::engine_config::KhiveConfig;
use tempfile::TempDir;

const OLD_IGNORE: &[u8] = b"*\n!.gitignore\n!kg/\n!kg/**\nkg/.remote-cache/\nkg/.remote-cache/**\nkg/remotes/\n!khive.toml\n";
const NEW_IGNORE: &[u8] = b"*\n!.gitignore\n!kg/\n!kg/**\nkg/.remote-cache/\nkg/.remote-cache/**\nkg/remotes/\n!config.toml\n";

struct Fixture {
    root: TempDir,
    repo: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let home = root.path().join("home");
        std::fs::create_dir(&repo).unwrap();
        std::fs::create_dir(&home).unwrap();
        Self { root, repo, home }
    }

    fn write(&self, relative: &str, bytes: impl AsRef<[u8]>) {
        let path = self.repo.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn init(&self, extra: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kkernel"));
        for (key, _) in std::env::vars_os() {
            if key
                .to_str()
                .is_some_and(|key| key.starts_with("KHIVE_") && key != "KHIVE_TEST_HARNESS")
            {
                command.env_remove(key);
            }
        }
        let output = command
            .current_dir(&self.repo)
            .env("HOME", &self.home)
            .env(
                "KHIVE_VOLUME_LOCK_DIR",
                self.root.path().join("volume-locks"),
            )
            .args(["kg", "init", "--repo"])
            .arg(&self.repo)
            .args(extra)
            .output()
            .unwrap();
        assert!(
            std::fs::read_dir(&self.home).unwrap().next().is_none(),
            "initialization must not open a home database or model cache"
        );
        output
    }
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Directory,
    File(Vec<u8>),
    Link(PathBuf),
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Entry> {
    fn visit(root: &Path, path: &Path, entries: &mut BTreeMap<PathBuf, Entry>) {
        for child in std::fs::read_dir(path).unwrap() {
            let child = child.unwrap().path();
            let relative = child.strip_prefix(root).unwrap().to_path_buf();
            let metadata = std::fs::symlink_metadata(&child).unwrap();
            if metadata.file_type().is_symlink() {
                entries.insert(relative, Entry::Link(std::fs::read_link(child).unwrap()));
            } else if metadata.is_dir() {
                entries.insert(relative, Entry::Directory);
                visit(root, &child, entries);
            } else {
                entries.insert(relative, Entry::File(std::fs::read(child).unwrap()));
            }
        }
    }
    let mut entries = BTreeMap::new();
    visit(root, root, &mut entries);
    entries
}

#[test]
fn fresh_init_emits_a_minimal_loader_valid_canonical_config() {
    let fixture = Fixture::new();
    let output = fixture.init(&[]);
    success(&output);
    let path = fixture.repo.join(".khive/config.toml");
    let config = KhiveConfig::load(Some(&path)).unwrap().unwrap();
    assert_eq!(config.engines.len(), 1);
    // Check the emitted wire shape without coupling this fixture to the
    // runtime's legacy-versus-ordered in-memory EngineConfig representation.
    let value: toml::Value = std::fs::read_to_string(&path).unwrap().parse().unwrap();
    assert_eq!(value.as_table().unwrap().len(), 1);
    let engines = value["engines"].as_array().unwrap();
    assert_eq!(engines.len(), 1);
    let engine = engines[0].as_table().unwrap();
    assert_eq!(engine.len(), 4);
    assert_eq!(engine["name"].as_str(), Some("default"));
    assert_eq!(engine["model"].as_str(), Some("all-minilm-l6-v2"));
    assert_eq!(engine["default"].as_bool(), Some(true));
    assert_eq!(engine["dims"].as_integer(), Some(384));
    assert!(!fixture.repo.join(".khive/khive.toml").exists());
    assert_eq!(
        std::fs::read(fixture.repo.join(".khive/.gitignore")).unwrap(),
        NEW_IGNORE
    );
    for relative in [
        ".khive/kg/entities.ndjson",
        ".khive/kg/edges.ndjson",
        ".khive/kg/hooks/pre-commit",
    ] {
        assert!(fixture.repo.join(relative).is_file(), "{relative}");
    }
    assert!(String::from_utf8_lossy(&output.stdout).contains("config.toml"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(fixture.repo.join(".khive/kg/hooks/pre-commit"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }
}

#[test]
fn repeated_init_preserves_config_exports_hook_and_custom_ignore_bytes() {
    let fixture = Fixture::new();
    success(&fixture.init(&[]));
    for (path, bytes) in [
        (".khive/config.toml", b"# custom config\n".as_slice()),
        (
            ".khive/kg/entities.ndjson",
            b"existing entities\n".as_slice(),
        ),
        (".khive/kg/edges.ndjson", b"existing edges\n".as_slice()),
        (
            ".khive/kg/hooks/pre-commit",
            b"#!/bin/sh\nexit 0\n".as_slice(),
        ),
        (
            ".khive/.gitignore",
            b"# custom ignore\nprivate/\n".as_slice(),
        ),
    ] {
        fixture.write(path, bytes);
    }
    let before = snapshot(&fixture.repo);
    success(&fixture.init(&[]));
    assert_eq!(snapshot(&fixture.repo), before);
}

#[test]
fn preexisting_root_and_canonical_configs_are_preserved_without_legacy_siblings() {
    for (root, canonical) in [(true, false), (false, true), (true, true)] {
        let fixture = Fixture::new();
        if root {
            fixture.write("khive.toml", b"# root owner\n");
        }
        if canonical {
            fixture.write(".khive/config.toml", b"# hidden owner\n");
        }
        success(&fixture.init(&[]));
        if root {
            assert_eq!(
                std::fs::read(fixture.repo.join("khive.toml")).unwrap(),
                b"# root owner\n"
            );
        }
        if canonical {
            assert_eq!(
                std::fs::read(fixture.repo.join(".khive/config.toml")).unwrap(),
                b"# hidden owner\n"
            );
        } else {
            assert!(!fixture.repo.join(".khive/config.toml").exists());
        }
        assert!(!fixture.repo.join(".khive/khive.toml").exists());
        assert!(fixture.repo.join(".khive/kg/entities.ndjson").is_file());
    }
}

#[test]
fn legacy_config_refusal_precedes_every_scaffold_write() {
    for (canonical, root) in [(false, false), (true, false), (false, true), (true, true)] {
        let fixture = Fixture::new();
        fixture.write(".khive/khive.toml", b"# legacy owner\n");
        if canonical {
            fixture.write(".khive/config.toml", b"# canonical owner\n");
        }
        if root {
            fixture.write("khive.toml", b"# root owner\n");
        }
        let before = snapshot(&fixture.repo);
        let output = fixture.init(&["--ci"]);
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(".khive/khive.toml"), "{stderr}");
        assert!(stderr.contains(".khive/config.toml"), "{stderr}");
        assert_eq!(snapshot(&fixture.repo), before);
    }
}

#[test]
fn only_exact_old_generated_ignore_bytes_are_migrated() {
    for (input, expected) in [
        (OLD_IGNORE.to_vec(), NEW_IGNORE.to_vec()),
        (NEW_IGNORE.to_vec(), NEW_IGNORE.to_vec()),
        (
            [OLD_IGNORE, b"# local changes\n"].concat(),
            [OLD_IGNORE, b"# local changes\n"].concat(),
        ),
        (
            String::from_utf8_lossy(OLD_IGNORE)
                .replace('\n', "\r\n")
                .into_bytes(),
            String::from_utf8_lossy(OLD_IGNORE)
                .replace('\n', "\r\n")
                .into_bytes(),
        ),
        (
            b"# non-UTF8 owner\n\xff\n".to_vec(),
            b"# non-UTF8 owner\n\xff\n".to_vec(),
        ),
    ] {
        let fixture = Fixture::new();
        fixture.write(".khive/.gitignore", input);
        success(&fixture.init(&[]));
        assert_eq!(
            std::fs::read(fixture.repo.join(".khive/.gitignore")).unwrap(),
            expected
        );
    }
}

#[test]
fn config_directory_collisions_fail_before_scaffolding() {
    for path in ["khive.toml", ".khive/config.toml", ".khive/khive.toml"] {
        let fixture = Fixture::new();
        std::fs::create_dir_all(fixture.repo.join(path)).unwrap();
        let before = snapshot(&fixture.repo);
        let output = fixture.init(&["--ci"]);
        assert!(!output.status.success(), "{path}");
        assert_eq!(snapshot(&fixture.repo), before);
    }
}

#[test]
fn ci_scaffolding_remains_idempotent() {
    let fixture = Fixture::new();
    success(&fixture.init(&["--ci"]));
    let workflow = fixture.repo.join(".github/workflows/kg-validate.yml");
    assert!(std::fs::read_to_string(&workflow)
        .unwrap()
        .contains("kkernel kg validate --format github"));
    fixture.write(
        ".github/workflows/kg-validate.yml",
        b"# operator workflow\n",
    );
    let before = snapshot(&fixture.repo);
    success(&fixture.init(&["--ci"]));
    assert_eq!(snapshot(&fixture.repo), before);
}

#[cfg(unix)]
#[test]
fn dangling_config_links_are_not_followed_or_replaced() {
    for path in ["khive.toml", ".khive/config.toml", ".khive/khive.toml"] {
        let fixture = Fixture::new();
        let link = fixture.repo.join(path);
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        let missing = fixture.root.path().join("missing-config");
        std::os::unix::fs::symlink(&missing, &link).unwrap();
        let before = snapshot(&fixture.repo);
        let output = fixture.init(&["--ci"]);
        assert!(!output.status.success(), "{path}");
        assert_eq!(snapshot(&fixture.repo), before);
        assert!(!missing.exists());
        if path.ends_with(".khive/khive.toml") {
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains(".khive/config.toml"), "{stderr}");
        }
    }
}

#[cfg(unix)]
#[test]
fn existing_config_and_ignore_symlink_targets_are_preserved() {
    for root in [false, true] {
        let fixture = Fixture::new();
        let target = fixture.root.path().join("shared-config");
        std::fs::write(&target, b"# shared config\n").unwrap();
        let config = fixture.repo.join(if root {
            "khive.toml"
        } else {
            ".khive/config.toml"
        });
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &config).unwrap();
        let ignore_target = fixture.root.path().join("shared-ignore");
        std::fs::write(&ignore_target, OLD_IGNORE).unwrap();
        std::fs::create_dir_all(fixture.repo.join(".khive")).unwrap();
        let ignore = fixture.repo.join(".khive/.gitignore");
        std::os::unix::fs::symlink(&ignore_target, &ignore).unwrap();
        success(&fixture.init(&[]));
        assert_eq!(std::fs::read(&target).unwrap(), b"# shared config\n");
        assert_eq!(std::fs::read(&ignore_target).unwrap(), OLD_IGNORE);
        assert_eq!(std::fs::read_link(config).unwrap(), target);
        assert_eq!(std::fs::read_link(ignore).unwrap(), ignore_target);
        if root {
            assert!(!fixture.repo.join(".khive/config.toml").exists());
        }
    }
}

#[cfg(unix)]
#[test]
fn add_hooks_remains_independent_of_config_initialization() {
    let fixture = Fixture::new();
    fixture.write(".khive/khive.toml", b"# legacy config\n");
    fixture.write(".khive/kg/hooks/pre-commit", b"#!/bin/sh\nexit 0\n");
    success(&fixture.init(&["--add-hooks"]));
    assert_eq!(
        std::fs::read_link(fixture.repo.join(".git/hooks/pre-commit")).unwrap(),
        fixture
            .repo
            .join(".khive/kg/hooks/pre-commit")
            .canonicalize()
            .unwrap()
    );
    assert!(!fixture.repo.join(".khive/config.toml").exists());
    assert!(!fixture.repo.join(".khive/kg/entities.ndjson").exists());
    assert_eq!(
        std::fs::read(fixture.repo.join(".khive/khive.toml")).unwrap(),
        b"# legacy config\n"
    );
}

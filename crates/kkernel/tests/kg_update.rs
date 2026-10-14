use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{json, Value};
use tempfile::TempDir;

fn isolated(command: &mut Command, root: &Path) {
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_some_and(|name| {
            name.starts_with("GIT_") || (name.starts_with("KHIVE_") && name != "KHIVE_TEST_HARNESS")
        }) {
            command.env_remove(name);
        }
    }
    command
        .env("HOME", root.join("home"))
        .env("USERPROFILE", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("home/config"))
        .env("KHIVE_VOLUME_LOCK_DIR", root.join("volume-locks"))
        .env("KHIVE_NO_DAEMON", "1")
        .env("KHIVE_DB", root.join("must-not-open.db"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", root.join("empty-git-config"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_DATE", "2026-10-09T12:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-10-09T12:00:00Z")
        .env("TMPDIR", root.join("scratch"))
        .env("TMP", root.join("scratch"))
        .env("TEMP", root.join("scratch"));
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git(root: &Path, repo: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    isolated(&mut command, root);
    let output = command
        .arg("-C")
        .arg(repo)
        .arg("-c")
        .arg(format!(
            "core.hooksPath={}",
            root.join("no-hooks").display()
        ))
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "gc.auto=0",
        ])
        .args(args)
        .output()
        .expect("run fixture Git");
    success(&output);
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Directory,
    File(Vec<u8>),
    Symlink(PathBuf),
}

fn snapshot(root: &Path, excluded: &[&str]) -> BTreeMap<PathBuf, Entry> {
    fn visit(root: &Path, path: &Path, excluded: &[&str], entries: &mut BTreeMap<PathBuf, Entry>) {
        for child in fs::read_dir(path).unwrap() {
            let child = child.unwrap().path();
            let relative = child.strip_prefix(root).unwrap().to_owned();
            if excluded.iter().any(|prefix| relative.starts_with(prefix)) {
                continue;
            }
            let kind = fs::symlink_metadata(&child).unwrap().file_type();
            let entry = if kind.is_symlink() {
                Entry::Symlink(fs::read_link(&child).unwrap())
            } else if kind.is_dir() {
                visit(root, &child, excluded, entries);
                Entry::Directory
            } else {
                assert!(kind.is_file(), "unexpected fixture entry {child:?}");
                Entry::File(fs::read(&child).unwrap())
            };
            entries.insert(relative, entry);
        }
    }
    let mut entries = BTreeMap::new();
    visit(root, root, excluded, &mut entries);
    entries
}

struct Fixture {
    root: TempDir,
    project: PathBuf,
    remote: PathBuf,
    first: String,
    second: String,
    third: String,
    annotated_tag: String,
}

impl Fixture {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        for directory in ["home", "scratch", "no-hooks", "source", "project/.khive/kg"] {
            fs::create_dir_all(root.path().join(directory)).unwrap();
        }
        fs::write(root.path().join("empty-git-config"), "").unwrap();
        let source = root.path().join("source");
        git(
            root.path(),
            &source,
            &["init", "-q", "-b", "upstream-default"],
        );
        git(
            root.path(),
            &source,
            &["config", "user.name", "KG Update Fixture"],
        );
        git(
            root.path(),
            &source,
            &["config", "user.email", "fixture@example.invalid"],
        );
        let mut commits = Vec::new();
        for (content, tag) in [("first", "v1"), ("second", "v2"), ("third", "v3")] {
            fs::write(source.join("record.txt"), content).unwrap();
            git(root.path(), &source, &["add", "--", "record.txt"]);
            git(root.path(), &source, &["commit", "-qm", content]);
            let commit = git(root.path(), &source, &["rev-parse", "HEAD"]);
            git(root.path(), &source, &["tag", tag, &commit]);
            commits.push(commit);
        }
        git(
            root.path(),
            &source,
            &[
                "tag",
                "-a",
                "v2-annotated",
                "-m",
                "annotated v2",
                &commits[1],
            ],
        );
        let annotated_tag = git(root.path(), &source, &["rev-parse", "v2-annotated"]);
        assert_ne!(annotated_tag, commits[1]);
        let tree = git(root.path(), &source, &["rev-parse", "HEAD^{tree}"]);
        let blob = git(root.path(), &source, &["rev-parse", "HEAD:record.txt"]);
        git(root.path(), &source, &["tag", "tree-only", &tree]);
        git(root.path(), &source, &["tag", "blob-only", &blob]);
        let remote = root.path().join("remote with spaces.git");
        git(
            root.path(),
            root.path(),
            &[
                "clone",
                "--quiet",
                "--bare",
                source.to_str().unwrap(),
                remote.to_str().unwrap(),
            ],
        );
        assert_eq!(
            git(root.path(), &remote, &["symbolic-ref", "HEAD"]),
            "refs/heads/upstream-default"
        );
        let project = root.path().join("project");
        for (path, body) in [
            (".khive/kg/entities.ndjson", b"entity sentinel\n".as_slice()),
            (".khive/kg/edges.ndjson", b"edge sentinel\n".as_slice()),
            (".khive/kg/notes.ndjson", b"note sentinel\n".as_slice()),
            (
                ".khive/kg/remotes/origin/archive.ndjson",
                b"cache sentinel\n".as_slice(),
            ),
            (
                ".khive/kg/remotes/origin/pin",
                b"content pin sentinel\n".as_slice(),
            ),
            (".khive/db/khive.db", b"database sentinel\0".as_slice()),
            (
                ".khive/state/existing-state",
                b"state sentinel\n".as_slice(),
            ),
        ] {
            let path = project.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
        Self {
            root,
            project,
            remote,
            first: commits[0].clone(),
            second: commits[1].clone(),
            third: commits[2].clone(),
            annotated_tag,
        }
    }

    fn schema_path(&self) -> PathBuf {
        self.project.join(".khive/kg/schema.yaml")
    }

    fn schema(&self) -> Value {
        json!({
            "format_version": "2.1.7",
            "khive_version": "0.8.4",
            "ontology": {"version": "4.2.0", "labels": ["α", "needs: quoting"]},
            "packs": ["kg", "knowledge"],
            "custom": {"keep_null": null, "enabled": false, "nested": [7, {"text": "00123"}]},
            "remotes": [
                {"name": "untouched", "repo": "example/other", "commit": self.third,
                 "pin": format!("sha256:{}", "b".repeat(64)), "ref": "other-branch"},
                {"name": "origin", "url": self.remote.to_str().unwrap(), "commit": self.first,
                 "pin": format!("sha256:{}", "a".repeat(64)), "ref": "v1", "path": "nested/kg",
                 "namespace": "research", "custom_remote": {"tags": ["keep", "me"]}}
            ]
        })
    }

    fn write_schema(&self, value: &Value) {
        fs::write(self.schema_path(), serde_yaml::to_string(value).unwrap()).unwrap();
    }

    fn read_schema(&self) -> Value {
        serde_yaml::from_slice(&fs::read(self.schema_path()).unwrap()).unwrap()
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kkernel"));
        isolated(&mut command, self.root.path());
        command.current_dir(self.root.path());
        command
    }

    fn update(&self, remote: &str, reference: Option<&str>) -> Command {
        let mut command = self.command();
        command
            .args(["kg", "update", remote, "--repo"])
            .arg(&self.project);
        if let Some(reference) = reference {
            command.arg(format!("--ref={reference}"));
        }
        command
    }

    fn protected(&self) -> BTreeMap<PathBuf, Entry> {
        snapshot(
            &self.project,
            &[".khive/kg/schema.yaml", ".khive/state/kg-update.lock"],
        )
    }

    fn assert_no_runtime_or_staging(&self) {
        assert!(!self.root.path().join("must-not-open.db").exists());
        assert!(fs::read_dir(self.root.path().join("scratch"))
            .unwrap()
            .next()
            .is_none());
    }

    fn assert_report(
        &self,
        output: Output,
        reference: &str,
        previous: &str,
        commit: &str,
        updated: bool,
    ) {
        success(&output);
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            report,
            json!({
                "remote": "origin", "requested_ref": reference, "previous_commit": previous,
                "commit": commit, "updated": updated
            })
        );
        let resolved = report["commit"].as_str().unwrap();
        assert_eq!(resolved.len(), 40);
        assert!(resolved
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
        assert!(!String::from_utf8_lossy(&output.stdout).contains(self.remote.to_str().unwrap()));
        self.assert_no_runtime_or_staging();
    }

    fn assert_refusal(&self, command: &mut Command) {
        let before = fs::read(self.schema_path()).unwrap();
        let protected = self.protected();
        let output = command.output().unwrap();
        assert!(!output.status.success(), "unexpected success: {output:?}");
        assert!(
            output.stdout.is_empty(),
            "refusal must not print a success receipt"
        );
        assert!(!output.stderr.is_empty());
        assert_eq!(fs::read(self.schema_path()).unwrap(), before);
        assert_eq!(self.protected(), protected);
        self.assert_no_runtime_or_staging();
    }
}

#[test]
fn real_binary_updates_lightweight_and_annotated_tags_without_touching_other_values() {
    let fixture = Fixture::new();
    for reference in ["v2", "v2-annotated"] {
        let before = fixture.schema();
        fixture.write_schema(&before);
        let protected = fixture.protected();
        fixture.assert_report(
            fixture.update("origin", Some(reference)).output().unwrap(),
            reference,
            &fixture.first,
            &fixture.second,
            true,
        );
        let mut expected = before;
        expected["remotes"][1]["commit"] = json!(fixture.second);
        assert_eq!(fixture.read_schema(), expected);
        assert_ne!(
            fixture.read_schema()["remotes"][1]["commit"],
            fixture.annotated_tag
        );
        assert_eq!(fixture.protected(), protected);
    }
}

#[test]
fn defaults_use_configured_ref_then_the_remotes_non_main_head() {
    let fixture = Fixture::new();
    for configured in [true, false] {
        let mut schema = fixture.schema();
        if configured {
            schema["remotes"][1]["ref"] = json!("v2");
        } else {
            schema["remotes"][1].as_object_mut().unwrap().remove("ref");
        }
        fixture.write_schema(&schema);
        let expected = if configured {
            &fixture.second
        } else {
            &fixture.third
        };
        fixture.assert_report(
            fixture.update("origin", None).output().unwrap(),
            if configured { "v2" } else { "HEAD" },
            &fixture.first,
            expected,
            true,
        );
        schema["remotes"][1]["commit"] = json!(expected);
        assert_eq!(fixture.read_schema(), schema);
    }
}

#[test]
fn branch_and_full_commit_inputs_resolve_through_actual_git() {
    let fixture = Fixture::new();
    for (reference, expected) in [
        ("upstream-default", fixture.third.as_str()),
        (fixture.second.as_str(), fixture.second.as_str()),
    ] {
        fixture.write_schema(&fixture.schema());
        fixture.assert_report(
            fixture.update("origin", Some(reference)).output().unwrap(),
            reference,
            &fixture.first,
            expected,
            true,
        );
        assert_eq!(fixture.read_schema()["remotes"][1]["commit"], expected);
    }
}

#[test]
fn repeated_and_case_equivalent_commits_leave_exact_yaml_bytes_untouched() {
    let fixture = Fixture::new();
    fixture.write_schema(&fixture.schema());
    fixture.assert_report(
        fixture.update("origin", Some("v2")).output().unwrap(),
        "v2",
        &fixture.first,
        &fixture.second,
        true,
    );
    for uppercase in [false, true] {
        let mut schema = fixture.read_schema();
        let previous = if uppercase {
            fixture.second.to_ascii_uppercase()
        } else {
            fixture.second.clone()
        };
        schema["remotes"][1]["commit"] = json!(previous);
        let bytes = format!(
            "# keep this exact no-op comment\n{}\n# trailing comment\n",
            serde_yaml::to_string(&schema).unwrap()
        );
        fs::write(fixture.schema_path(), &bytes).unwrap();
        let protected = fixture.protected();
        fixture.assert_report(
            fixture.update("origin", Some("v2")).output().unwrap(),
            "v2",
            &previous,
            &fixture.second,
            false,
        );
        assert_eq!(fs::read(fixture.schema_path()).unwrap(), bytes.as_bytes());
        assert_eq!(fixture.protected(), protected);
    }
}

#[test]
fn relative_source_uses_repo_argument_and_repo_defaults_to_current_directory() {
    let fixture = Fixture::new();
    let mut schema = fixture.schema();
    schema["remotes"][1]["url"] = json!("../remote with spaces.git");
    fixture.write_schema(&schema);
    fixture.assert_report(
        fixture.update("origin", Some("v2")).output().unwrap(),
        "v2",
        &fixture.first,
        &fixture.second,
        true,
    );
    fixture.write_schema(&schema);
    fixture.assert_report(
        fixture
            .command()
            .current_dir(&fixture.project)
            .args(["kg", "update", "origin", "--ref", "v2"])
            .output()
            .unwrap(),
        "v2",
        &fixture.first,
        &fixture.second,
        true,
    );
    schema["remotes"][1]["commit"] = json!(fixture.second);
    assert_eq!(fixture.read_schema(), schema);
}

#[test]
fn malformed_schema_and_remote_fields_refuse_without_mutation() {
    let fixture = Fixture::new();
    let base = fixture.schema();
    fixture.write_schema(&base);
    fixture.assert_report(
        fixture.update("origin", None).output().unwrap(),
        "v1",
        &fixture.first,
        &fixture.first,
        false,
    );
    let mut documents = vec![
        "[broken".to_owned(),
        "[]\n".to_owned(),
        format!("{}---\n{{}}\n", serde_yaml::to_string(&base).unwrap()),
    ];
    for version in [
        Value::Null,
        json!(2),
        json!("2"),
        json!("1.0.0"),
        json!("3.0.0"),
    ] {
        let mut schema = base.clone();
        schema["format_version"] = version;
        documents.push(serde_yaml::to_string(&schema).unwrap());
    }
    for field in ["format_version", "remotes"] {
        let mut schema = base.clone();
        schema.as_object_mut().unwrap().remove(field);
        documents.push(serde_yaml::to_string(&schema).unwrap());
    }
    for remotes in [json!({}), json!([false]), json!([])] {
        let mut schema = base.clone();
        schema["remotes"] = remotes;
        documents.push(serde_yaml::to_string(&schema).unwrap());
    }
    let mut duplicate = base.clone();
    let selected = duplicate["remotes"][1].clone();
    duplicate["remotes"].as_array_mut().unwrap().push(selected);
    documents.push(serde_yaml::to_string(&duplicate).unwrap());
    for (field, value) in [
        ("name", json!(7)),
        ("commit", Value::Null),
        ("commit", json!(7)),
        ("commit", json!("v1")),
        ("commit", json!("f".repeat(39))),
        ("commit", json!("sha256:".to_owned() + &"a".repeat(64))),
        ("url", Value::Null),
        ("url", json!(7)),
        ("url", json!("")),
        ("ref", json!(7)),
        ("ref", json!("")),
        ("repo", json!("owner/name")),
    ] {
        let mut schema = base.clone();
        schema["remotes"][1][field] = value;
        documents.push(serde_yaml::to_string(&schema).unwrap());
    }
    for field in ["name", "commit", "url"] {
        let mut schema = base.clone();
        schema["remotes"][1].as_object_mut().unwrap().remove(field);
        documents.push(serde_yaml::to_string(&schema).unwrap());
    }
    for repository in ["../owner", "owner/../repo", "owner", "/repo", "owner/"] {
        let mut schema = base.clone();
        schema["remotes"][1].as_object_mut().unwrap().remove("url");
        schema["remotes"][1]["repo"] = json!(repository);
        documents.push(serde_yaml::to_string(&schema).unwrap());
    }
    for document in documents {
        fs::write(fixture.schema_path(), document).unwrap();
        fixture.assert_refusal(&mut fixture.update("origin", None));
    }
    fixture.write_schema(&base);
    fixture.assert_refusal(&mut fixture.update("absent", Some("v2")));
}

#[test]
fn invalid_unknown_and_non_commit_refs_leave_schema_and_archive_intact() {
    let fixture = Fixture::new();
    fixture.write_schema(&fixture.schema());
    for reference in [
        "",
        "-c",
        "*",
        "v2:refs/heads/injected",
        "^v2",
        "v2^{commit}",
        "v2~1",
        "bad ref",
        "v2\nother",
        "refs/heads/../bad",
        "does-not-exist",
        "tree-only",
        "blob-only",
    ] {
        fixture.assert_refusal(&mut fixture.update("origin", Some(reference)));
    }
    let mut schema = fixture.schema();
    schema["remotes"][1]["url"] = json!(fixture.root.path().join("missing.git").to_str().unwrap());
    fixture.write_schema(&schema);
    fixture.assert_refusal(&mut fixture.update("origin", Some("v2")));
}

#[test]
fn inherited_git_routing_cannot_mutate_the_callers_repository_or_index() {
    let fixture = Fixture::new();
    fixture.write_schema(&fixture.schema());
    let caller = fixture.root.path().join("caller");
    fs::create_dir(&caller).unwrap();
    git(
        fixture.root.path(),
        &caller,
        &["init", "-q", "-b", "caller-branch"],
    );
    git(
        fixture.root.path(),
        &caller,
        &["config", "user.name", "Caller"],
    );
    git(
        fixture.root.path(),
        &caller,
        &["config", "user.email", "caller@example.invalid"],
    );
    fs::write(caller.join("tracked"), "committed\n").unwrap();
    git(fixture.root.path(), &caller, &["add", "--", "tracked"]);
    git(
        fixture.root.path(),
        &caller,
        &["commit", "-qm", "caller commit"],
    );
    fs::write(caller.join("tracked"), "staged\n").unwrap();
    git(fixture.root.path(), &caller, &["add", "--", "tracked"]);
    fs::write(caller.join("tracked"), "unstaged\n").unwrap();
    fs::write(caller.join("untracked"), "retain\n").unwrap();
    let before = snapshot(&caller, &[]);
    let protected = fixture.protected();
    let git_dir = caller.join(".git");
    let output = fixture
        .update("origin", Some("v2"))
        .env("GIT_DIR", &git_dir)
        .env("GIT_COMMON_DIR", &git_dir)
        .env("GIT_WORK_TREE", &caller)
        .env("GIT_INDEX_FILE", git_dir.join("index"))
        .env("GIT_OBJECT_DIRECTORY", git_dir.join("objects"))
        .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", git_dir.join("objects"))
        .output()
        .unwrap();
    fixture.assert_report(output, "v2", &fixture.first, &fixture.second, true);
    assert_eq!(snapshot(&caller, &[]), before);
    assert_eq!(fixture.protected(), protected);
}

#[test]
fn missing_schema_and_nonregular_schema_are_not_fabricated_or_replaced() {
    let fixture = Fixture::new();
    let before = fixture.protected();
    let output = fixture.update("origin", Some("v2")).output().unwrap();
    assert!(!output.status.success());
    assert!(!fixture.schema_path().exists());
    assert_eq!(fixture.protected(), before);
    fs::create_dir(fixture.schema_path()).unwrap();
    fs::write(fixture.schema_path().join("keep"), "directory sentinel").unwrap();
    let output = fixture.update("origin", Some("v2")).output().unwrap();
    assert!(!output.status.success());
    assert!(fixture.schema_path().is_dir());
    assert_eq!(
        fs::read(fixture.schema_path().join("keep")).unwrap(),
        b"directory sentinel"
    );
    fixture.assert_no_runtime_or_staging();
}

#[cfg(unix)]
#[test]
fn schema_symlink_is_refused_without_writing_its_target() {
    let fixture = Fixture::new();
    let target = fixture.root.path().join("outside-schema.yaml");
    let bytes = serde_yaml::to_string(&fixture.schema()).unwrap();
    fs::write(&target, &bytes).unwrap();
    std::os::unix::fs::symlink(&target, fixture.schema_path()).unwrap();
    let output = fixture.update("origin", Some("v2")).output().unwrap();
    assert!(!output.status.success());
    assert_eq!(fs::read_link(fixture.schema_path()).unwrap(), target);
    assert_eq!(fs::read(&target).unwrap(), bytes.as_bytes());
    fixture.assert_no_runtime_or_staging();
}

#[test]
fn clap_refusals_and_existing_command_help_do_not_write_schema() {
    let fixture = Fixture::new();
    fixture.write_schema(&fixture.schema());
    for args in [
        vec!["kg", "update"],
        vec!["kg", "update", "origin", "--unknown"],
        vec!["kg", "update", "origin", "--ref"],
    ] {
        fixture.assert_refusal(fixture.command().args(args));
    }
    for (subcommand, expected) in [
        ("update", vec!["--repo", "--ref", "<REMOTE>"]),
        ("fetch", vec!["--url", "--repo"]),
        ("import", vec!["--format", "json", "csv", "tsv"]),
    ] {
        let before = fs::read(fixture.schema_path()).unwrap();
        let output = fixture
            .command()
            .args(["kg", subcommand, "--help"])
            .output()
            .unwrap();
        success(&output);
        let help = String::from_utf8(output.stdout).unwrap();
        for marker in expected {
            assert!(
                help.contains(marker),
                "{subcommand} missing {marker}: {help}"
            );
        }
        assert_eq!(fs::read(fixture.schema_path()).unwrap(), before);
        fixture.assert_no_runtime_or_staging();
    }
}

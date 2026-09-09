//! Local git contract tests. The CAS control removes update-ref's expected old
//! value; the at-ref-move test must then fail by observing the rival overwritten.
//! The hooks control removes core.hooksPath=/dev/null; the hook marker test must
//! then fail. These controls require separate source mutation runs.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use khive_pack_blob::BlobPack;
use khive_pack_exec::{tree, ExecPack};
use khive_pack_git::GitPack;
use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::engine_config::{
    GitWriteActorConfig, GitWriteEntryConfig, GitWriteSectionConfig,
};
use khive_runtime::{
    KhiveRuntime, RequestIdentity, RuntimeConfig, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::{ContentRef, MAX_BLOB_WHOLE_BYTES};
use serde_json::{json, Value};

const ACTOR: &str = "actor:dev-loop";
const TOKEN: &str = "synthetic-credential-not-a-live-secret";
const ZERO: &str = "0000000000000000000000000000000000000000";

fn quoted(path: &Path) -> String {
    format!(
        "'{}'",
        path.to_str()
            .expect("fixture path UTF-8")
            .replace('\'', "'\\''")
    )
}

fn executable(path: &Path, source: &str) {
    std::fs::write(path, source).expect("write executable fixture");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod fixture");
}

fn git_program() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .and_then(|path| std::fs::canonicalize(path).ok())
        .expect("native git executable")
}

fn command(program: &Path, repo: &Path, args: &[&str], hardened: bool) -> Command {
    let mut command = Command::new(program);
    command.env_clear();
    command
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_AUTHOR_NAME", "Fixture Author")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture Author")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid");
    if hardened {
        command.args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "commit.gpgsign=false",
        ]);
    }
    command.arg("-C").arg(repo).args(args);
    command
}

fn output(mut command: Command, input: Option<&[u8]>) -> Output {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn native git");
    if let Some(bytes) = input {
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(bytes)
            .expect("write stdin");
    } else {
        drop(child.stdin.take());
    }
    let out = child.wait_with_output().expect("wait for native git");
    assert!(
        out.status.success(),
        "native git failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

struct EnvGuard(Vec<(String, Option<OsString>)>);

impl EnvGuard {
    fn set(values: &[(&str, OsString)]) -> Self {
        let mut previous = Vec::new();
        for (key, value) in values {
            previous.push(((*key).to_string(), std::env::var_os(key)));
            std::env::set_var(key, value);
        }
        Self(previous)
    }

    fn remove(keys: &[&str]) -> Self {
        let previous = keys
            .iter()
            .map(|key| {
                let previous = ((*key).to_string(), std::env::var_os(key));
                std::env::remove_var(key);
                previous
            })
            .collect();
        Self(previous)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..).rev() {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

struct Fixture {
    registry: VerbRegistry,
    rt: KhiveRuntime,
    repo: PathBuf,
    /// A second allowlisted path that exists and holds no repository: git.init's only legal target.
    blank: PathBuf,
    git: PathBuf,
    base: String,
    resolver_calls: PathBuf,
    secret: PathBuf,
    dir: tempfile::TempDir,
}

impl Fixture {
    async fn new(allowlisted: bool, mapped: bool) -> Self {
        Self::new_with_tool_pack(allowlisted, mapped, true).await
    }

    async fn new_with_tool_pack(allowlisted: bool, mapped: bool, include_tool: bool) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).expect("repo directory");
        let repo = std::fs::canonicalize(repo).expect("canonical repo");
        let blank = dir.path().join("blank");
        std::fs::create_dir(&blank).expect("blank directory");
        let blank = std::fs::canonicalize(blank).expect("canonical blank");
        let git = git_program();
        output(
            command(&git, &repo, &["init", "-q", "-b", "work"], true),
            None,
        );
        std::fs::write(repo.join("a.txt"), b"old\n").expect("tracked file");
        std::fs::write(repo.join("removed.txt"), b"delete me\n").expect("tracked deletion");
        output(
            command(&git, &repo, &["add", "--", "a.txt", "removed.txt"], true),
            None,
        );
        output(
            command(&git, &repo, &["commit", "-q", "-m", "initial"], true),
            None,
        );
        let base = String::from_utf8(
            output(command(&git, &repo, &["rev-parse", "HEAD"], true), None).stdout,
        )
        .expect("SHA")
        .trim()
        .to_string();
        let resolver = dir.path().join("resolver");
        let resolver_calls = dir.path().join("resolver.calls");
        let secret = dir.path().join("resolver.value");
        std::fs::write(&secret, format!("{TOKEN}\n")).expect("synthetic secret");
        executable(
            &resolver,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$1\" >> {}\nexec /bin/cat {}\n",
                quoted(&resolver_calls),
                quoted(&secret)
            ),
        );
        let actors = if mapped {
            BTreeMap::from([(
                ACTOR.into(),
                GitWriteActorConfig {
                    name: "Mapped Author".into(),
                    email: "mapped@example.invalid".into(),
                    credential_ref: "fixture-reference".into(),
                    platform_identity: "fixture-login".into(),
                },
            )])
        } else {
            BTreeMap::new()
        };
        let config = RuntimeConfig {
            db_path: Some(dir.path().join("runtime.db")),
            git_write: GitWriteSectionConfig {
                allowed: if allowlisted {
                    // `blank` is listed second on purpose: every existing arm asserts gate.id 0.
                    vec![
                        GitWriteEntryConfig {
                            repo: repo.display().to_string(),
                            branches: vec!["*".into()],
                        },
                        GitWriteEntryConfig {
                            repo: blank.display().to_string(),
                            branches: vec!["*".into()],
                        },
                    ]
                } else {
                    vec![]
                },
                actors,
                credential_resolver: vec![resolver.display().to_string(), "{ref}".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let rt = KhiveRuntime::new(config).expect("file runtime");
        // File runtimes require explicit blob-store installation, as at boot.
        // Keep the fixture independent of the host's blob-root environment.
        let blob_store = {
            let _env = EnvGuard::remove(&["KHIVE_BLOB_ROOT"]);
            rt.backend()
                .blob_store(Some(&dir.path().join("blobs")), Some(0))
                .expect("fixture blob store")
        };
        rt.install_blob_store(blob_store)
            .expect("install fixture blobs");
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some(ACTOR.into()));
        builder.register(KgPack::new(rt.clone()));
        builder.register(BlobPack::new(rt.clone()));
        if include_tool {
            builder.register(ToolPack::new(rt.clone()));
            builder.register(ExecPack::new(rt.clone()));
        }
        builder.register(GitPack::new(rt.clone()));
        builder
            .with_runtime_event_store(&rt)
            .expect("real audit store");
        let registry = builder.build().expect("registry");
        registry.apply_schema_plans(rt.backend());
        rt.install_edge_rules(registry.all_edge_rules());
        let fixture = Self {
            registry,
            rt,
            repo,
            blank,
            git,
            base,
            resolver_calls,
            secret,
            dir,
        };
        if include_tool {
            for verb in [
                "git.receipts",
                "git.gates",
                "git.checkout",
                "git.diff",
                "git.reconcile",
            ] {
                fixture.policy(verb, "allow").await;
            }
        }
        fixture
    }

    fn git_bytes(&self, args: &[&str]) -> Vec<u8> {
        output(command(&self.git, &self.repo, args, true), None).stdout
    }

    fn git_text(&self, args: &[&str]) -> String {
        String::from_utf8(self.git_bytes(args))
            .expect("native output UTF-8")
            .trim()
            .to_string()
    }

    async fn call(&self, verb: &str, params: Value) -> Value {
        self.registry
            .dispatch(verb, params)
            .await
            .unwrap_or_else(|error| panic!("{verb}: {error}"))
    }

    async fn err(&self, verb: &str, params: Value) -> String {
        self.registry
            .dispatch(verb, params)
            .await
            .expect_err("operation must refuse")
            .to_string()
    }

    async fn policy(&self, verb: &str, decision: &str) -> String {
        self.call(
            "tool.policy",
            json!({"actor": ACTOR, "tool": verb, "decision": decision}),
        )
        .await["policy"]["id"]
            .as_str()
            .expect("actual policy id")
            .to_string()
    }

    async fn tree(&self, files: &[(&str, &[u8], u32)]) -> String {
        let blobs = tree::blob_store(&self.rt).expect("blob store");
        let mut entries = Vec::new();
        for (path, bytes, mode) in files {
            let content = blobs.put(bytes.to_vec()).await.expect("put raw blob");
            entries.push(json!({"path":path,"ref":content.as_str(),"mode":mode}));
        }
        self.call("exec.tree", json!({"entries":entries})).await["tree"]
            .as_str()
            .expect("tree ref")
            .to_string()
    }

    async fn blob(&self, reference: &str) -> Vec<u8> {
        tree::blob_store(&self.rt)
            .expect("blob store")
            .get_bounded_verified(
                &ContentRef::from_hex(reference).expect("content ref"),
                MAX_BLOB_WHOLE_BYTES,
            )
            .await
            .expect("verified blob bytes")
    }

    fn commit_params(&self, manifest: &str) -> Value {
        json!({"repo":self.repo,"branch":"work","tree":manifest,"message":"manifest commit","expected_head":self.base,"session_id":"session-a"})
    }

    async fn receipt(&self, id: &str) -> Value {
        self.call("git.receipts", json!({"limit":500,"offset":0}))
            .await["receipts"]
            .as_array()
            .expect("receipts array")
            .iter()
            .find(|row| row["id"] == id)
            .unwrap_or_else(|| panic!("durable receipt {id} absent"))
            .clone()
    }

    async fn refusal_receipt(&self, error: &str) -> Value {
        let id: String = error
            .split("receipt_id=")
            .nth(1)
            .expect("error names receipt")
            .chars()
            .take_while(|c| c.is_ascii_hexdigit() || *c == '-')
            .collect();
        let receipt = self.receipt(&id).await;
        assert_eq!(receipt["disposition"], "not_committed");
        receipt
    }

    async fn stored_refusal_receipt(&self, error: &str) -> Value {
        let id: String = error
            .split("receipt_id=")
            .nth(1)
            .expect("error names receipt")
            .chars()
            .take_while(|c| c.is_ascii_hexdigit() || *c == '-')
            .collect();
        let mut reader = self.rt.sql().reader().await.expect("refusal row reader");
        let rows = reader.query_all(SqlStatement {
            sql:"SELECT gate, policy, disposition, reason FROM git_receipts WHERE id = ?1 AND namespace = 'local' AND actor = ?2".into(),
            params:vec![SqlValue::Text(id),SqlValue::Text(ACTOR.into())],
            label:Some("git_test_refusal_without_tool_pack".into()),
        }).await.expect("read actual owned refusal row");
        assert_eq!(rows.len(), 1);
        let mut value = serde_json::Map::new();
        for name in ["gate", "policy", "disposition", "reason"] {
            let Some(SqlValue::Text(text)) = rows[0].get(name) else {
                panic!("missing text column {name}");
            };
            value.insert(
                name.into(),
                if matches!(name, "gate" | "policy") {
                    serde_json::from_str(text).expect("stored decision JSON")
                } else {
                    Value::String(text.clone())
                },
            );
        }
        assert_eq!(value["disposition"], "not_committed");
        Value::Object(value)
    }

    async fn success_receipt(&self, result: &Value) -> Value {
        let receipt = self
            .receipt(result["receipt_id"].as_str().expect("receipt id"))
            .await;
        assert_eq!(receipt["disposition"], "committed");
        assert_eq!(receipt["result"], *result);
        receipt
    }

    fn resolver_count(&self) -> usize {
        match std::fs::read_to_string(&self.resolver_calls) {
            Ok(text) => text.lines().count(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => panic!("resolver log: {error}"),
        }
    }

    async fn seed_unknown_receipt(&self, id: &str, branch: &str, sha: &str, policy_id: &str) {
        let inputs = json!({"branch":branch,"expected_head":self.base});
        let result = json!({"sha":sha,"parent":self.base,"ref":format!("refs/heads/{branch}"),"receipt_id":id});
        let gate = json!({"decision":"allow","source":"git_write.allowed","id":0});
        let policy = json!({"decision":"allow","source":"policy","id":policy_id});
        let credential =
            json!({"source":"actor","ref":"fixture-reference","platform_identity":"fixture-login"});
        let mut writer = self.rt.sql().writer().await.expect("test receipt writer");
        let changed = writer.execute(SqlStatement {
            sql:"INSERT INTO git_receipts (id, namespace, actor, session_id, verb, repo, inputs, gate, policy, fork_policy, credential, started_at, finished_at, disposition, result, reason) VALUES (?1, 'local', ?2, 'reconcile-test', 'git.commit', ?3, ?4, ?5, ?6, NULL, ?7, ?8, NULL, 'unknown', ?9, NULL)".into(),
            params:vec![
                SqlValue::Text(id.into()), SqlValue::Text(ACTOR.into()),
                SqlValue::Text(self.repo.display().to_string()), SqlValue::Text(inputs.to_string()),
                SqlValue::Text(gate.to_string()), SqlValue::Text(policy.to_string()),
                SqlValue::Text(credential.to_string()), SqlValue::Integer(chrono::Utc::now().timestamp_micros()),
                SqlValue::Text(result.to_string()),
            ],
            label:Some("git_test_unknown_intent".into()),
        }).await.expect("seed exact unknown intent row");
        assert_eq!(changed, 1);
    }
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn arm13_existing_branch_refuses_without_repository_changes() {
    let f = Fixture::new(true, false).await;
    f.policy("git.branch", "allow").await;
    let refs = f.git_bytes(&["show-ref"]);
    let objects = f.git_bytes(&["count-objects", "-v"]);
    let error = f
        .err(
            "git.branch",
            json!({"repo":f.repo,"name":"work","from":f.base}),
        )
        .await;
    f.refusal_receipt(&error).await;
    assert_eq!(f.git_bytes(&["show-ref"]), refs);
    assert_eq!(f.git_bytes(&["count-objects", "-v"]), objects);
    assert_eq!(f.resolver_count(), 0);
    let result = f
        .call(
            "git.branch",
            json!({"repo":f.repo,"name":"new","expected":f.base}),
        )
        .await;
    let row = f.success_receipt(&result).await;
    assert_eq!(f.git_text(&["rev-parse", "new"]), f.base);
    assert!(row["credential"].is_null());
    assert_eq!(f.resolver_count(), 0);
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn arm14_moved_parent_refuses_before_ref_move() {
    let f = Fixture::new(true, true).await;
    f.policy("git.commit", "allow").await;
    let manifest = f.tree(&[("a.txt", b"new\n", 644)]).await;
    f.git_bytes(&["commit", "--allow-empty", "-q", "-m", "rival"]);
    let refs = f.git_bytes(&["show-ref"]);
    let error = f.err("git.commit", f.commit_params(&manifest)).await;
    assert_eq!(
        f.refusal_receipt(&error).await["reason"],
        "expected_head_mismatch"
    );
    assert_eq!(f.git_bytes(&["show-ref"]), refs);
    assert_eq!(f.resolver_count(), 0);
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn arm14_ref_move_race_preserves_rival_with_atomic_compare() {
    let f = Fixture::new(true, true).await;
    f.policy("git.commit", "allow").await;
    let manifest = f.tree(&[("a.txt", b"ours\n", 644)]).await;
    let git_tree = f.git_text(&["rev-parse", "HEAD^{tree}"]);
    let rival = f.git_text(&["commit-tree", &git_tree, "-p", &f.base, "-m", "rival"]);
    let bin = f.dir.path().join("interposer");
    std::fs::create_dir(&bin).expect("interposer directory");
    let marker = f.dir.path().join("raced");
    executable(&bin.join("git"), &format!(
        "#!/bin/sh\nis_update=0\nfor argument do\n  if [ \"$argument\" = update-ref ]; then is_update=1; fi\n  if [ \"$is_update\" = 1 ] && [ \"$argument\" = refs/heads/work ] && [ ! -e {marker} ]; then\n    printf raced > {marker}\n    {git} -c core.hooksPath=/dev/null -C {repo} update-ref refs/heads/work {rival} {base} || exit 91\n  fi\ndone\nexec {git} \"$@\"\n",
        marker=quoted(&marker), git=quoted(&f.git), repo=quoted(&f.repo), rival=rival, base=f.base,
    ));
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").expect("PATH"),
    ));
    let _env = EnvGuard::set(&[(
        "PATH",
        std::env::join_paths(paths).expect("interposer PATH"),
    )]);
    let outcome = f
        .registry
        .dispatch("git.commit", f.commit_params(&manifest))
        .await;
    assert!(
        marker.exists(),
        "CAS race control never reached native update-ref"
    );
    assert_eq!(
        f.git_text(&["rev-parse", "work"]),
        rival,
        "CAS_CONTROL_RIVAL_MUST_SURVIVE"
    );
    let error = outcome
        .expect_err("CAS_CONTROL_COMPARE_MUST_REFUSE")
        .to_string();
    f.refusal_receipt(&error).await;
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn arm14_manifest_commit_preserves_index_and_worktree_residue() {
    let f = Fixture::new(true, true).await;
    f.policy("git.commit", "allow").await;
    std::fs::write(f.repo.join("a.txt"), b"staged\n").expect("staged content");
    f.git_bytes(&["add", "--", "a.txt"]);
    std::fs::write(f.repo.join("a.txt"), b"unstaged\n").expect("worktree residue");
    std::fs::write(f.repo.join("untracked"), b"untracked\n").expect("untracked residue");
    let index = std::fs::read(f.repo.join(".git/index")).expect("index bytes");
    let manifest = f
        .tree(&[
            ("a.txt", b"manifest\n", 644),
            ("nested/run", b"#!/bin/sh\n", 755),
        ])
        .await;
    let result = f.call("git.commit", f.commit_params(&manifest)).await;
    let sha = result["sha"].as_str().expect("commit SHA");
    assert_eq!(f.git_text(&["rev-parse", &format!("{sha}^")]), f.base);
    assert_eq!(
        f.git_bytes(&["ls-tree", "-r", "--name-only", sha]),
        b"a.txt\nnested/run\n"
    );
    assert_eq!(
        f.git_bytes(&["cat-file", "blob", &format!("{sha}:a.txt")]),
        b"manifest\n"
    );
    assert!(f
        .git_text(&["ls-tree", "-r", sha, "nested/run"])
        .starts_with("100755 blob "));
    assert_eq!(
        std::fs::read(f.repo.join(".git/index")).expect("index after"),
        index
    );
    assert_eq!(
        std::fs::read(f.repo.join("a.txt")).expect("worktree after"),
        b"unstaged\n"
    );
    assert_eq!(
        std::fs::read(f.repo.join("untracked")).expect("untracked after"),
        b"untracked\n"
    );
    assert_eq!(
        f.git_text(&["show", "-s", "--format=%an <%ae>", sha]),
        "Mapped Author <mapped@example.invalid>"
    );
    let checkout = f
        .call("git.checkout", json!({"repo":f.repo,"ref":sha}))
        .await;
    assert_eq!(
        checkout["tree"], manifest,
        "exact complete manifest including modes and deletion"
    );
    let row = f.success_receipt(&result).await;
    assert_eq!(
        row["credential"],
        json!({"source":"actor","ref":"fixture-reference","platform_identity":"fixture-login"})
    );
    assert_eq!(f.resolver_count(), 1);
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn arm15_overrides_and_unmapped_actor_refuse_before_resolver() {
    for mapped in [true, false] {
        let f = Fixture::new(true, mapped).await;
        f.policy("git.commit", "allow").await;
        let manifest = f.tree(&[("a.txt", b"new\n", 644)]).await;
        let refs = f.git_bytes(&["show-ref"]);
        for key in ["author", "actor", "credential"] {
            let mut params = f.commit_params(&manifest);
            params[key] = json!(TOKEN);
            let error = f.err("git.commit", params).await;
            let row = f.refusal_receipt(&error).await;
            assert_eq!(row["reason"], "invalid_params");
            assert!(!error.contains(TOKEN));
            assert!(!row.to_string().contains(TOKEN));
        }
        if !mapped {
            let error = f.err("git.commit", f.commit_params(&manifest)).await;
            assert_eq!(f.refusal_receipt(&error).await["reason"], "actor_unmapped");
        }
        assert_eq!(f.resolver_count(), 0);
        assert_eq!(f.git_bytes(&["show-ref"]), refs);
    }
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn arm16_hostile_hooks_filters_and_config_never_execute() {
    let f = Fixture::new(true, true).await;
    f.policy("git.commit", "allow").await;
    let hooks = f.dir.path().join("hooks");
    std::fs::create_dir(&hooks).expect("hooks directory");
    let hook_marker = f.dir.path().join("hook.marker");
    let filter_marker = f.dir.path().join("filter.marker");
    let config_marker = f.dir.path().join("config.marker");
    executable(
        &hooks.join("reference-transaction"),
        &format!(
            "#!/bin/sh\nprintf hook >> {}\nexit 0\n",
            quoted(&hook_marker)
        ),
    );
    let filter = f.dir.path().join("filter");
    executable(
        &filter,
        &format!(
            "#!/bin/sh\nprintf filter >> {}\nprintf FILTERED\n",
            quoted(&filter_marker)
        ),
    );
    let hostile = f.dir.path().join("hostile-program");
    executable(
        &hostile,
        &format!(
            "#!/bin/sh\nprintf config >> {}\nexit 1\n",
            quoted(&config_marker)
        ),
    );
    f.git_bytes(&["config", "core.hooksPath", hooks.to_str().unwrap()]);
    f.git_bytes(&["config", "filter.hostile.clean", filter.to_str().unwrap()]);
    f.git_bytes(&["config", "filter.hostile.smudge", filter.to_str().unwrap()]);
    f.git_bytes(&["config", "core.fsmonitor", hostile.to_str().unwrap()]);
    f.git_bytes(&["config", "commit.gpgsign", "true"]);
    f.git_bytes(&["config", "gpg.program", hostile.to_str().unwrap()]);
    std::fs::write(f.repo.join(".gitattributes"), b"a.txt filter=hostile\n")
        .expect("hostile attributes");

    output(
        command(
            &f.git,
            &f.repo,
            &["update-ref", "refs/heads/hook-control", &f.base, ZERO],
            false,
        ),
        None,
    );
    assert!(
        hook_marker.exists(),
        "reference-transaction positive control is inert"
    );
    f.git_bytes(&["update-ref", "-d", "refs/heads/hook-control"]);
    std::fs::remove_file(&hook_marker).expect("clear positive-control hook marker");
    output(
        command(
            &f.git,
            &f.repo,
            &["hash-object", "--path=a.txt", "--stdin"],
            true,
        ),
        Some(b"filter input\n"),
    );
    assert!(
        filter_marker.exists(),
        "clean-filter positive control is inert"
    );
    std::fs::remove_file(&filter_marker).expect("clear filter marker");
    let _env = EnvGuard::set(&[
        ("GIT_CONFIG_COUNT", "1".into()),
        ("GIT_CONFIG_KEY_0", "core.hooksPath".into()),
        ("GIT_CONFIG_VALUE_0", hooks.as_os_str().to_owned()),
        ("GIT_AUTHOR_NAME", "Injected Author".into()),
    ]);
    let raw = b"raw\r\n\0\xffbytes\n";
    let manifest = f.tree(&[("a.txt", raw, 644)]).await;
    let result = f.call("git.commit", f.commit_params(&manifest)).await;
    assert!(!hook_marker.exists(), "HOOK_CONTROL_MUST_NOT_RUN");
    assert!(!filter_marker.exists(), "clean or smudge filter ran");
    assert!(!config_marker.exists(), "fsmonitor or signer ran");
    let sha = result["sha"].as_str().unwrap();
    assert_eq!(
        f.git_bytes(&["cat-file", "blob", &format!("{sha}:a.txt")]),
        raw
    );
    assert_eq!(
        f.git_text(&["show", "-s", "--format=%an", sha]),
        "Mapped Author"
    );
    f.success_receipt(&result).await;
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn arm26_receipts_bind_real_policy_ids_and_gate_denial_has_null_policy() {
    for allowlisted in [false, true] {
        let f = Fixture::new(allowlisted, false).await;
        let policy_id = f.policy("git.branch", "allow").await;
        let params = json!({"repo":f.repo,"name":"new","from":f.base});
        if allowlisted {
            let result = f.call("git.branch", params).await;
            let row = f.success_receipt(&result).await;
            assert_eq!(
                row["gate"],
                json!({"decision":"allow","source":"git_write.allowed","id":0})
            );
            assert_eq!(
                row["policy"],
                json!({"decision":"allow","source":"policy","id":policy_id})
            );
        } else {
            let error = f.err("git.branch", params).await;
            let row = f.refusal_receipt(&error).await;
            assert_eq!(row["gate"]["decision"], "deny");
            assert!(row["policy"].is_null());
        }
    }
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn receipts_page_by_actor_session_and_never_echo_resolved_secrets() {
    let f = Fixture::new(true, true).await;
    f.policy("git.commit", "allow").await;
    let manifest = f.tree(&[("a.txt", b"new\n", 644)]).await;
    let result = f.call("git.commit", f.commit_params(&manifest)).await;
    f.success_receipt(&result).await;
    std::fs::write(&f.secret, b"rotated-synthetic-token\n").expect("rotate fixture secret");
    let mut params = f.commit_params(&manifest);
    params["expected_head"] = result["sha"].clone();
    let second = f.call("git.commit", params).await;
    assert_eq!(f.resolver_count(), 2);
    f.call(
        "tool.policy",
        json!({"actor":"actor:other","tool":"git.checkout","decision":"allow"}),
    )
    .await;
    let foreign = f
        .registry
        .dispatch_with_identity(
            "git.checkout",
            json!({"repo":f.repo,"ref":"HEAD","session_id":"session-a"}),
            Some(RequestIdentity {
                namespace: "local".into(),
                actor_id: Some("actor:other".into()),
                ..Default::default()
            }),
        )
        .await
        .expect("foreign read fixture");
    let first_page = f
        .call(
            "git.receipts",
            json!({"repo":f.repo,"session_id":"session-a","limit":1,"offset":0}),
        )
        .await;
    assert_eq!(first_page["receipts"].as_array().unwrap().len(), 1);
    assert_eq!(first_page["receipts"][0]["id"], result["receipt_id"]);
    assert_eq!(first_page["next_offset"], 1);
    let last_page = f
        .call(
            "git.receipts",
            json!({"repo":f.repo,"session_id":"session-a","limit":1,"offset":1}),
        )
        .await;
    assert_eq!(last_page["receipts"][0]["id"], second["receipt_id"]);
    assert!(last_page["next_offset"].is_null());
    let all = f
        .call("git.receipts", json!({"limit":500,"offset":0}))
        .await;
    assert!(!all
        .to_string()
        .contains(foreign["receipt_id"].as_str().unwrap()));
    for secret in [TOKEN, "rotated-synthetic-token"] {
        assert!(!all.to_string().contains(secret));
        assert!(!result.to_string().contains(secret));
        assert!(!second.to_string().contains(secret));
    }
    let mut reader = f.rt.sql().reader().await.expect("SQL reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT * FROM git_receipts".into(),
            params: vec![],
            label: Some("git_test_credential_census".into()),
        })
        .await
        .expect("actual durable receipt rows");
    drop(reader);
    let stored = serde_json::to_string(&rows).expect("raw row serialization");
    assert!(!stored.contains(TOKEN));
    assert!(!stored.contains("rotated-synthetic-token"));
    assert!(f
        .err(
            "git.receipts",
            json!({"actor":"actor:other","limit":1,"offset":0})
        )
        .await
        .contains("foreign_actor"));
    assert_eq!(
        f.call("git.receipts", json!({"limit":500,"offset":0}))
            .await,
        all,
        "audit reads do not recursively receipt themselves"
    );
    std::fs::write(&f.secret, b"").expect("rotate to invalid credential");
    let mut params = f.commit_params(&manifest);
    params["expected_head"] = second["sha"].clone();
    let error = f.err("git.commit", params).await;
    assert_eq!(f.refusal_receipt(&error).await["reason"], "actor_unmapped");
    assert_eq!(f.resolver_count(), 3, "each mutation rereads the resolver");
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn checkout_preserves_status_index_and_raw_content() {
    let f = Fixture::new(true, false).await;
    std::fs::write(f.repo.join("a.txt"), b"staged\n").unwrap();
    f.git_bytes(&["add", "--", "a.txt"]);
    std::fs::write(f.repo.join("a.txt"), b"unstaged\n").unwrap();
    std::fs::write(f.repo.join("untracked"), b"untracked\n").unwrap();
    let status = f.git_bytes(&["status", "--porcelain=v1", "-z"]);
    let index = std::fs::read(f.repo.join(".git/index")).unwrap();
    let result = f
        .call("git.checkout", json!({"repo":f.repo,"ref":f.base}))
        .await;
    assert_eq!(result["commit"], f.base);
    assert_eq!(f.git_bytes(&["status", "--porcelain=v1", "-z"]), status);
    assert_eq!(std::fs::read(f.repo.join(".git/index")).unwrap(), index);
    let manifest = f
        .call("exec.tree_get", json!({"tree":result["tree"]}))
        .await;
    let entries = manifest["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    let entry = entries
        .iter()
        .find(|entry| entry["path"] == "a.txt")
        .unwrap();
    assert_eq!(f.blob(entry["ref"].as_str().unwrap()).await, b"old\n");
    f.success_receipt(&result).await;
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn checkout_refuses_symlinks_and_submodules_without_checkout() {
    for entry_kind in ["symlink", "submodule"] {
        let f = Fixture::new(true, false).await;
        if entry_kind == "symlink" {
            std::os::unix::fs::symlink("a.txt", f.repo.join("link")).expect("symlink fixture");
            f.git_bytes(&["add", "--", "link"]);
        } else {
            f.git_bytes(&[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{},submodule", f.base),
            ]);
        }
        f.git_bytes(&["commit", "-q", "-m", "unsupported tree entry"]);
        let status = f.git_bytes(&["status", "--porcelain=v1", "-z"]);
        let index = std::fs::read(f.repo.join(".git/index")).expect("index");
        let error = f
            .err("git.checkout", json!({"repo":f.repo,"ref":"HEAD"}))
            .await;
        f.refusal_receipt(&error).await;
        assert_eq!(f.git_bytes(&["status", "--porcelain=v1", "-z"]), status);
        assert_eq!(
            std::fs::read(f.repo.join(".git/index")).expect("index after"),
            index
        );
    }
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn diff_bytes_and_inputs_match_independent_native_oracle_for_commits_and_trees() {
    let f = Fixture::new(true, false).await;
    let base_tree = f
        .call("git.checkout", json!({"repo":f.repo,"ref":f.base}))
        .await["tree"]
        .clone();
    std::fs::write(f.repo.join("a.txt"), b"new\nsecond\n").unwrap();
    std::fs::remove_file(f.repo.join("removed.txt")).unwrap();
    f.git_bytes(&["add", "-A"]);
    f.git_bytes(&["commit", "-q", "-m", "diff head"]);
    let head = f.git_text(&["rev-parse", "HEAD"]);
    let head_tree = f
        .call("git.checkout", json!({"repo":f.repo,"ref":head}))
        .await["tree"]
        .clone();
    for (kind, base, target, oracle_base, oracle_head) in [
        (
            "commits",
            json!(f.base),
            json!(head),
            f.base.clone(),
            head.clone(),
        ),
        (
            "trees",
            base_tree,
            head_tree,
            f.git_text(&["rev-parse", &format!("{}^{{tree}}", f.base)]),
            f.git_text(&["rev-parse", "HEAD^{tree}"]),
        ),
    ] {
        let oracle = f.git_bytes(&[
            "diff-tree",
            "-p",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--no-renames",
            &oracle_base,
            &oracle_head,
        ]);
        let result = f
            .call(
                "git.diff",
                json!({"repo":f.repo,"input_kind":kind,"base":base,"head":target}),
            )
            .await;
        assert_eq!(f.blob(result["diff"].as_str().unwrap()).await, oracle);
        assert_eq!(
            result["summary"],
            json!({"files":2,"additions":2,"deletions":2})
        );
        let row = f.success_receipt(&result).await;
        assert_eq!(
            row["inputs"],
            json!({"input_kind":kind,"base":base,"head":target})
        );
    }
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn arm28_symbolic_branch_refuses_both_moves_and_plain_ref_controls_proceed() {
    let f = Fixture::new(true, true).await;
    f.policy("git.branch", "allow").await;
    f.policy("git.commit", "allow").await;
    let manifest = f.tree(&[("a.txt", b"plain ref commit\n", 644)]).await;
    f.git_bytes(&["symbolic-ref", "refs/heads/symbolic", "refs/heads/work"]);
    let refs = f.git_bytes(&["show-ref"]);
    let alias = std::fs::read(f.repo.join(".git/refs/heads/symbolic")).expect("symbolic ref bytes");
    let branch_params = json!({"repo":f.repo,"name":"symbolic","from":f.base,"expected":f.base});
    let mut commit_params = f.commit_params(&manifest);
    commit_params["branch"] = json!("symbolic");
    for (verb, params) in [
        ("git.branch", branch_params.clone()),
        ("git.commit", commit_params.clone()),
    ] {
        let error = f.err(verb, params).await;
        assert_eq!(f.refusal_receipt(&error).await["reason"], "ref_symbolic");
        assert_eq!(f.git_bytes(&["show-ref"]), refs);
        assert_eq!(
            std::fs::read(f.repo.join(".git/refs/heads/symbolic")).expect("alias after refusal"),
            alias
        );
    }
    assert_eq!(f.resolver_count(), 0);

    // Removing only the alias makes the same create call legal; its target stays put.
    f.git_bytes(&["symbolic-ref", "--delete", "refs/heads/symbolic"]);
    assert_eq!(f.git_text(&["rev-parse", "work"]), f.base);
    let branch = f.call("git.branch", branch_params).await;
    f.success_receipt(&branch).await;
    let plain = command(
        &f.git,
        &f.repo,
        &["symbolic-ref", "--quiet", "refs/heads/symbolic"],
        true,
    )
    .output()
    .expect("inspect plain ref");
    assert_eq!(
        plain.status.code(),
        Some(1),
        "positive-control ref is still symbolic"
    );
    let result = f.call("git.commit", commit_params).await;
    f.success_receipt(&result).await;
    assert_eq!(
        f.git_text(&["rev-parse", "symbolic"]),
        result["sha"].as_str().unwrap()
    );
    assert_eq!(
        f.git_text(&["rev-parse", "work"]),
        f.base,
        "symbolic target must never move"
    );
    assert_eq!(f.resolver_count(), 1);
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn arm29_reconcile_requires_matching_receipt_marker_and_new_sha() {
    let f = Fixture::new(true, true).await;
    let policy_id = f.policy("git.commit", "allow").await;
    let native_tree = f.git_text(&["rev-parse", "HEAD^{tree}"]);
    let intended = f.git_text(&["commit-tree", &native_tree, "-p", &f.base, "-m", "intended"]);
    let other = f.git_text(&[
        "commit-tree",
        &native_tree,
        "-p",
        &f.base,
        "-m",
        "different move",
    ]);
    let descendant = f.git_text(&[
        "commit-tree",
        &native_tree,
        "-p",
        &intended,
        "-m",
        "advanced descendant",
    ]);
    for evidence in [
        "rival",
        "wrong-sha",
        "pruned",
        "rewound",
        "current",
        "matching",
    ] {
        let id = uuid::Uuid::new_v4().to_string();
        let branch = format!("reconcile-{evidence}");
        let reference = format!("refs/heads/{branch}");
        let marker = format!("khive-receipt:{id}");
        f.git_bytes(&[
            "update-ref",
            "--no-deref",
            "--create-reflog",
            "-m",
            "baseline",
            &reference,
            &f.base,
            ZERO,
        ]);
        f.seed_unknown_receipt(&id, &branch, &intended, &policy_id)
            .await;
        match evidence {
            "rival" => {
                f.git_bytes(&[
                    "update-ref",
                    "--no-deref",
                    "--create-reflog",
                    "-m",
                    "rival-installed",
                    &reference,
                    &intended,
                    &f.base,
                ]);
            }
            "wrong-sha" => {
                f.git_bytes(&[
                    "update-ref",
                    "--no-deref",
                    "--create-reflog",
                    "-m",
                    &marker,
                    &reference,
                    &other,
                    &f.base,
                ]);
                f.git_bytes(&[
                    "update-ref",
                    "--no-deref",
                    "--create-reflog",
                    "-m",
                    "rival-installed",
                    &reference,
                    &intended,
                    &other,
                ]);
            }
            "pruned" => {
                f.git_bytes(&[
                    "update-ref",
                    "--no-deref",
                    "--create-reflog",
                    "-m",
                    &marker,
                    &reference,
                    &intended,
                    &f.base,
                ]);
                f.git_bytes(&["reflog", "expire", "--expire=all", &reference]);
            }
            "rewound" => {
                f.git_bytes(&[
                    "update-ref",
                    "--no-deref",
                    "--create-reflog",
                    "-m",
                    &marker,
                    &reference,
                    &intended,
                    &f.base,
                ]);
                f.git_bytes(&[
                    "update-ref",
                    "--no-deref",
                    "--create-reflog",
                    "-m",
                    "rewound",
                    &reference,
                    &f.base,
                    &intended,
                ]);
            }
            "current" => {
                f.git_bytes(&[
                    "update-ref",
                    "--no-deref",
                    "--create-reflog",
                    "-m",
                    &marker,
                    &reference,
                    &intended,
                    &f.base,
                ]);
            }
            "matching" => {
                f.git_bytes(&[
                    "update-ref",
                    "--no-deref",
                    "--create-reflog",
                    "-m",
                    &marker,
                    &reference,
                    &intended,
                    &f.base,
                ]);
                f.git_bytes(&[
                    "update-ref",
                    "--no-deref",
                    "--create-reflog",
                    "-m",
                    "later-descendant",
                    &reference,
                    &descendant,
                    &intended,
                ]);
            }
            _ => unreachable!(),
        }
        let log_before = f.git_bytes(&["reflog", "show", "--format=%H %gs", &reference]);
        let witness = format!("{intended} {marker}\n");
        assert_eq!(
            String::from_utf8_lossy(&log_before).contains(&witness),
            matches!(evidence, "rewound" | "current" | "matching")
        );
        if matches!(evidence, "rival" | "wrong-sha" | "pruned") {
            assert_eq!(
                f.git_text(&["rev-parse", &reference]),
                intended,
                "all negative controls have the intended current SHA"
            );
        }
        let refs_before = f.git_bytes(&["show-ref"]);
        let result = f.call("git.reconcile", json!({"receipt":id})).await;
        let expected = if matches!(evidence, "current" | "matching") {
            "committed"
        } else {
            "unknown"
        };
        assert_eq!(
            result["receipt"]["disposition"], expected,
            "reconcile evidence case {evidence}"
        );
        assert_eq!(f.receipt(&id).await["disposition"], expected);
        assert_eq!(
            f.git_bytes(&["show-ref"]),
            refs_before,
            "reconcile must never retry the ref move"
        );
        assert_eq!(
            f.git_bytes(&["reflog", "show", "--format=%H %gs", &reference]),
            log_before
        );
        f.success_receipt(&result).await;
    }
    assert_eq!(
        f.resolver_count(),
        0,
        "read-only reconciliation must not resolve a credential"
    );
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn arm30_tool_pack_absence_refuses_tree_commit_but_preserves_legacy_paths() {
    let f = Fixture::new_with_tool_pack(true, true, false).await;
    let mut reader = f.rt.sql().reader().await.expect("schema reader");
    let tables = reader.query_all(SqlStatement {
        sql:"SELECT name FROM sqlite_master WHERE type = 'table' AND name IN ('tool_policy', 'tool_grants')".into(),
        params:vec![], label:Some("git_test_absent_tool_schema".into()),
    }).await.expect("inspect real schema");
    drop(reader);
    assert!(
        tables.is_empty(),
        "missing-pack control accidentally installed tool tables"
    );
    let blob = tree::blob_store(&f.rt)
        .expect("blob store")
        .put(b"tree candidate\n".to_vec())
        .await
        .expect("manifest content");
    let manifest = tree::store(
        &f.rt,
        &[tree::TreeEntry {
            path: "a.txt".into(),
            content_ref: blob.as_str().into(),
            mode: 644,
        }],
    )
    .await
    .expect("existing tree manifest without exec dispatch");
    let refs = f.git_bytes(&["show-ref"]);
    let error = f.err("git.commit", f.commit_params(&manifest)).await;
    let row = f.stored_refusal_receipt(&error).await;
    assert_eq!(row["reason"], "policy_unavailable");
    assert_eq!(
        row["gate"],
        json!({"decision":"allow","source":"git_write.allowed","id":0})
    );
    assert_eq!(
        row["policy"],
        json!({"decision":"deny","source":"policy_unavailable","id":null})
    );
    assert_eq!(f.git_bytes(&["show-ref"]), refs);
    assert_eq!(f.resolver_count(), 0);

    f.git_bytes(&["config", "user.name", "Legacy Fixture"]);
    f.git_bytes(&["config", "user.email", "legacy@example.invalid"]);
    f.git_bytes(&["config", "commit.gpgsign", "false"]);
    std::fs::write(f.repo.join("a.txt"), b"legacy paths content\n").expect("legacy content");
    let _identity = EnvGuard::remove(&[
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
        "GIT_AUTHOR_DATE",
        "GIT_COMMITTER_DATE",
        "GIT_CONFIG_PARAMETERS",
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_EXEC_PATH",
    ]);
    let _config = EnvGuard::set(&[
        ("GIT_CONFIG_COUNT", "0".into()),
        ("GIT_CONFIG_NOSYSTEM", "1".into()),
        ("GIT_CONFIG_SYSTEM", "/dev/null".into()),
        ("GIT_CONFIG_GLOBAL", "/dev/null".into()),
    ]);
    let legacy = f
        .call(
            "git.commit",
            json!({"repo":f.repo,"paths":["a.txt"],"message":"legacy paths commit"}),
        )
        .await;
    let sha = legacy["sha"].as_str().expect("legacy commit SHA");
    assert_eq!(f.git_text(&["rev-parse", "work"]), sha);
    assert_eq!(
        f.git_bytes(&["cat-file", "blob", &format!("{sha}:a.txt")]),
        b"legacy paths content\n"
    );
    assert_eq!(
        f.git_text(&["show", "-s", "--format=%an <%ae>", sha]),
        "Legacy Fixture <legacy@example.invalid>"
    );
    assert_eq!(f.resolver_count(), 0);
}

#[tokio::test]
#[serial_test::serial(git_dev_loop_env)]
async fn arm31_reflog_append_without_ref_install_stays_unknown_until_descendant_installed() {
    let f = Fixture::new(true, true).await;
    let policy_id = f.policy("git.commit", "allow").await;
    let native_tree = f.git_text(&["rev-parse", "HEAD^{tree}"]);
    let candidate = f.git_text(&[
        "commit-tree",
        &native_tree,
        "-p",
        &f.base,
        "-m",
        "candidate",
    ]);
    let descendant = f.git_text(&[
        "commit-tree",
        &native_tree,
        "-p",
        &candidate,
        "-m",
        "descendant",
    ]);
    let branch = "reflog-before-install";
    let reference = format!("refs/heads/{branch}");
    f.git_bytes(&[
        "update-ref",
        "--no-deref",
        "--create-reflog",
        "-m",
        "baseline",
        &reference,
        &f.base,
        ZERO,
    ]);
    let id = uuid::Uuid::new_v4().to_string();
    f.seed_unknown_receipt(&id, branch, &candidate, &policy_id)
        .await;
    let marker = format!("khive-receipt:{id}");
    let log_path = f.repo.join(".git/logs").join(&reference);
    let mut log = std::fs::OpenOptions::new()
        .append(true)
        .open(&log_path)
        .expect("real reflog");
    let entry = format!(
        "{} {} Fixture Author <fixture@example.invalid> {} +0000\t{}\n",
        f.base,
        candidate,
        chrono::Utc::now().timestamp(),
        marker
    );
    log.write_all(entry.as_bytes())
        .expect("append pre-install reflog witness");
    drop(log);
    assert_eq!(
        f.git_text(&["rev-parse", &reference]),
        f.base,
        "raw reflog append must not move a ref"
    );
    let witness = format!("{candidate} {marker}\n");
    assert!(String::from_utf8_lossy(&f.git_bytes(&[
        "reflog",
        "show",
        "--format=%H %gs",
        &reference
    ]))
    .contains(&witness));
    let refs = f.git_bytes(&["show-ref"]);
    let first = f.call("git.reconcile", json!({"receipt":id})).await;
    assert_eq!(
        first["receipt"]["disposition"], "unknown",
        "marker alone cannot prove the ref installed"
    );
    assert_eq!(f.receipt(&id).await["disposition"], "unknown");
    assert_eq!(f.git_bytes(&["show-ref"]), refs);

    f.git_bytes(&[
        "update-ref",
        "--no-deref",
        "--create-reflog",
        "-m",
        "descendant-installed",
        &reference,
        &descendant,
        &f.base,
    ]);
    f.git_bytes(&["merge-base", "--is-ancestor", &candidate, &descendant]);
    let refs = f.git_bytes(&["show-ref"]);
    let second = f.call("git.reconcile", json!({"receipt":id})).await;
    assert_eq!(second["receipt"]["disposition"], "committed");
    assert_eq!(f.receipt(&id).await["disposition"], "committed");
    assert_eq!(f.git_bytes(&["show-ref"]), refs);
    assert_eq!(f.resolver_count(), 0);
}

// -- ADR-182 Amendment 8: git.status and git.log ------------------------------------------------

/// Native porcelain v2 records, headers dropped, so a count here is the population `total` claims.
fn native_status_paths(fixture: &Fixture, untracked: &str) -> Vec<String> {
    let bytes = fixture.git_bytes(&[
        "status",
        "--porcelain=v2",
        "--branch",
        "--no-renames",
        &format!("--untracked-files={untracked}"),
        "-z",
    ]);
    String::from_utf8(bytes)
        .expect("porcelain UTF-8 in this fixture")
        .split('\0')
        .filter(|record| !record.is_empty() && !record.starts_with("# "))
        .map(|record| {
            let (token, rest) = record.split_once(' ').unwrap_or((record, ""));
            match token {
                "?" | "!" => rest.to_string(),
                "1" => rest.splitn(8, ' ').nth(7).unwrap_or_default().to_string(),
                "u" => rest.splitn(10, ' ').nth(9).unwrap_or_default().to_string(),
                _ => panic!("unhandled porcelain token {token:?}"),
            }
        })
        .collect()
}

fn status_paths(result: &Value) -> Vec<String> {
    result["entries"]
        .as_array()
        .expect("entries array")
        .iter()
        .map(|entry| entry["path"].as_str().expect("entry path").to_string())
        .collect()
}

#[tokio::test]
async fn status_agrees_with_native_porcelain_and_changes_nothing_on_disk() {
    let f = Fixture::new(true, true).await;
    for verb in ["git.status", "git.log"] {
        f.policy(verb, "allow").await;
    }
    let clean = f.call("git.status", json!({"repo": f.repo})).await;
    assert_eq!(clean["clean"], json!(true), "{clean}");
    assert_eq!(clean["total"], json!(0));
    assert_eq!(clean["branch"]["head"], json!("work"));
    assert_eq!(clean["branch"]["oid"], json!(f.base));
    assert_eq!(clean["gate"]["source"], json!("git_write.allowed"));

    std::fs::write(f.repo.join("a.txt"), b"changed\n").expect("dirty a tracked file");
    std::fs::write(f.repo.join("fresh.txt"), b"new\n").expect("an untracked file");
    let index_before = std::fs::read(f.repo.join(".git/index")).expect("index before");
    let dirty = f
        .call("git.status", json!({"repo": f.repo, "untracked": "all"}))
        .await;
    let index_after = std::fs::read(f.repo.join(".git/index")).expect("index after");

    assert_eq!(
        index_before, index_after,
        "git.status refreshed the index; GIT_OPTIONAL_LOCKS=0 is what keeps this read inert"
    );
    assert_eq!(dirty["clean"], json!(false));
    let mut mine = status_paths(&dirty);
    let mut native = native_status_paths(&f, "all");
    mine.sort();
    native.sort();
    assert_eq!(mine, native, "khive and native porcelain disagree");
    assert_eq!(dirty["total"], json!(native.len()));
    assert_eq!(dirty["truncated"], json!(false));
    assert!(
        status_paths(&dirty).contains(&"fresh.txt".to_string()),
        "untracked=all must report an untracked file: {dirty}"
    );
}

#[tokio::test]
async fn status_total_counts_the_whole_repository_even_when_entries_are_capped() {
    let f = Fixture::new(true, true).await;
    f.policy("git.status", "allow").await;
    for n in 0..7 {
        std::fs::write(f.repo.join(format!("u{n}.txt")), b"x\n").expect("untracked file");
    }
    let capped = f
        .call(
            "git.status",
            json!({"repo": f.repo, "untracked": "all", "limit": 2}),
        )
        .await;
    assert_eq!(capped["entries"].as_array().expect("entries").len(), 2);
    assert_eq!(capped["total"], json!(7), "{capped}");
    assert_eq!(capped["truncated"], json!(true));
    assert_eq!(
        capped["clean"],
        json!(false),
        "a capped page must never report a clean repository"
    );
}

#[tokio::test]
async fn status_keeps_a_path_holding_a_newline_a_space_and_non_ascii_as_one_entry() {
    let f = Fixture::new(true, true).await;
    f.policy("git.status", "allow").await;
    // A space would split a whitespace parser, a newline would split a line parser, and the CJK
    // and accented characters catch a byte-wise parser that assumes ASCII. `-z` is the only
    // reason all three survive as one record.
    let awkward = "a file\nwith a newline \u{4e2d}\u{6587} caf\u{e9}.txt";
    std::fs::write(f.repo.join(awkward), b"x\n").expect("awkward untracked name");
    let result = f
        .call("git.status", json!({"repo": f.repo, "untracked": "all"}))
        .await;
    assert_eq!(result["total"], json!(1), "{result}");
    assert_eq!(status_paths(&result), vec![awkward.to_string()]);
}

#[tokio::test]
async fn status_reports_a_detached_head_as_a_null_branch_beside_a_real_sha() {
    let f = Fixture::new(true, true).await;
    f.policy("git.status", "allow").await;
    let attached = f.call("git.status", json!({"repo": f.repo})).await;
    assert_eq!(
        attached["branch"]["head"],
        json!("work"),
        "control: {attached}"
    );

    output(
        command(
            &f.git,
            &f.repo,
            &["checkout", "-q", "--detach", "HEAD"],
            true,
        ),
        None,
    );
    let detached = f.call("git.status", json!({"repo": f.repo})).await;
    assert_eq!(
        detached["branch"]["head"],
        json!(null),
        "a detached head is an absent branch name, never the porcelain sentinel: {detached}"
    );
    assert_eq!(
        detached["branch"]["oid"],
        json!(f.base),
        "the sha is still readable while detached: {detached}"
    );
}

#[tokio::test]
async fn log_agrees_with_native_rev_list_and_bounds_its_page() {
    let f = Fixture::new(true, true).await;
    f.policy("git.log", "allow").await;
    let page = f
        .call("git.log", json!({"repo": f.repo, "limit": 10}))
        .await;
    let mine: Vec<String> = page["commits"]
        .as_array()
        .expect("commits array")
        .iter()
        .map(|c| c["sha"].as_str().expect("sha").to_string())
        .collect();
    let native: Vec<String> = f
        .git_text(&["rev-list", "-n", "10", "HEAD"])
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(mine, native, "{page}");
    let head = &page["commits"][0];
    assert_eq!(head["sha"], json!(f.base));
    assert!(
        head["subject"].as_str().is_some_and(|s| !s.is_empty()),
        "subject must be populated: {head}"
    );
    assert!(
        head["committed_at"]
            .as_str()
            .is_some_and(|s| s.contains('T')),
        "committed_at must be ISO 8601: {head}"
    );

    let one = f.call("git.log", json!({"repo": f.repo, "limit": 1})).await;
    assert_eq!(one["commits"].as_array().expect("commits").len(), 1);
    assert_eq!(one["truncated"], json!(true));
    for bad in [json!(0), json!(501), json!("10"), json!(null)] {
        let error = f
            .err("git.log", json!({"repo": f.repo, "limit": bad}))
            .await;
        assert!(error.contains("invalid_params"), "limit {bad}: {error}");
    }
}

#[tokio::test]
async fn log_treats_a_path_filter_literally_rather_than_as_a_glob() {
    let f = Fixture::new(true, true).await;
    f.policy("git.log", "allow").await;
    std::fs::write(f.repo.join("*.txt"), b"star\n").expect("a file literally named *.txt");
    output(
        command(&f.git, &f.repo, &["add", "--", "*.txt"], true),
        None,
    );
    output(
        command(
            &f.git,
            &f.repo,
            &["commit", "-q", "-m", "the literal star file"],
            true,
        ),
        None,
    );
    let literal = f
        .call(
            "git.log",
            json!({"repo": f.repo, "path": "*.txt", "limit": 50}),
        )
        .await;
    let commits = literal["commits"].as_array().expect("commits array");
    assert_eq!(
        commits.len(),
        1,
        "a glob would also match a.txt and removed.txt: {literal}"
    );
    assert_eq!(commits[0]["subject"], json!("the literal star file"));
}

#[tokio::test]
async fn status_and_log_refuse_off_allowlist_and_on_deny_and_write_no_receipt() {
    // No status/log grant is made here on purpose. The allowlist is consulted before the policy,
    // so the off-allowlist arm below refuses without one, which is what proves that ordering.
    let f = Fixture::new(true, true).await;
    let before = f
        .call("git.receipts", json!({"limit":500,"offset":0}))
        .await["receipts"]
        .as_array()
        .expect("receipts array")
        .len();

    let outside = f.dir.path().join("not-listed");
    std::fs::create_dir(&outside).expect("unlisted directory");
    for verb in ["git.status", "git.log"] {
        let error = f.err(verb, json!({"repo": outside})).await;
        assert!(
            error.contains("repo_not_allowlisted"),
            "{verb} off the allowlist: {error}"
        );
    }
    // Control first, and it has to be first: tool policies are append-only rows and a deny is not
    // reversible by a later allow, so a control placed after the deny arm would fail on a healthy
    // pack. The same repo and the same call shape succeed while the decision is allow, which is
    // what makes the refusals below the policy's doing rather than a broken fixture.
    for verb in ["git.status", "git.log"] {
        f.policy(verb, "allow").await;
        f.call(verb, json!({"repo": f.repo})).await;
    }
    for verb in ["git.status", "git.log"] {
        f.policy(verb, "deny").await;
        let error = f.err(verb, json!({"repo": f.repo})).await;
        assert!(error.contains("policy_denied"), "{verb} denied: {error}");
    }

    let after = f
        .call("git.receipts", json!({"limit":500,"offset":0}))
        .await["receipts"]
        .as_array()
        .expect("receipts array")
        .len();
    assert_eq!(
        before, after,
        "git.status and git.log write no receipt, allowed or refused"
    );
}

#[tokio::test]
async fn init_makes_an_allowlisted_empty_directory_a_repository_and_records_a_receipt() {
    let f = Fixture::new(true, true).await;
    f.policy("git.init", "allow").await;
    f.policy("git.status", "allow").await;
    assert!(
        !f.blank.join(".git").exists(),
        "precondition: the target is not a repository yet"
    );
    let before = f
        .call("git.receipts", json!({"limit":500,"offset":0}))
        .await["receipts"]
        .as_array()
        .expect("receipts array")
        .len();

    let created = f
        .call("git.init", json!({"repo": f.blank, "branch": "trunk"}))
        .await;
    assert_eq!(created["branch"], json!("trunk"), "{created}");
    assert!(created["receipt_id"].as_str().is_some(), "{created}");
    assert!(f.blank.join(".git").exists(), "the repository was created");
    assert!(
        !f.blank.join(".git/hooks/pre-commit.sample").exists(),
        "--template= must leave no sample hooks behind"
    );

    let after = f
        .call("git.receipts", json!({"limit":500,"offset":0}))
        .await["receipts"]
        .as_array()
        .expect("receipts array")
        .len();
    assert_eq!(after, before + 1, "git.init is a write and takes a receipt");

    // The new repository is readable through the same pack that made it.
    let status = f.call("git.status", json!({"repo": f.blank})).await;
    assert_eq!(status["branch"]["head"], json!("trunk"), "{status}");
    assert_eq!(
        status["branch"]["oid"],
        json!(null),
        "an unborn branch reports a null oid, not the porcelain sentinel: {status}"
    );
}

#[tokio::test]
async fn init_refuses_a_target_that_already_holds_a_repository_and_leaves_it_untouched() {
    let f = Fixture::new(true, true).await;
    f.policy("git.init", "allow").await;
    let head_before = f.git_text(&["rev-parse", "HEAD"]);
    let config_before = std::fs::read(f.repo.join(".git/config")).expect("config before");

    let error = f.err("git.init", json!({"repo": f.repo})).await;
    assert!(
        error.contains("already_initialized"),
        "reinitializing a live repository must refuse: {error}"
    );
    assert_eq!(
        head_before,
        f.git_text(&["rev-parse", "HEAD"]),
        "the refused init moved HEAD"
    );
    assert_eq!(
        config_before,
        std::fs::read(f.repo.join(".git/config")).expect("config after"),
        "the refused init rewrote configuration in place, which is the reason it refuses"
    );

    // Control: the same call shape succeeds against a target that holds no repository, so the
    // refusal above is the precondition and not a broken fixture.
    f.call("git.init", json!({"repo": f.blank})).await;
}

#[tokio::test]
async fn init_refuses_a_directory_that_is_not_allowlisted() {
    let f = Fixture::new(true, true).await;
    f.policy("git.init", "allow").await;
    let outside = f.dir.path().join("unlisted-target");
    std::fs::create_dir(&outside).expect("unlisted directory");
    let error = f.err("git.init", json!({"repo": outside})).await;
    assert!(
        error.contains("repo_not_allowlisted"),
        "the operator's allowlist is what decides where a repository may appear: {error}"
    );
    assert!(
        !outside.join(".git").exists(),
        "a refused init created a repository anyway"
    );
}

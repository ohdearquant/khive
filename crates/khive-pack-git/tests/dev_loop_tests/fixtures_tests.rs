const ACTOR: &str = "actor:dev-loop";
/// Verbs the fixture allows at construction when the tool pack is present.
const FIXTURE_ALLOWED_VERBS: [&str; 6] = [
    "git.receipts",
    "git.gates",
    "git.checkout",
    "git.diff",
    "git.reconcile",
    "git.update_ref",
];
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

/// Directory name of the PATH shim one case installs. Named here because the resolver below has
/// to know it.
const SHIM_DIR: &str = "shim-bin";

/// The native git binary, resolved from PATH but never from a fixture's shim directory.
///
/// One case puts a shim on PATH for the length of its own body and takes it off again. PATH is
/// process-global, so a fixture built concurrently resolves git through the shim and CACHES that
/// path; the shim's temporary directory is then removed with its fixture and every later spawn of
/// the cached path fails with ENOENT. Skipping the shim directory here keeps that case's PATH
/// mutation invisible to every other FIXTURE.
///
/// It does not keep it invisible to the code under test, which is the other half and the reason
/// every case in this file now carries `git_dev_loop_env`. `base_command` builds
/// `Command::new("git")` and hands the process's current PATH to the child, so the verb resolves
/// its program by name at spawn, against whatever PATH the process holds at that instant. A case
/// running beside a shim case therefore resolves the shim, or a path inside a fixture directory
/// being deleted as that case ends, and reports `git_failed` for a reason nothing in its own body
/// explains. The window is scheduler time, so it opens under load and stays shut on a quiet
/// machine: 2 of 13 runs failed under one busy loop per core, 0 of 19 without them (#2515).
fn git_program() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .filter(|dir| dir.file_name() != Some(std::ffi::OsStr::new(SHIM_DIR)))
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
            for verb in FIXTURE_ALLOWED_VERBS {
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

    async fn indexed_tree(&self) -> String {
        let blobs = tree::blob_store(&self.rt).expect("blob store");
        let listing = self.git_bytes(&["ls-files", "-s", "-z"]);
        let mut entries = Vec::new();
        for record in listing
            .split(|byte| *byte == 0)
            .filter(|row| !row.is_empty())
        {
            let tab = record.iter().position(|byte| *byte == b'\t').unwrap();
            let metadata = std::str::from_utf8(&record[..tab]).unwrap();
            let fields: Vec<_> = metadata.split_whitespace().collect();
            assert_eq!(fields.len(), 3);
            assert_eq!(fields[2], "0", "fixture must have no unmerged entries");
            let mode = match fields[0] {
                "100644" => 644,
                "100755" => 755,
                "120000" => 120000,
                mode => panic!("unexpected index mode {mode}"),
            };
            let path = std::str::from_utf8(&record[tab + 1..]).unwrap();
            let bytes = self.git_bytes(&["cat-file", "blob", fields[1]]);
            let content = blobs.put(bytes).await.expect("store index blob");
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

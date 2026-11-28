use super::*;

const CONFIGURED: &str = "lambda:configured";
const PROJECT: &str = "lambda:project";
const PINNED: &str = "lambda:pinned";
const OWNED: &str = "project:owned";
const VISIBLE: &str = "project:visible";

struct RestoreCwd(PathBuf);

impl Drop for RestoreCwd {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

struct Fixture {
    db: String,
    config: PathBuf,
    project_config: PathBuf,
}

impl Fixture {
    fn write_config(&self, owned: bool) {
        let declaration = if owned {
            format!("owned_namespaces = [\"{OWNED}\"]\n")
        } else {
            String::new()
        };
        std::fs::write(
            &self.config,
            format!(
                "[actor]\nid = \"{CONFIGURED}\"\n{declaration}visible_namespaces = [\"{PROJECT}\", \"{VISIBLE}\"]\n"
            ),
        )
        .expect("write configured actor and declaration");
    }

    fn write_project(&self, actor: &str) {
        std::fs::write(&self.project_config, format!("[actor]\nid = \"{actor}\"\n"))
            .expect("write project actor");
    }

    fn args(&self, actor: Option<&str>) -> ExecArgs {
        ExecArgs {
            read_only: false,
            plan: false,
            ops: Some("verbs()".to_string()),
            pending_events: false,
            db: Some(self.db.clone()),
            config: None,
            namespace: "local".to_string(),
            actor: actor.map(str::to_string),
            expect_actor: None,
            presentation: None,
            output_format: None,
            verbose: false,
            save_file: None,
            ops_file: None,
            dry_run: false,
            serial: false,
            atomic: false,
            atomic_max_ops: None,
            strict: true,
        }
    }

    fn configured(&self) -> RuntimeConfig {
        khive_mcp::serve::resolve_runtime_config(RuntimeConfigInputs {
            db: Some(&self.db),
            config: Some(&self.config),
            namespace: namespace(CONFIGURED),
            namespace_explicit: true,
            actor_explicit: true,
            no_embed: false,
            packs: Some(vec!["kg".to_string()]),
            brain_profile: None,
        })
        .expect("load explicit configured-actor fixture through MCP")
    }

    fn assert_provenance(&self) {
        let configured = self.configured();
        assert_eq!(configured.actor_id.as_deref(), Some(CONFIGURED));
        assert_eq!(configured.owned_namespaces, vec![namespace(OWNED)]);
        let (project, anchor) = resolve_exec_runtime_config(&self.args(None))
            .expect("resolve actual exec path with separate project actor");
        assert_eq!(project.actor_id.as_deref(), Some(PROJECT));
        assert_eq!(project.default_namespace, Namespace::local());
        assert_eq!(anchor.as_deref(), project.db_path.as_deref());
        assert!(project.visible_namespaces.contains(&namespace(PROJECT)));
        assert!(project.visible_namespaces.contains(&namespace(VISIBLE)));
        // Qualify the production exec loader with config=None as well as the
        // explicit MCP config above. No-pin A keeps S even under the exec-only
        // premature-finalization control, so this cannot mask its target case.
        self.write_project(CONFIGURED);
        let (configured, _) = resolve_exec_runtime_config(&self.args(None))
            .expect("qualify actual exec database-anchor declaration");
        assert_eq!(configured.actor_id.as_deref(), Some(CONFIGURED));
        assert_eq!(configured.owned_namespaces, vec![namespace(OWNED)]);
        self.write_project(PROJECT);
        let (restored, _) = resolve_exec_runtime_config(&self.args(None))
            .expect("requalify independent project B after restoring its config");
        assert_eq!(restored.actor_id.as_deref(), Some(PROJECT));
        assert!(
            !PathBuf::from(&self.db).exists(),
            "configuration-only qualification must not open a store"
        );
        // The drop-omission control must reach the final named assertion;
        // fixture preparation therefore does not assert B's ownership list.
    }
}

fn namespace(value: &str) -> Namespace {
    Namespace::parse(value).expect("valid fixture namespace")
}

fn with_fixture(test: impl FnOnce(&Fixture)) {
    let root = tempfile::tempdir().expect("private config fixture");
    // This guard drops before root, including when an assertion unwinds.
    // HOME remains the private, untouched one supplied by run_in_child.
    let _restore = RestoreCwd(std::env::current_dir().expect("initial cwd"));
    let anchor = root.path().join("anchor").join(".khive");
    let project = root.path().join("project");
    std::fs::create_dir_all(&anchor).expect("private database-anchor config directory");
    std::fs::create_dir_all(project.join(".khive")).expect("private project config directory");
    let fixture = Fixture {
        db: anchor
            .join("config-only.db")
            .to_str()
            .expect("UTF-8 fixture path")
            .to_string(),
        config: anchor.join("config.toml"),
        project_config: project.join(".khive").join("config.toml"),
    };
    fixture.write_config(true);
    fixture.write_project(PROJECT);
    std::env::set_var("KHIVE_PACKS", "kg");
    std::env::set_current_dir(&project).expect("enter separate project directory");
    fixture.assert_provenance();
    test(&fixture);
}

#[test]
fn exec_pin_retains_configured_ownership_after_project_override() {
    if crate::test_process::run_in_child() {
        return;
    }
    with_fixture(|fixture| {
        let (config, _) = resolve_exec_runtime_config(&fixture.args(Some(CONFIGURED)))
            .expect("resolve actual exec config returning to configured actor");
        assert_eq!(config.actor_id.as_deref(), Some(CONFIGURED));
        assert_eq!(config.default_namespace, Namespace::local());
        assert_eq!(
            config.owned_namespaces,
            vec![namespace(OWNED)],
            "C2_EXEC_PIN_RETAINS_DECLARED_GRANTS"
        );
    });
}

#[test]
fn exec_different_actor_drops_ownership_and_preserves_visibility() {
    if crate::test_process::run_in_child() {
        return;
    }
    with_fixture(|fixture| {
        let (config, _) = resolve_exec_runtime_config(&fixture.args(Some(PINNED)))
            .expect("resolve actual exec config for different actor");
        assert_eq!(config.actor_id.as_deref(), Some(PINNED));
        assert_eq!(config.default_namespace, Namespace::local());
        assert!(!config.visible_namespaces.contains(&namespace(PROJECT)));
        assert!(config.visible_namespaces.contains(&namespace(VISIBLE)));
        assert_eq!(
            config
                .visible_namespaces
                .iter()
                .filter(|item| **item == namespace(PINNED))
                .count(),
            1
        );
        assert!(
            config.owned_namespaces.is_empty(),
            "C1_EXEC_DIFFERENT_ACTOR_DROPS_GRANTS"
        );
    });
}

#[test]
fn exec_local_pin_is_anonymous_and_drops_configured_ownership() {
    if crate::test_process::run_in_child() {
        return;
    }
    with_fixture(|fixture| {
        let (config, _) = resolve_exec_runtime_config(&fixture.args(Some("local")))
            .expect("resolve anonymous exec pin");
        assert!(config.actor_id.is_none());
        assert_eq!(config.default_namespace, Namespace::local());
        assert!(config.owned_namespaces.is_empty());
        assert!(!config.visible_namespaces.contains(&namespace(PROJECT)));
        assert!(config.visible_namespaces.contains(&namespace(VISIBLE)));
    });
}

#[test]
fn exec_no_pin_keeps_existing_project_and_configured_actor_behavior() {
    if crate::test_process::run_in_child() {
        return;
    }
    with_fixture(|fixture| {
        let (project, _) =
            resolve_exec_runtime_config(&fixture.args(None)).expect("no-pin project");
        assert_eq!(project.actor_id.as_deref(), Some(PROJECT));
        assert!(project.owned_namespaces.is_empty());
        assert_eq!(project.default_namespace, Namespace::local());
        fixture.write_project(CONFIGURED);
        let (configured, _) =
            resolve_exec_runtime_config(&fixture.args(None)).expect("no-pin configured actor");
        assert_eq!(configured.actor_id.as_deref(), Some(CONFIGURED));
        assert_eq!(configured.owned_namespaces, vec![namespace(OWNED)]);
    });
}

#[test]
fn exec_pin_preserves_storage_namespace_without_inventing_ownership() {
    if crate::test_process::run_in_child() {
        return;
    }
    with_fixture(|fixture| {
        fixture.write_config(false);
        assert!(fixture.configured().owned_namespaces.is_empty());
        let mut args = fixture.args(Some(PINNED));
        args.namespace = "project:storage".to_string();
        let (config, _) = resolve_exec_runtime_config(&args).expect("nonlocal storage namespace");
        assert_eq!(config.default_namespace, namespace("project:storage"));
        assert_eq!(config.actor_id.as_deref(), Some(PINNED));
        assert!(config.owned_namespaces.is_empty());
    });
}

#[test]
fn exec_expectation_checks_the_final_pinned_identity() {
    if crate::test_process::run_in_child() {
        return;
    }
    with_fixture(|fixture| {
        let mut args = fixture.args(Some(PINNED));
        args.expect_actor = Some(PINNED.to_string());
        let (config, _) = resolve_exec_runtime_config(&args).expect("matching final expectation");
        assert_eq!(config.actor_id.as_deref(), Some(PINNED));
        args.expect_actor = Some(PROJECT.to_string());
        let error = match resolve_exec_runtime_config(&args) {
            Err(error) => error,
            Ok(_) => panic!("displaced project actor must not satisfy expectation"),
        };
        let refusal = error
            .downcast_ref::<ExecRefusal>()
            .expect("typed expectation refusal");
        assert!(matches!(refusal.reason, RefusalReason::ExpectActorMismatch));
    });
}

#[test]
fn exec_project_config_error_precedes_invalid_actor() {
    if crate::test_process::run_in_child() {
        return;
    }
    with_fixture(|fixture| {
        const INVALID_ACTOR: &str = "bad actor";
        assert!(Namespace::parse(INVALID_ACTOR).is_err());
        std::fs::write(&fixture.project_config, "[actor]\nid = [")
            .expect("write malformed private project config");
        let args = fixture.args(Some(INVALID_ACTOR));
        let error = match resolve_exec_runtime_config(&args) {
            Err(error) => error,
            Ok(_) => panic!("malformed project config must be refused"),
        };
        let message = error.to_string();
        assert!(message.contains("config error"), "{message}");
        assert!(
            message.contains(fixture.project_config.to_str().unwrap()),
            "{message}"
        );
        assert!(!message.contains("invalid --actor"), "{message}");
    });
}

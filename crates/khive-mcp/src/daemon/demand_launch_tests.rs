use super::*;

#[test]
fn shared_builder_selects_demand_and_recognizes_both_launch_modes() {
    let packs = vec!["kg".to_owned(), "schedule".to_owned()];
    let command = daemon_launch_command(
        std::path::Path::new("kkernel"),
        Some(std::path::Path::new("fixture.toml")),
        Some("fixture.db"),
        Some(&packs),
    );
    let args: Vec<_> = command
        .get_args()
        .map(|arg| arg.to_str().unwrap())
        .collect();
    assert_eq!(
        args,
        vec![
            "mcp",
            "--daemon",
            "--lifetime",
            "demand",
            "--config",
            "fixture.toml",
            "--db",
            "fixture.db",
            "--pack",
            "kg",
            "--pack",
            "schedule"
        ]
    );
    assert!(argv_is_khive_daemon(
        "kkernel mcp --daemon --lifetime demand --config fixture.toml"
    ));
    assert!(argv_is_khive_daemon(
        "kkernel mcp --daemon --lifetime persistent"
    ));
    assert!(argv_is_khive_daemon("kkernel mcp --daemon"));
    assert!(!argv_is_khive_daemon("kkernel exec stats()"));
}

#[test]
fn spawn_overrides_preserve_discovered_and_explicit_configuration() {
    let discovered = std::path::Path::new("discovered.toml");
    let explicit = std::path::Path::new("explicit.toml");

    assert_eq!(
        super::select_daemon_spawn_overrides(None, Some("fixture.db"), Some(discovered), true),
        (Some(discovered.to_path_buf()), None),
    );
    assert_eq!(
        super::select_daemon_spawn_overrides(
            Some(explicit),
            Some(":memory:"),
            Some(discovered),
            true,
        ),
        (Some(explicit.to_path_buf()), Some(":memory:".to_owned())),
    );
    assert_eq!(
        super::select_daemon_spawn_overrides(None, Some("fixture.db"), Some(discovered), false),
        (None, Some("fixture.db".to_owned())),
    );
}

fn isolated_launch_environment() -> bool {
    use std::os::unix::fs::PermissionsExt;

    let mut private_home = None;
    let parent = khive_storage::test_support::run_exact_test_in_child(
        "KHIVE_LAUNCH_PREFLIGHT_TEST",
        false,
        |command| {
            let home = tempfile::Builder::new()
                .prefix("launch-home-")
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir_in("/tmp")
                .unwrap();
            for (key, _) in std::env::vars_os() {
                if key.to_string_lossy().starts_with("KHIVE_") {
                    command.env_remove(key);
                }
            }
            command
                .env_remove("LATTICE_MODEL_CACHE")
                .env("HOME", home.path())
                .env("USERPROFILE", home.path())
                .env("KHIVE_TEST_HARNESS", "1")
                .env("KHIVE_NO_EMBED", "true")
                .current_dir(home.path());
            private_home = Some(home);
        },
    );
    drop(private_home);
    parent
}

#[test]
fn launch_preflight_parser_consumes_mcp_as_program_name() {
    use clap::Parser;

    if isolated_launch_environment() {
        return;
    }
    let config = std::path::Path::new("selected.toml");
    let packs = vec!["kg".to_owned(), "schedule".to_owned()];
    let command = daemon_launch_command(
        std::path::Path::new("kkernel"),
        Some(config),
        Some("selected.db"),
        Some(&packs),
    );
    assert_eq!(command.get_args().next(), Some(std::ffi::OsStr::new("mcp")));
    let args = crate::args::Args::try_parse_from(command.get_args())
        .expect("mcp occupies clap's program-name slot");
    assert!(args.daemon);
    assert_eq!(args.lifetime, Some(crate::args::LifetimeMode::Demand));
    assert_eq!(args.config.as_deref(), Some(config));
    assert_eq!(args.db.as_deref(), Some("selected.db"));
    assert_eq!(args.pack, packs);
}

fn launch_fixture_files(root: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            paths.extend(launch_fixture_files(&entry.path()));
        } else {
            paths.push((entry.path(), std::fs::read(entry.path()).unwrap()));
        }
    }
    paths.sort();
    paths
}

#[tokio::test(start_paused = true)]
async fn public_forwarders_refuse_oversized_events_socket_before_recovery() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    if isolated_launch_environment() {
        return;
    }
    let _cleanup = super::test_harness::RecoveryTestGuard::new();
    reset_counters();
    let directory = tempfile::Builder::new()
        .prefix("launch-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in("/tmp")
        .unwrap();
    let root = directory.path().canonicalize().unwrap();
    // SAFETY: sockaddr_un consists of integer fields and a character array.
    let address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let capacity = address.sun_path.len();
    let socket_name = "main.db.events.sock";
    let padding = capacity
        .checked_sub(root.as_os_str().as_bytes().len() + 2 + socket_name.len())
        .expect("short private fixture root");
    let database_dir = root.join("d".repeat(padding));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&database_dir)
        .unwrap();
    let main = database_dir.join("main.db");
    let events_socket = khive_runtime::events_split::events_socket_path_beside(
        &khive_runtime::events_split::events_db_path_beside(&main),
    );
    assert_eq!(events_socket.as_os_str().as_bytes().len(), capacity);
    let config = root.join("khive.toml");
    std::fs::write(
        &config,
        format!(
            "engines = []\n[runtime]\npacks = [\"kg\"]\n\n[[backends]]\nname = \"main\"\nkind = \"sqlite\"\npath = {main:?}\n"
        ),
    )
    .unwrap();
    let marker = root.join("supervisor");
    // If preflight is removed, this claim and the earlier caller deadline
    // produce a bounded refusal without reaching the real process launcher.
    std::fs::write(&marker, "example.preflight\n0\n10\n").unwrap();
    for (name, path) in [
        ("KHIVE_CONFIG", config.clone()),
        ("KHIVE_SOCKET", root.join("daemon.sock")),
        ("KHIVE_PID", root.join("daemon.pid")),
        ("KHIVE_LOCK", root.join("boot.lock")),
        ("KHIVE_RECOVERER_LOCK", root.join("recoverer.lock")),
        ("KHIVE_SUPERVISOR_MARKER", marker.clone()),
    ] {
        std::env::set_var(name, path);
    }
    let before = launch_fixture_files(&root);
    let frame = DaemonRequestFrame {
        ops: "stats()".into(),
        namespace: "local".into(),
        config_id: "preflight-before-recovery".into(),
        protocol_version: PROTOCOL_VERSION,
        ..Default::default()
    };
    let packs = vec!["kg".to_owned()];
    for strict in ["0", "1"] {
        std::env::set_var("KHIVE_DAEMON_STRICT", strict);
        for bare in [false, true] {
            let started = tokio::time::Instant::now();
            let forwarded = async {
                if bare {
                    forward_or_spawn(&frame).await
                } else {
                    forward_or_spawn_with_config_and_packs(
                        &frame,
                        Some(&config),
                        None,
                        Some(&packs),
                    )
                    .await
                }
            };
            let error = tokio::time::timeout(
                Duration::from_secs(2),
                khive_storage::scope_request_read_deadline(Duration::from_millis(250), forwarded),
            )
            .await
            .expect("public forwarding settles within its deadline")
            .expect("preflight refusal cannot fall back locally")
            .expect_err("oversized events socket must be refused");
            let data = error.data.as_ref().expect("structured preflight refusal");
            assert_eq!(data["reason"], "daemon_launch_preflight");
            assert_eq!(data["domain_disposition"], "not_committed");
            assert!(
                error.message.contains(&format!("{events_socket:?}")),
                "{error}"
            );
            assert!(
                error.message.contains(&format!("{capacity} path bytes")),
                "{error}"
            );
            assert!(
                error
                    .message
                    .contains(&format!("{} including NUL", capacity + 1)),
                "{error}"
            );
            assert!(
                error
                    .message
                    .contains(&format!("sun_path limit of {capacity} bytes")),
                "{error}"
            );
            assert_eq!(tokio::time::Instant::now(), started, "no recovery wait");
            assert_eq!(SPAWN_COUNT.load(Ordering::SeqCst), 0);
            assert_eq!(KILL_COUNT.load(Ordering::SeqCst), 0);
            assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 0);
            assert_eq!(launch_fixture_files(&root), before);
        }
    }
    super::launch::preflight_daemon_launch(Some(&config), Some(":memory:"), Some(&packs))
        .expect("the same launch configuration remains valid with the memory override");
    assert_eq!(launch_fixture_files(&root), before);
}

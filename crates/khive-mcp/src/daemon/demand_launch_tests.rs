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

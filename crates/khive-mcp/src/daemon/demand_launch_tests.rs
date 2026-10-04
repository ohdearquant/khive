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

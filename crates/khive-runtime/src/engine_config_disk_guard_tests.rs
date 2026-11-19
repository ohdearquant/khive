#[test]
fn disk_guard_backend_configuration_validates_memory_and_deadline() {
    let parse = |fields: &str| {
        toml::from_str::<KhiveConfig>(&format!("[[backends]]\nname='main'\n{fields}")).unwrap()
    };
    for deadline in [0, 99, 10_001] {
        let config = parse(&format!(
            "kind='sqlite'\npath='main.db'\ndisk_guard_deadline_ms={deadline}\n"
        ));
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidBackendDiskGuard { .. })
        ));
    }
    let memory = parse("kind='memory'\ndisk_reserve_bytes=1\n");
    assert!(matches!(
        memory.validate(),
        Err(ConfigError::InvalidBackendDiskGuard { .. })
    ));
    let zero = parse("kind='memory'\ndisk_reserve_bytes=0\n");
    zero.validate().unwrap();
    let file = parse(
        "kind='sqlite'\npath='main.db'\ndisk_reserve_bytes=4294967296\n\
             disk_guard_deadline_ms=250\n",
    );
    file.validate().unwrap();
    let policy = file.backends[0]
        .resolve_disk_guard(&khive_db::DiskGuardEnvironment::default())
        .unwrap()
        .unwrap();
    assert_eq!(
        (policy.reserve_bytes, policy.guard_deadline_ms),
        (4_294_967_296, 250)
    );
}

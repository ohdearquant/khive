#[test]
fn external_inbound_disabled_ignores_missing_or_invalid_routing_environment() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _saved = EnvSnapshot::capture(&[
        "KHIVE_EMAIL_EXTERNAL_INBOUND_ACTOR",
        "KHIVE_EMAIL_INBOUND_ROUTES",
    ]);
    std::env::remove_var("KHIVE_EMAIL_EXTERNAL_INBOUND_ACTOR");
    std::env::set_var("KHIVE_EMAIL_INBOUND_ROUTES", "invalid route");
    assert_eq!(ExternalInboundConfig::from_env(false).unwrap(), None);
    assert!(ExternalInboundConfig::from_env(true).is_err());
}

#[test]
fn external_inbound_enabled_resolves_required_actor_and_exact_routes() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _saved = EnvSnapshot::capture(&[
        "KHIVE_EMAIL_EXTERNAL_INBOUND_ACTOR",
        "KHIVE_EMAIL_INBOUND_ROUTES",
    ]);
    std::env::set_var("KHIVE_EMAIL_EXTERNAL_INBOUND_ACTOR", " lambda:inbound ");
    std::env::set_var(
        "KHIVE_EMAIL_INBOUND_ROUTES",
        "Support@Example.com=lambda:support",
    );
    let config = ExternalInboundConfig::from_env(true).unwrap().unwrap();
    assert_eq!(config.actor, "lambda:inbound");
    assert_eq!(
        config
            .routes
            .actor_for(&MailAddress::parse("support@example.com").unwrap()),
        Some("lambda:support")
    );
    std::env::set_var(
        "KHIVE_EMAIL_INBOUND_ROUTES",
        "support@example.com=lambda:a,SUPPORT@example.com=lambda:b",
    );
    assert!(ExternalInboundConfig::from_env(true).is_err());
    std::env::set_var("KHIVE_EMAIL_EXTERNAL_INBOUND_ACTOR", "  ");
    assert!(ExternalInboundConfig::from_env(true).is_err());
}

#[test]
fn inbound_only_switch_reads_enabled_and_refuses_unrecognised_values() {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _saved = EnvSnapshot::capture(&["KHIVE_EMAIL_EXTERNAL_INBOUND"]);
    std::env::remove_var("KHIVE_EMAIL_EXTERNAL_INBOUND");
    assert!(!ExternalInboundConfig::inbound_only_from_env().unwrap());
    for (raw, enabled) in [
        ("", false),
        ("  ", false),
        ("enabled", true),
        (" Enabled ", true),
    ] {
        std::env::set_var("KHIVE_EMAIL_EXTERNAL_INBOUND", raw);
        assert_eq!(
            ExternalInboundConfig::inbound_only_from_env().unwrap(),
            enabled,
            "{raw:?}"
        );
    }
    for raw in ["true", "1", "yes", "disabled", "enabled,outbound"] {
        std::env::set_var("KHIVE_EMAIL_EXTERNAL_INBOUND", raw);
        let error = ExternalInboundConfig::inbound_only_from_env().unwrap_err();
        assert!(
            error.to_string().contains("KHIVE_EMAIL_EXTERNAL_INBOUND")
                && error.to_string().contains("not recognised"),
            "{raw:?}: {error}"
        );
    }
}

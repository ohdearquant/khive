use super::*;
use clap::Parser;

fn runtime_config() -> khive_runtime::RuntimeConfig {
    khive_runtime::RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        actor_id: Some("test:demand-startup".to_owned()),
        packs: vec!["kg".to_owned()],
        events_split: None,
        ..Default::default()
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn configured_channels_and_schedule_are_skipped_by_real_startup() {
    let runtime = KhiveRuntime::new(runtime_config()).unwrap();
    let server = KhiveMcpServer::new(runtime.clone()).unwrap();
    let demand = Args::parse_from(["mcp", "--daemon", "--lifetime", "demand"]);
    let before = khive_runtime::daemon::background_task_count();
    let report = start_host_background_tasks(&demand, &server, Some(runtime));
    assert_eq!(
        khive_runtime::daemon::background_task_count(),
        before,
        "a configured schedule must not create its supervised worker"
    );
    assert!(report
        .skipped_components
        .contains(&"schedule-tick".to_owned()));
    #[cfg(feature = "channel-email")]
    assert!(report
        .skipped_components
        .contains(&"email_channel_poll".to_owned()));
    #[cfg(feature = "channel-telegram")]
    assert!(report
        .skipped_components
        .contains(&"telegram_channel_poll".to_owned()));
    let persistent = Args::parse_from(["mcp", "--daemon"]);
    assert!(daemon_startup_report(&persistent, &server, true)
        .skipped_components
        .is_empty());
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn events_supervision_configuration_is_idle_ineligible() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = runtime_config();
    config.events_split = Some(khive_runtime::events_split::EventsSplitConfig {
        db_path: dir.path().join("events.db"),
        socket_path: Some(dir.path().join("events.sock")),
    });
    let server = KhiveMcpServer::new(KhiveRuntime::new(config).unwrap()).unwrap();
    let demand = Args::parse_from(["mcp", "--daemon", "--lifetime", "demand"]);
    let report = daemon_startup_report(&demand, &server, false);
    assert!(report
        .idle_ineligible_reasons
        .contains(&"events_child_may_be_exclusively_owned".to_owned()));
    assert!(
        daemon_startup_report(&Args::parse_from(["mcp", "--daemon"]), &server, false)
            .idle_ineligible_reasons
            .is_empty()
    );
}

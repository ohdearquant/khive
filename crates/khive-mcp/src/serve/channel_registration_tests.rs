use super::ServeRegistrations;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use khive_channel::{Channel, ChannelEnvelope, ChannelError};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn config_args(
    path: &std::path::Path,
    database: Option<&std::path::Path>,
    comm: bool,
) -> crate::args::Args {
    use clap::Parser;
    let mut args = crate::args::Args::parse_from([
        "mcp",
        "--no-embed",
        "--actor",
        "channel-config-test",
        "--transport",
        "config-capture",
    ]);
    args.config = Some(path.to_path_buf());
    args.db = database.map(|path| path.to_string_lossy().into_owned());
    args.pack = if comm {
        vec!["kg".into(), "comm".into()]
    } else {
        vec!["kg".into()]
    };
    args
}

struct ConfigCaptureTransport {
    served: Arc<AtomicUsize>,
    observed: Arc<std::sync::Mutex<Vec<Option<toml::Value>>>>,
}

#[async_trait]
impl crate::transport::Transport for ConfigCaptureTransport {
    fn name(&self) -> &'static str {
        "config-capture"
    }
    fn about(&self) -> &'static str {
        "Local configuration fixture"
    }
    async fn serve(
        &self,
        server: crate::server::KhiveMcpServer,
        _: &crate::transport::ServeOptions,
    ) -> anyhow::Result<()> {
        assert_eq!(server.actor_id(), Some("channel-config-test"));
        let observed = self.observed.lock().unwrap();
        assert!(
            observed.len() >= 2,
            "host preparation and activated comm both validate before transport serving"
        );
        assert!(
            observed.iter().all(|value| value
                .as_ref()
                .and_then(|value| value.get("marker"))
                .and_then(toml::Value::as_integer)
                == Some(41)),
            "every validation boundary receives the literal component value"
        );
        self.served.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn registered_config_reaches_real_single_and_multi_server_builds() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    for multi in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let topology = if multi {
            "[[backends]]\nname='main'\nkind='memory'\n[[backends]]\nname='secondary'\nkind='memory'\n[packs.comm]\nbackend='secondary'\nno_embed=true\n"
        } else {
            ""
        };
        std::fs::write(
            &path,
            format!("{topology}[channel]\nunknown=17\n[channel.stand_in]\nmarker=41\n"),
        )
        .unwrap();
        let memory = std::path::Path::new(":memory:");
        let args = config_args(&path, (!multi).then_some(memory), true);
        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = observed.clone();
        let mut registrations = ServeRegistrations::new();
        registrations
            .register_config_validator(
                "stand_in",
                Arc::new(move |value| {
                    recorded.lock().unwrap().push(value.cloned());
                    Ok(())
                }),
            )
            .unwrap();
        let served = Arc::new(AtomicUsize::new(0));
        let mut transports = crate::transport::TransportRegistry::new();
        transports.register(Box::new(ConfigCaptureTransport {
            served: served.clone(),
            observed,
        }));
        super::run_with_registrations(args, &transports, registrations)
            .await
            .unwrap();
        assert_eq!(
            served.load(Ordering::SeqCst),
            1,
            "actual run reaches the local capture transport"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn invalid_registered_config_refuses_without_comm_before_database_or_resident_work() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    for multi in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let main = directory.path().join("main.db");
        let secondary = directory.path().join("secondary.db");
        let topology = if multi {
            format!("[[backends]]\nname='main'\npath={main:?}\n[[backends]]\nname='secondary'\npath={secondary:?}\n")
        } else {
            String::new()
        };
        std::fs::write(&path, format!("{topology}[channel]\nstand_in=41\n")).unwrap();
        let args = config_args(&path, (!multi).then_some(main.as_path()), false);
        assert_eq!(
            args.pack,
            ["kg"],
            "this refusal cannot depend on comm activation"
        );
        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = observed.clone();
        let mut registrations = ServeRegistrations::new();
        registrations
            .register_config_validator(
                "stand_in",
                Arc::new(move |value| {
                    recorded.lock().unwrap().push(value.cloned());
                    Err(khive_runtime::RuntimeError::InvalidInput(
                        "stand-in requires a table".into(),
                    ))
                }),
            )
            .unwrap();
        let starts = Arc::new(AtomicUsize::new(0));
        let started = starts.clone();
        registrations
            .register_resident(crate::components::ResidentComponentRegistration::new(
                "invalid-config-resident",
                move |_, _| {
                    started.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async { Ok(()) })
                },
            ))
            .unwrap();
        let channel = ProbeChannel::new("fixture", "invalid-config");
        registrations.register_channel(channel.clone()).unwrap();
        let result = super::run_with_registrations(
            args,
            &crate::transport::TransportRegistry::new(),
            registrations,
        )
        .await;
        assert!(
            matches!(result.as_ref().err().and_then(|error| error.downcast_ref::<khive_runtime::RuntimeError>()), Some(khive_runtime::RuntimeError::InvalidInput(message)) if message == "stand-in requires a table"),
            "registered validation must refuse through the real run path without comm"
        );
        assert_eq!(*observed.lock().unwrap(), [Some(toml::Value::Integer(41))]);
        assert!(
            !main.exists() && !secondary.exists(),
            "invalid component config must refuse before database construction"
        );
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        assert_eq!(channel.polls.load(Ordering::SeqCst), 0);
        assert_eq!(channel.sends.load(Ordering::SeqCst), 0);
    }
}

#[test]
#[serial_test::serial(config_ledger)]
fn config_installation_refuses_duplicates_and_validation_without_partial_assignment() {
    let mut config = khive_runtime::RuntimeConfig::no_embeddings();
    config
        .channel
        .insert("existing".into(), toml::Value::Integer(7));
    let originals = Arc::new(AtomicUsize::new(0));
    let original_calls = originals.clone();
    config
        .channel_config_validators
        .register(
            "existing",
            Arc::new(move |value| {
                assert_eq!(value.and_then(toml::Value::as_integer), Some(7));
                original_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
        )
        .unwrap();
    let proposed = Arc::new(AtomicUsize::new(0));
    let proposed_calls = proposed.clone();
    let mut duplicate = ServeRegistrations::new();
    duplicate
        .register_config_validator(
            "new",
            Arc::new(move |_| {
                proposed_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
        )
        .unwrap();
    duplicate
        .register_config_validator("existing", Arc::new(|_| Ok(())))
        .unwrap();
    assert!(
        duplicate.install_runtime_config(&mut config).is_err(),
        "existing validators cannot be replaced"
    );
    config
        .channel_config_validators
        .validate(&config.channel)
        .unwrap();
    assert_eq!(originals.load(Ordering::SeqCst), 1);
    assert_eq!(
        proposed.load(Ordering::SeqCst),
        0,
        "duplicate refusal installs none of the proposed callbacks"
    );

    let mut invalid = ServeRegistrations::new();
    invalid
        .register_config_validator(
            "new",
            Arc::new(|value| {
                assert!(value.is_none(), "absent component value remains absent");
                Err(khive_runtime::RuntimeError::InvalidInput(
                    "invalid absent component".into(),
                ))
            }),
        )
        .unwrap();
    assert!(
        invalid
            .register_config_validator("new", Arc::new(|_| Ok(())))
            .is_err(),
        "duplicate registration retains the first callback"
    );
    assert!(invalid
        .register_config_validator("", Arc::new(|_| Ok(())))
        .is_err());
    assert!(
        matches!(invalid.install_runtime_config(&mut config).unwrap_err().downcast_ref::<khive_runtime::RuntimeError>(), Some(khive_runtime::RuntimeError::InvalidInput(message)) if message == "invalid absent component")
    );
    config
        .channel_config_validators
        .validate(&config.channel)
        .unwrap();
    assert_eq!(
        config.channel.get("existing"),
        Some(&toml::Value::Integer(7))
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn prebuilt_server_refuses_validator_bundle_before_validation_or_startup() {
    use clap::Parser;
    let server =
        crate::server::KhiveMcpServer::new(khive_runtime::KhiveRuntime::memory().unwrap()).unwrap();
    let args = crate::args::Args::parse_from(["mcp"]);
    let validations = Arc::new(AtomicUsize::new(0));
    let captured = validations.clone();
    let mut registrations = ServeRegistrations::new();
    registrations
        .register_config_validator(
            "stand_in",
            Arc::new(move |_| {
                captured.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
        )
        .unwrap();
    let starts = Arc::new(AtomicUsize::new(0));
    let started = starts.clone();
    registrations
        .register_resident(crate::components::ResidentComponentRegistration::new(
            "prebuilt-resident",
            move |_, _| {
                started.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            },
        ))
        .unwrap();
    let channel = ProbeChannel::new("fixture", "prebuilt");
    registrations.register_channel(channel.clone()).unwrap();
    let result = super::serve_server_with_registrations(
        server,
        &args,
        &crate::transport::TransportRegistry::new(),
        None,
        None,
        registrations,
    )
    .await;
    assert!(
        result
            .as_ref()
            .err()
            .is_some_and(|error| error.to_string().contains("use run_with_registrations")),
        "prebuilt serving must refuse build-time callbacks with an actionable error"
    );
    assert_eq!(
        validations.load(Ordering::SeqCst),
        0,
        "prebuilt refusal must not retroactively validate"
    );
    assert_eq!(starts.load(Ordering::SeqCst), 0);
    assert_eq!(channel.polls.load(Ordering::SeqCst), 0);
}

#[cfg(unix)]
#[test]
fn config_callbacks_require_a_local_stdio_host_without_changing_environment() {
    use clap::Parser;
    let mut registrations = ServeRegistrations::new();
    registrations
        .register_config_validator("stand_in", Arc::new(|_| Ok(())))
        .unwrap();
    for argv in [vec!["mcp"], vec!["mcp", "--transport", "stdio"]] {
        let args = crate::args::Args::parse_from(argv);
        let result = registrations.require_local_dispatch(&args, false);
        assert!(
            result
                .as_ref()
                .err()
                .is_some_and(|error| error.to_string().contains("KHIVE_NO_DAEMON=1")),
            "forwarding stdio cannot carry process-local validators"
        );
        assert!(registrations.require_local_dispatch(&args, true).is_ok());
        assert!(
            ServeRegistrations::new()
                .require_local_dispatch(&args, false)
                .is_ok(),
            "empty registration preserves existing forwarding behavior"
        );
    }
    for argv in [
        vec!["mcp", "--daemon"],
        vec!["mcp", "--transport", "config-capture"],
    ] {
        assert!(registrations
            .require_local_dispatch(&crate::args::Args::parse_from(argv), false)
            .is_ok());
    }
}

struct ProbeChannel {
    kind: &'static str,
    slug: &'static str,
    polls: AtomicUsize,
    sends: AtomicUsize,
    #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
    polled: tokio::sync::Notify,
    #[cfg(feature = "channel-telegram")]
    commits: AtomicUsize,
    #[cfg(feature = "channel-telegram")]
    shutdown: tokio_util::sync::CancellationToken,
}

impl ProbeChannel {
    fn new(kind: &'static str, slug: &'static str) -> Arc<Self> {
        Arc::new(Self {
            kind,
            slug,
            polls: AtomicUsize::new(0),
            sends: AtomicUsize::new(0),
            #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
            polled: tokio::sync::Notify::new(),
            #[cfg(feature = "channel-telegram")]
            commits: AtomicUsize::new(0),
            #[cfg(feature = "channel-telegram")]
            shutdown: tokio_util::sync::CancellationToken::new(),
        })
    }
}

#[async_trait]
impl Channel for ProbeChannel {
    fn kind(&self) -> &'static str {
        self.kind
    }
    fn slug(&self) -> String {
        self.slug.into()
    }
    async fn send(&self, _: ChannelEnvelope) -> Result<(), ChannelError> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn poll(&self, _: DateTime<Utc>) -> Result<Vec<ChannelEnvelope>, ChannelError> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        #[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
        self.polled.notify_one();
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn caller_channels_keep_composite_identity_and_actual_objects() {
    let first = ProbeChannel::new("fixture", "first");
    let second = ProbeChannel::new("fixture", "second");
    let builtin = ProbeChannel::new("builtin", "first");
    let mut registrations = ServeRegistrations::new();
    registrations.register_channel(first.clone()).unwrap();
    registrations.register_channel(second.clone()).unwrap();
    let builtins: Vec<Arc<dyn Channel>> = vec![builtin.clone()];
    let channels = registrations.merge_channels(builtins).unwrap();
    assert_eq!(
        channels.iter().count(),
        3,
        "merged registry retains every distinct composite key"
    );
    let selected = channels.get_by_slug("fixture", "second");
    assert!(
        selected.is_some(),
        "caller registration is visible in merged registry"
    );
    let selected = selected.unwrap();
    let expected: Arc<dyn Channel> = second.clone();
    assert!(
        Arc::ptr_eq(&selected, &expected),
        "registry preserves caller channel ownership"
    );
    selected.poll(Utc::now()).await.unwrap();
    selected
        .send(ChannelEnvelope::new("fixture:from", "fixture:to", "body"))
        .await
        .unwrap();
    assert_eq!(
        second.polls.load(Ordering::SeqCst),
        1,
        "lookup drives the registered caller object"
    );
    assert_eq!(second.sends.load(Ordering::SeqCst), 1);
    assert_eq!(first.polls.load(Ordering::SeqCst), 0);
    assert_eq!(builtin.polls.load(Ordering::SeqCst), 0);
}

#[test]
fn duplicate_caller_and_builtin_identities_refuse_without_replacement() {
    let first = ProbeChannel::new("fixture", "same");
    let second = ProbeChannel::new("fixture", "same");
    let mut registrations = ServeRegistrations::new();
    registrations.register_channel(first.clone()).unwrap();
    assert!(
        registrations.register_channel(second.clone()).is_err(),
        "duplicate caller identity must refuse"
    );
    let builtins: Vec<Arc<dyn Channel>> = vec![second];
    assert!(
        registrations.merge_channels(builtins).is_err(),
        "builtin caller collision must refuse before spawn"
    );
    assert_eq!(first.polls.load(Ordering::SeqCst), 0);
    let channels = registrations.merge_channels(Vec::new()).unwrap();
    let retained = channels.get_by_slug("fixture", "same").unwrap();
    let expected: Arc<dyn Channel> = first;
    assert!(
        Arc::ptr_eq(&retained, &expected),
        "refused duplicate does not replace the caller"
    );
}

#[cfg(not(any(feature = "channel-email", feature = "channel-telegram")))]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn host_preparation_installs_channels_without_optional_transports() {
    use clap::Parser;
    let runtime = khive_runtime::KhiveRuntime::memory().unwrap();
    let server = crate::server::KhiveMcpServer::new(runtime).unwrap();
    let args = crate::args::Args::parse_from(["mcp"]);
    let channel = ProbeChannel::new("fixture", "caller");
    let mut registrations = ServeRegistrations::new();
    registrations.register_channel(channel.clone()).unwrap();
    let prepared = super::startup::prepare_host_background_tasks_with_registrations(
        &args,
        &server,
        &registrations,
    )
    .await
    .unwrap();
    let selected = prepared.channels.get_by_slug("fixture", "caller");
    assert!(
        selected.is_some(),
        "actual host preparation must consume caller registrations"
    );
    selected.unwrap().poll(Utc::now()).await.unwrap();
    assert_eq!(channel.polls.load(Ordering::SeqCst), 1);
}

#[cfg(any(feature = "channel-email", feature = "channel-telegram"))]
fn verb_registry() -> khive_runtime::VerbRegistry {
    let runtime = khive_runtime::KhiveRuntime::memory().unwrap();
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    khive_runtime::PackRegistry::register_packs(
        &["kg".into(), "comm".into()],
        runtime,
        &mut builder,
    )
    .unwrap();
    builder.build().unwrap()
}

#[cfg(feature = "channel-email")]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn email_poll_resolves_only_owned_keys_from_shared_registry() {
    let owned = ProbeChannel::new("email", "owned");
    // A different slug of the same kind also belongs to a caller's driver.
    let caller = ProbeChannel::new("email", "caller");
    let mut registrations = ServeRegistrations::new();
    registrations.register_channel(caller.clone()).unwrap();
    let builtins: Vec<Arc<dyn Channel>> = vec![owned.clone()];
    let channels = registrations.merge_channels(builtins).unwrap();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let task = tokio::spawn(super::channel_poll::channel_poll_loop_registered(
        channels,
        vec![("email".into(), "owned".into())],
        verb_registry(),
        "local".into(),
        "actor:test".into(),
        shutdown.clone(),
    ));
    let observed = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        // Two owned polls prove that a complete first registry cycle finished.
        // This catches extra caller polling regardless of HashMap iteration order.
        while owned.polls.load(Ordering::SeqCst) < 2 {
            owned.polled.notified().await;
        }
    })
    .await;
    shutdown.cancel();
    task.await.unwrap();
    assert!(
        observed.is_ok(),
        "actual email loop must resolve and poll its registered owned key"
    );
    assert!(owned.polls.load(Ordering::SeqCst) >= 2);
    assert_eq!(
        caller.polls.load(Ordering::SeqCst),
        0,
        "email loop must not drive caller-owned sibling channels"
    );
}

#[cfg(feature = "channel-telegram")]
impl super::telegram::TelegramPollChannel for ProbeChannel {
    fn commit_offset(&self) {
        self.commits.fetch_add(1, Ordering::SeqCst);
        self.shutdown.cancel();
    }
}

#[cfg(feature = "channel-telegram")]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn telegram_shared_registry_retains_the_typed_acknowledgement_owner() {
    let owned = ProbeChannel::new("telegram", "owned");
    let caller = ProbeChannel::new("telegram", "caller");
    let mut registrations = ServeRegistrations::new();
    registrations.register_channel(caller.clone()).unwrap();
    let builtins: Vec<Arc<dyn Channel>> = vec![owned.clone()];
    let channels = registrations.merge_channels(builtins).unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        super::telegram::telegram_poll_loop_registered(
            owned.clone(),
            channels,
            verb_registry(),
            "local".into(),
            "actor:test".into(),
            owned.shutdown.clone(),
        ),
    )
    .await;
    assert!(
        result.is_ok(),
        "registered telegram owner must complete one real loop cycle"
    );
    assert_eq!(
        owned.polls.load(Ordering::SeqCst),
        1,
        "telegram polls the registered object"
    );
    assert_eq!(
        owned.commits.load(Ordering::SeqCst),
        1,
        "telegram acknowledges the same typed owner"
    );
    assert_eq!(caller.polls.load(Ordering::SeqCst), 0);
    assert_eq!(caller.commits.load(Ordering::SeqCst), 0);
}

#[cfg(feature = "channel-telegram")]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn telegram_refuses_same_key_with_a_different_acknowledgement_owner() {
    let typed = ProbeChannel::new("telegram", "same");
    let other = ProbeChannel::new("telegram", "same");
    let builtins: Vec<Arc<dyn Channel>> = vec![other.clone()];
    let channels = ServeRegistrations::new().merge_channels(builtins).unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        super::telegram::telegram_poll_loop_registered(
            typed.clone(),
            channels,
            verb_registry(),
            "local".into(),
            "actor:test".into(),
            typed.shutdown.clone(),
        ),
    )
    .await;
    assert!(
        result.is_ok(),
        "mismatched telegram ownership must refuse before polling"
    );
    assert_eq!(
        other.polls.load(Ordering::SeqCst),
        0,
        "same key cannot substitute a different Telegram object"
    );
    assert_eq!(typed.polls.load(Ordering::SeqCst), 0);
    assert_eq!(typed.commits.load(Ordering::SeqCst), 0);
}

#[test]
fn public_resident_registration_refuses_names_before_any_factory_runs() {
    use crate::components::ResidentComponentRegistration;
    let calls = Arc::new(AtomicUsize::new(0));
    let factory_calls = calls.clone();
    let resident = ResidentComponentRegistration::new("registered-resident", move |_, _| {
        factory_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    });
    let mut registrations = ServeRegistrations::new();
    registrations.register_resident(resident.clone()).unwrap();
    assert!(
        registrations.register_resident(resident).is_err(),
        "duplicate public resident must refuse before startup"
    );
    let reserved =
        ResidentComponentRegistration::new("email-outbound", |_, _| Box::pin(async { Ok(()) }));
    assert!(
        registrations.register_resident(reserved).is_err(),
        "caller resident cannot replace a builtin supervisor"
    );
    assert_eq!(
        registrations.resident_roster().unwrap().len(),
        1,
        "refused registration leaves the prior roster intact"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "registration must never run the resident factory"
    );
}

#[cfg(all(
    unix,
    not(any(feature = "channel-email", feature = "channel-telegram"))
))]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn real_startup_reports_stdio_and_demand_resident_skips_without_idle_obligations() {
    use clap::Parser;
    let runtime = khive_runtime::KhiveRuntime::memory().unwrap();
    let server = crate::server::KhiveMcpServer::new(runtime).unwrap();
    let starts = Arc::new(AtomicUsize::new(0));
    let captured = starts.clone();
    let mut registrations = ServeRegistrations::new();
    registrations
        .register_resident(crate::components::ResidentComponentRegistration::new(
            "skipped-caller-resident",
            move |_, _| {
                captured.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            },
        ))
        .unwrap();
    for argv in [vec!["mcp"], vec!["mcp", "--daemon", "--lifetime", "demand"]] {
        let args = crate::args::Args::parse_from(argv);
        let prepared = super::startup::prepare_host_background_tasks_with_registrations(
            &args,
            &server,
            &registrations,
        )
        .await
        .unwrap();
        let before = khive_runtime::background_task_count();
        let report = super::startup::start_host_background_tasks(&args, &server, None, prepared);
        assert!(
            report
                .skipped_components
                .iter()
                .any(|name| name == "skipped-caller-resident"),
            "real startup must name each skipped caller resident"
        );
        assert!(
            !report
                .idle_ineligible_reasons
                .iter()
                .any(|name| name.contains("skipped-caller-resident")),
            "a never-started resident must not become an idle-retirement obligation"
        );
        assert_eq!(
            khive_runtime::background_task_count(),
            before,
            "skipped residents must not spawn a supervisor"
        );
    }
    assert_eq!(starts.load(Ordering::SeqCst), 0);
}

#[cfg(not(any(feature = "channel-email", feature = "channel-telegram")))]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn real_persistent_startup_drives_captured_resident_with_shared_channels_and_cancellation() {
    use clap::Parser;
    let runtime = khive_runtime::KhiveRuntime::memory().unwrap();
    let server = crate::server::KhiveMcpServer::new(runtime).unwrap();
    let channel = ProbeChannel::new("fixture", "resident-owned");
    let mut registrations = ServeRegistrations::new();
    registrations.register_channel(channel.clone()).unwrap();
    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
    let (finished_tx, mut finished_rx) = tokio::sync::mpsc::unbounded_channel();
    registrations
        .register_resident(crate::components::ResidentComponentRegistration::new(
            "public-startup-resident",
            move |ctx, channels| {
                let started = started_tx.clone();
                let finished = finished_tx.clone();
                Box::pin(async move {
                    let selected = channels
                        .get_by_slug("fixture", "resident-owned")
                        .ok_or_else(|| {
                            crate::components::ComponentError::Permanent(
                                "registered caller channel is missing".into(),
                            )
                        })?;
                    selected
                        .poll(Utc::now())
                        .await
                        .map_err(|e| crate::components::ComponentError::Permanent(e.to_string()))?;
                    started
                        .send((channels, ctx.cancellation().clone()))
                        .unwrap();
                    ctx.cancellation().cancelled().await;
                    finished.send(()).unwrap();
                    Ok(())
                })
            },
        ))
        .unwrap();
    let args = crate::args::Args::parse_from(["mcp", "--daemon"]);
    let prepared = super::startup::prepare_host_background_tasks_with_registrations(
        &args,
        &server,
        &registrations,
    )
    .await
    .unwrap();
    let expected = prepared.channels.clone();
    let report = super::startup::start_host_background_tasks(&args, &server, None, prepared);
    assert!(!report
        .skipped_components
        .iter()
        .any(|name| name == "public-startup-resident"));
    let started = tokio::time::timeout(std::time::Duration::from_secs(5), started_rx.recv()).await;
    assert!(
        matches!(&started, Ok(Some(_))),
        "real persistent startup must run the registered resident"
    );
    let (actual, cancellation) = started.unwrap().unwrap();
    assert!(
        Arc::ptr_eq(&actual, &expected),
        "resident must consume the host's one shared registry"
    );
    assert_eq!(
        channel.polls.load(Ordering::SeqCst),
        1,
        "resident must drive the actual registered channel"
    );
    // This is the supervisor's child token; cancelling it leaves the process token intact.
    cancellation.cancel();
    assert!(
        matches!(
            tokio::time::timeout(std::time::Duration::from_secs(5), finished_rx.recv()).await,
            Ok(Some(()))
        ),
        "resident must observe the host cancellation context"
    );
    let settled = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if crate::components::component_health()
                .status("public-startup-resident")
                .is_some_and(|status| status.state == crate::components::ComponentState::Stopped)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        settled.is_ok(),
        "supervisor must join the cancelled resident and record stopped"
    );
}

#[cfg(not(any(feature = "channel-email", feature = "channel-telegram")))]
struct CombinedProofRoute {
    channel: Arc<dyn Channel>,
    seen: std::sync::Mutex<Vec<serde_json::Value>>,
}

#[cfg(not(any(feature = "channel-email", feature = "channel-telegram")))]
#[test]
#[serial_test::serial(config_ledger)]
fn route_registration_preserves_existing_bindings_and_requires_local_construction() {
    let channel = ProbeChannel::new("fixture", "route-owner");
    let route: Arc<dyn khive_runtime::channel_send::ChannelSendRoute> =
        Arc::new(CombinedProofRoute {
            channel,
            seen: std::sync::Mutex::new(Vec::new()),
        });
    let mut registrations = ServeRegistrations::new();
    registrations
        .register_send_route("fixture", route.clone())
        .unwrap();
    assert!(
        registrations
            .register_send_route("fixture", route.clone())
            .is_err(),
        "public route registration cannot replace an existing kind"
    );
    assert!(
        registrations
            .register_send_route("actor", route.clone())
            .is_err(),
        "public route registration preserves local actor addresses"
    );
    assert!(
        registrations.require_prebuilt_compatible().is_err(),
        "route-only bundles also require construction before serving"
    );
    #[cfg(unix)]
    {
        use clap::Parser;
        let stdio = crate::args::Args::parse_from(["mcp"]);
        assert!(
            registrations.require_local_dispatch(&stdio, false).is_err(),
            "route-only bundles cannot disappear through daemon forwarding"
        );
        assert!(registrations.require_local_dispatch(&stdio, true).is_ok());
        assert!(registrations
            .require_local_dispatch(&crate::args::Args::parse_from(["mcp", "--daemon"]), false)
            .is_ok());
    }

    let mut config = khive_runtime::RuntimeConfig::no_embeddings();
    config
        .channel_send_routes
        .register("fixture", route.clone())
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    registrations
        .register_config_validator(
            "fixture",
            Arc::new(move |_| {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
        )
        .unwrap();
    assert!(
        registrations.install_runtime_config(&mut config).is_err(),
        "an installed route collision refuses the whole registration bundle"
    );
    assert!(Arc::ptr_eq(
        &config
            .channel_send_routes
            .resolve("fixture:destination")
            .unwrap()
            .unwrap(),
        &route
    ));
    config
        .channel_config_validators
        .validate(&config.channel)
        .unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "route collision must not install or invoke proposed validators"
    );

    let mut invalid = ServeRegistrations::new();
    invalid
        .register_send_route("fixture", route.clone())
        .unwrap();
    invalid
        .register_config_validator(
            "fixture",
            Arc::new(|_| {
                Err(khive_runtime::RuntimeError::InvalidInput(
                    "fixture rejected".into(),
                ))
            }),
        )
        .unwrap();
    let mut empty = khive_runtime::RuntimeConfig::no_embeddings();
    assert!(invalid.install_runtime_config(&mut empty).is_err());
    assert!(
        empty
            .channel_send_routes
            .resolve("fixture:destination")
            .unwrap()
            .is_none(),
        "failed validation must not install the proposed route"
    );
    empty
        .channel_config_validators
        .validate(&empty.channel)
        .unwrap();

    registrations.install_runtime_config(&mut empty).unwrap();
    assert!(
        Arc::ptr_eq(
            &empty
                .channel_send_routes
                .resolve("fixture:destination")
                .unwrap()
                .unwrap(),
            &route
        ),
        "successful construction preserves the exact registered callback"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[cfg(not(any(feature = "channel-email", feature = "channel-telegram")))]
impl khive_runtime::channel_send::ChannelSendRoute for CombinedProofRoute {
    fn send<'a>(
        &'a self,
        _runtime: &'a khive_runtime::KhiveRuntime,
        _token: &'a khive_runtime::NamespaceToken,
        params: serde_json::Value,
    ) -> khive_runtime::channel_send::ChannelSendFuture<'a> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(params.clone());
            let to = params["to"].as_str().ok_or_else(|| {
                khive_runtime::RuntimeError::InvalidInput("stand-in recipient missing".into())
            })?;
            let content = params["content"].as_str().ok_or_else(|| {
                khive_runtime::RuntimeError::InvalidInput("stand-in content missing".into())
            })?;
            self.channel
                .send(ChannelEnvelope::new("fixture:sender", to, content))
                .await
                .map_err(|error| khive_runtime::RuntimeError::Internal(error.to_string()))?;
            // This stand-in's complete send policy is transport-only. It deliberately
            // makes no persistence/replay guarantee and the outer handler must not
            // append a legacy message after returning this exact custom result.
            Ok(serde_json::json!({"fixture": "sent"}))
        })
    }
}

#[cfg(not(any(feature = "channel-email", feature = "channel-telegram")))]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn configured_resident_uses_shared_channel_and_actual_comm_route_then_stops() {
    use clap::Parser;
    use khive_runtime::{KhiveConfig, KhiveRuntime, RuntimeConfig};
    use std::time::Duration;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("component.toml");
    std::fs::write(
        &path,
        "[channel.fixture]\nendpoint = 'fixture:destination'\nretries = 3\nlabels = ['one', 'two']\n[channel.fixture.nested]\nenabled = true\n",
    )
    .unwrap();
    let loaded = KhiveConfig::load(Some(&path)).unwrap().unwrap();
    // Independent typed literal, not parse/serialize of the fixture text.
    let expected_table = toml::Value::Table(toml::map::Map::from_iter([
        (
            "endpoint".into(),
            toml::Value::String("fixture:destination".into()),
        ),
        ("retries".into(), toml::Value::Integer(3)),
        (
            "labels".into(),
            toml::Value::Array(vec![
                toml::Value::String("one".into()),
                toml::Value::String("two".into()),
            ]),
        ),
        (
            "nested".into(),
            toml::Value::Table(toml::map::Map::from_iter([(
                "enabled".into(),
                toml::Value::Boolean(true),
            )])),
        ),
    ]));
    let validated = Arc::new(std::sync::Mutex::new(Vec::new()));
    let validated_in_callback = validated.clone();
    let channel = ProbeChannel::new("fixture", "resident-owned");
    let route = Arc::new(CombinedProofRoute {
        channel: channel.clone(),
        seen: std::sync::Mutex::new(Vec::new()),
    });
    let mut registrations = ServeRegistrations::new();
    registrations.register_channel(channel.clone()).unwrap();
    registrations
        .register_config_validator(
            "fixture",
            Arc::new(move |raw| {
                validated_in_callback.lock().unwrap().push(raw.cloned());
                // Keep the semantic oracle at the named outer assertion below:
                // dropped config must not fail a generic builder unwrap first.
                Ok(())
            }),
        )
        .unwrap();
    registrations
        .register_send_route("fixture", route.clone())
        .unwrap();

    let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
    let (observed_tx, mut observed_rx) = tokio::sync::mpsc::unbounded_channel();
    let (finished_tx, mut finished_rx) = tokio::sync::mpsc::unbounded_channel();
    registrations
        .register_resident(crate::components::ResidentComponentRegistration::new(
            "configured-channel-proof",
            move |ctx, channels| {
                let started = started_tx.clone();
                let observed = observed_tx.clone();
                let finished = finished_tx.clone();
                Box::pin(async move {
                    started
                        .send((channels.clone(), ctx.cancellation().clone()))
                        .unwrap();
                    let selected = channels.get_by_slug("fixture", "resident-owned");
                    let present = selected.is_some();
                    let outcome = if let Some(selected) = selected {
                        match selected.poll(Utc::now()).await {
                            Ok(_) => Some(
                                ctx.server()
                                    .verb_registry_clone()
                                    .dispatch(
                                        "comm.send",
                                        serde_json::json!({
                                            "to": "fixture:destination",
                                            "content": "literal component payload"
                                        }),
                                    )
                                    .await
                                    .map_err(|error| error.to_string()),
                            ),
                            Err(error) => Some(Err(error.to_string())),
                        }
                    } else {
                        None
                    };
                    observed.send((present, outcome)).unwrap();
                    ctx.cancellation().cancelled().await;
                    finished.send(()).unwrap();
                    Ok(())
                })
            },
        ))
        .unwrap();

    let base = RuntimeConfig {
        db_path: None,
        actor_id: Some("actor:channel-proof".into()),
        packs: vec!["kg".into(), "comm".into()],
        gate: Arc::new(khive_runtime::AllowAllGate),
        ..RuntimeConfig::no_embeddings()
    };
    let mut config = khive_runtime::runtime_config_from_khive_config(&loaded, base);
    registrations.install_runtime_config(&mut config).unwrap();
    let runtime = KhiveRuntime::new(config).unwrap();
    let built =
        crate::server::KhiveMcpServer::with_packs(runtime.clone(), &["kg".into(), "comm".into()]);
    assert!(
        built.is_ok(),
        "actual CommPack registry must accept the registered validator"
    );
    let server = built.unwrap();
    let observations = validated.lock().unwrap().clone();
    assert!(
        observations.len() >= 2,
        "host preparation and CommPack activation both validate the component"
    );
    assert!(
        observations
            .iter()
            .all(|value| value.as_ref() == Some(&expected_table)),
        "actual CommPack build must receive the exact raw component table"
    );

    let args = crate::args::Args::parse_from(["mcp", "--daemon"]);
    let prepared = super::startup::prepare_host_background_tasks_with_registrations(
        &args,
        &server,
        &registrations,
    )
    .await
    .unwrap();
    let expected_registry = prepared.channels.clone();
    let report = super::startup::start_host_background_tasks(&args, &server, None, prepared);
    assert!(!report
        .skipped_components
        .iter()
        .any(|name| name == "configured-channel-proof"));
    let started = tokio::time::timeout(Duration::from_secs(5), started_rx.recv()).await;
    assert!(
        matches!(&started, Ok(Some(_))),
        "actual startup must invoke the registered resident"
    );
    let (actual_registry, child_token) = started.unwrap().unwrap();
    let observed = tokio::time::timeout(Duration::from_secs(5), observed_rx.recv()).await;
    // Always stop and settle the child before checking mutation-sensitive results.
    // This is a supervisor child token, never the process-global token.
    child_token.cancel();
    assert!(
        matches!(
            tokio::time::timeout(Duration::from_secs(5), finished_rx.recv()).await,
            Ok(Some(()))
        ),
        "resident must observe host cancellation"
    );
    let stopped = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if crate::components::component_health()
                .status("configured-channel-proof")
                .is_some_and(|status| status.state == crate::components::ComponentState::Stopped)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        stopped.is_ok(),
        "existing supervisor must join and record the resident stopped"
    );
    assert!(
        Arc::ptr_eq(&actual_registry, &expected_registry),
        "resident must receive the exact merged registry"
    );
    assert!(
        matches!(&observed, Ok(Some((true, _)))),
        "resident must resolve the caller channel from the shared registry"
    );
    let (_, outcome) = observed.unwrap().unwrap();
    assert!(
        matches!(&outcome, Some(Ok(value)) if value == &serde_json::json!({"fixture": "sent"})),
        "actual comm.send must select the registered custom route: {outcome:?}"
    );
    assert_eq!(channel.polls.load(Ordering::SeqCst), 1);
    assert_eq!(
        channel.sends.load(Ordering::SeqCst),
        1,
        "custom route must invoke the registered channel send"
    );
    assert_eq!(
        *route.seen.lock().unwrap(),
        vec![
            serde_json::json!({"to": "fixture:destination", "content": "literal component payload"})
        ],
        "comm route must preserve the original validated request"
    );
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    assert_eq!(
        runtime
            .notes(&token)
            .unwrap()
            .count_notes("local", Some("message"))
            .await
            .unwrap(),
        0,
        "the core must not append a legacy message after the custom transport-only result"
    );
}

use super::ServeRegistrations;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use khive_channel::{Channel, ChannelEnvelope, ChannelError};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

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

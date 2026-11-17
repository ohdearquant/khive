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

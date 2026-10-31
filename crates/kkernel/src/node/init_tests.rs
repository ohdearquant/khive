//! Tests for `kkernel node init`: origin refusal before any side effect, the
//! add-only keychain contract and recovery of an incomplete initialization.

use std::io::{self, Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use clap::Parser;
use khive_channel_node::encoding::Realm;
use khive_channel_node::keychain::KeychainError;
use khive_channel_node::keys::{EnrolmentBundle, KeyFacility};

use super::test_support::{run_node, NoService, ScriptedConfirmation, TestNodeKeys};
use super::{run_with, NodeArgs, NodeCapabilities, NodeKeys, NodeSetupError};

const ORIGIN_MSG: &str =
    "service_url must be an HTTPS origin without credentials, query or fragment";
const ALREADY_INIT_MSG: &str = "node keys are already initialized; use node status --bundle";
const DIFFERS_MSG: &str = "initialization settings differ from the saved preparation";
const OUTPUT_MSG: &str = "could not write public output; use node status to export it again";
const GOOD_URL: &str = "https://service.example";

#[derive(Parser, Debug)]
#[command(name = "node-test")]
struct Wrapper {
    #[command(flatten)]
    node: NodeArgs,
}

/// Keys that delegate to `TestNodeKeys` and record what the configuration and
/// the keychain looked like at the moment `create` was called. With
/// `fail_after_create` set they fail after the item was added, which is the
/// "failure after add" case.
struct ObservingKeys<'a> {
    inner: &'a TestNodeKeys,
    config: PathBuf,
    at_create: Mutex<Option<(Option<String>, usize)>>,
    fail_after_create: AtomicBool,
    references: Mutex<Vec<String>>,
}

impl<'a> ObservingKeys<'a> {
    fn new(inner: &'a TestNodeKeys, config: &Path) -> Self {
        Self {
            inner,
            config: config.to_path_buf(),
            at_create: Mutex::new(None),
            fail_after_create: AtomicBool::new(false),
            references: Mutex::new(Vec::new()),
        }
    }
}

impl NodeKeys for ObservingKeys<'_> {
    fn create(&self, reference: &str, realm: Realm) -> Result<EnrolmentBundle, KeychainError> {
        self.references
            .lock()
            .expect("reference lock")
            .push(reference.to_owned());
        let seen = std::fs::read_to_string(&self.config).ok();
        let count = self.inner.keychain().item_count();
        *self.at_create.lock().expect("observation lock") = Some((seen, count));
        let bundle = self.inner.create(reference, realm)?;
        if self.fail_after_create.load(Ordering::SeqCst) {
            return Err(KeychainError::InvalidMaterial);
        }
        Ok(bundle)
    }

    fn open(
        &self,
        reference: &str,
        fingerprint: &[u8; 32],
    ) -> Result<Arc<dyn KeyFacility>, KeychainError> {
        self.inner.open(reference, fingerprint)
    }

    fn recover(&self, reference: &str, realm: Realm) -> Result<EnrolmentBundle, KeychainError> {
        self.inner.recover(reference, realm)
    }
}

/// An output stream that always fails, like a closed pipe.
struct FailingWriter;

impl Write for FailingWriter {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
    }
}

/// Runs `argv` against `keys` with a caller-supplied output stream.
async fn run_with_keys(
    argv: &[&str],
    keys: &dyn NodeKeys,
    output: &mut dyn Write,
) -> Result<(), NodeSetupError> {
    let mut full = vec!["node-test"];
    full.extend_from_slice(argv);
    let args = Wrapper::try_parse_from(full)
        .expect("test argv must parse")
        .node;
    let capabilities = NodeCapabilities {
        native_macos: true,
        macos_keychain: true,
    };
    let mut confirmation = ScriptedConfirmation::new(Vec::new());
    let mut input = Cursor::new(Vec::<u8>::new());
    let mut diagnostics = Vec::<u8>::new();
    run_with(
        args,
        capabilities,
        keys,
        &NoService,
        &mut confirmation,
        &mut input,
        output,
        &mut diagnostics,
    )
    .await
}

fn setup() -> (tempfile::TempDir, PathBuf, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("khive.toml");
    let text = config.to_str().expect("utf8 path").to_owned();
    (dir, config, text)
}

fn init_args<'a>(config: &'a str, actor: &'a str, url: &'a str) -> Vec<&'a str> {
    vec![
        "init",
        "--config",
        config,
        "--service-url",
        url,
        "--realm",
        "realm-1",
        "--actor",
        actor,
    ]
}

fn read_text(path: &Path) -> String {
    std::fs::read_to_string(path).expect("config readable")
}

fn reference_line(text: &str) -> String {
    text.lines()
        .find(|line| line.contains("key_reference"))
        .expect("config records a key_reference")
        .to_owned()
}

#[tokio::test]
async fn node_init_refuses_bad_origin_before_keychain() {
    let (_dir, config, text) = setup();
    let keys = TestNodeKeys::new();
    let mut confirmation = ScriptedConfirmation::new(Vec::new());

    for bad in [
        "http://service.example",
        "https://user:pw@service.example",
        "https://service.example/?q=1",
        "https://service.example/#frag",
        "https://service.example/path",
    ] {
        let run = run_node(
            &init_args(&text, "laptop", bad),
            &keys,
            &mut confirmation,
            &NoService,
            b"",
        )
        .await;
        let error = run.result.expect_err(bad);
        assert_eq!(error.exit, 2, "{bad}");
        assert_eq!(error.message, ORIGIN_MSG, "{bad}");
        assert_eq!(keys.calls(), (0, 0, 0), "{bad}");
        assert_eq!(keys.keychain().item_count(), 0, "{bad}");
        assert!(!config.exists(), "no config may be published for {bad}");
        assert!(run.stdout.is_empty(), "{bad}");
    }

    // Positive control: the same arguments with a valid HTTPS origin succeed.
    let run = run_node(
        &init_args(&text, "laptop", GOOD_URL),
        &keys,
        &mut confirmation,
        &NoService,
        b"",
    )
    .await;
    run.result.expect("valid https origin initialises");
    assert_eq!(keys.calls().0, 1);
    assert_eq!(keys.keychain().item_count(), 1);
    assert!(read_text(&config).contains("enabled = false"));
}

#[tokio::test]
async fn node_init_add_only_and_recoverable() {
    prepared_before_add_and_disabled().await;
    repeat_cannot_replace().await;
    failed_add_can_retry().await;
    failure_after_add_recovers_same_item().await;
    output_failure_recoverable_by_status().await;
}

/// The disabled preparation is stored before the add; the fingerprint after.
async fn prepared_before_add_and_disabled() {
    let (_dir, config, text) = setup();
    let keys = TestNodeKeys::new();
    let observing = ObservingKeys::new(&keys, &config);
    let mut out = Vec::<u8>::new();
    run_with_keys(&init_args(&text, "laptop", GOOD_URL), &observing, &mut out)
        .await
        .expect("init succeeds");

    let (seen, count) = observing
        .at_create
        .lock()
        .expect("observation lock")
        .take()
        .expect("create was called");
    let seen = seen.expect("config already published when create ran");
    assert_eq!(count, 0, "nothing may be added before the preparation");
    assert!(seen.contains("key_reference"));
    assert!(seen.contains("enabled = false"));
    assert!(!seen.contains("enabled = true"));
    assert!(!seen.contains("key_fingerprint"));

    let done = read_text(&config);
    assert!(done.contains("key_fingerprint"));
    assert!(done.contains("enabled = false"));
    assert!(!done.contains("enabled = true"));
    assert_eq!(keys.keychain().item_count(), 1);
    assert!(!out.is_empty(), "the public bundle goes to stdout");
}

/// A second init cannot replace the existing item.
async fn repeat_cannot_replace() {
    let (_dir, _config, text) = setup();
    let keys = TestNodeKeys::new();
    let mut confirmation = ScriptedConfirmation::new(Vec::new());
    run_node(
        &init_args(&text, "laptop", GOOD_URL),
        &keys,
        &mut confirmation,
        &NoService,
        b"",
    )
    .await
    .result
    .expect("first init succeeds");
    assert_eq!(keys.keychain().item_count(), 1);

    let run = run_node(
        &init_args(&text, "laptop", GOOD_URL),
        &keys,
        &mut confirmation,
        &NoService,
        b"",
    )
    .await;
    let error = run.result.expect_err("repeat init is refused");
    assert_eq!(error.exit, 2);
    assert_eq!(error.message, ALREADY_INIT_MSG);
    assert_eq!(keys.calls().0, 1, "create must not run again");
    assert_eq!(keys.keychain().item_count(), 1);
    assert!(run.stdout.is_empty());
}

/// A failed add leaves a preparation that a retry reuses.
async fn failed_add_can_retry() {
    let (_dir, config, text) = setup();
    let keys = TestNodeKeys::new();
    let observing = ObservingKeys::new(&keys, &config);
    keys.keychain().fail_adds(true);
    let mut out = Vec::<u8>::new();
    run_with_keys(&init_args(&text, "laptop", GOOD_URL), &observing, &mut out)
        .await
        .expect_err("a failed add fails init");
    assert_eq!(keys.keychain().item_count(), 0);
    let prepared = read_text(&config);
    assert!(prepared.contains("key_reference"));
    assert!(!prepared.contains("key_fingerprint"));
    let reference = reference_line(&prepared);

    // Different settings cannot reuse the saved preparation.
    let error = run_with_keys(&init_args(&text, "desktop", GOOD_URL), &observing, &mut out)
        .await
        .expect_err("differing settings are refused");
    assert_eq!(error.message, DIFFERS_MSG);
    assert_eq!(keys.keychain().item_count(), 0);

    // The same settings retry, reuse the reference and succeed.
    keys.keychain().fail_adds(false);
    run_with_keys(&init_args(&text, "laptop", GOOD_URL), &observing, &mut out)
        .await
        .expect("retry succeeds");
    assert_eq!(keys.keychain().item_count(), 1);
    let done = read_text(&config);
    assert_eq!(reference_line(&done), reference);
    assert!(done.contains("key_fingerprint"));
    let seen = observing.references.lock().expect("reference lock").clone();
    assert_eq!(seen.len(), 2, "create runs once per accepted attempt");
    assert_eq!(seen[0], seen[1], "the retry reuses the saved key reference");
    assert!(reference.contains(seen[0].as_str()));
}

/// After a failure that follows the add, recovery returns the same public
/// bundle without creating a second item.
async fn failure_after_add_recovers_same_item() {
    let (_dir, config, text) = setup();
    let keys = TestNodeKeys::new();
    let observing = ObservingKeys::new(&keys, &config);
    observing.fail_after_create.store(true, Ordering::SeqCst);
    let mut out = Vec::<u8>::new();
    run_with_keys(&init_args(&text, "laptop", GOOD_URL), &observing, &mut out)
        .await
        .expect_err("failure after the add fails init");
    assert_eq!(keys.keychain().item_count(), 1);
    let stuck = read_text(&config);
    assert!(stuck.contains("key_reference"));
    assert!(!stuck.contains("key_fingerprint"));

    let mut confirmation = ScriptedConfirmation::new(Vec::new());
    let recovered = run_node(
        &["init", "--config", text.as_str(), "--recover"],
        &keys,
        &mut confirmation,
        &NoService,
        b"",
    )
    .await;
    recovered.result.expect("recover succeeds");
    assert!(!recovered.stdout.is_empty());
    assert_eq!(keys.keychain().item_count(), 1, "recovery never adds");
    assert_eq!(keys.calls().0, 1, "create ran only in the failed init");
    assert_eq!(keys.calls().2, 1);
    assert!(read_text(&config).contains("key_fingerprint"));

    let status = run_node(
        &["status", "--config", text.as_str(), "--bundle"],
        &keys,
        &mut confirmation,
        &NoService,
        b"",
    )
    .await;
    status.result.expect("status --bundle succeeds");
    assert_eq!(status.stdout, recovered.stdout);
}

/// An output failure after the fingerprint is recorded is recoverable by status.
async fn output_failure_recoverable_by_status() {
    let (_dir, config, text) = setup();
    let keys = TestNodeKeys::new();
    let error = run_with_keys(
        &init_args(&text, "laptop", GOOD_URL),
        &keys,
        &mut FailingWriter,
    )
    .await
    .expect_err("a failed output write fails init");
    assert_eq!(error.exit, 6);
    assert_eq!(error.message, OUTPUT_MSG);
    assert_eq!(keys.keychain().item_count(), 1);
    assert!(read_text(&config).contains("key_fingerprint"));

    let mut confirmation = ScriptedConfirmation::new(Vec::new());
    let status = run_node(
        &["status", "--config", text.as_str(), "--bundle"],
        &keys,
        &mut confirmation,
        &NoService,
        b"",
    )
    .await;
    status.result.expect("status --bundle exports the bundle");
    assert!(status.stdout.starts_with(b"{"));
    assert!(status.stdout.ends_with(b"\n"));
    assert_eq!(keys.keychain().item_count(), 1);
}

//! Shared helpers for the node command tests: in-memory keys, scripted consent,
//! loopback and absent service connections, and a runner that drives `run_with`.

// Later command tests use these helpers; until they land some are unused.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use clap::Parser;
use khive_channel_node::client::NodeClient;
use khive_channel_node::encoding::Realm;
use khive_channel_node::keychain::testing::TestKeychain;
use khive_channel_node::keychain::KeychainError;
use khive_channel_node::keys::{EnrolmentBundle, KeyFacility};
use khive_runtime::node_config::NodeChannelConfig;

use super::commands::{binding_from, EmptyOutbound, UnconfirmedPins};
use super::{
    run_with, Confirmation, NodeArgs, NodeCapabilities, NodeKeys, NodeService, NodeSetupError,
};

/// In-memory node keys over a `TestKeychain`, counting every call.
pub(crate) struct TestNodeKeys {
    keychain: TestKeychain,
    create_calls: AtomicUsize,
    open_calls: AtomicUsize,
    recover_calls: AtomicUsize,
}

impl TestNodeKeys {
    /// Empty keys with all call counters at zero.
    pub(crate) fn new() -> Self {
        Self {
            keychain: TestKeychain::new(),
            create_calls: AtomicUsize::new(0),
            open_calls: AtomicUsize::new(0),
            recover_calls: AtomicUsize::new(0),
        }
    }

    /// The wrapped keychain, for `fail_adds`, `fail_reads` and `item_count`.
    pub(crate) fn keychain(&self) -> &TestKeychain {
        &self.keychain
    }

    /// How many times create, open and recover were called, in that order.
    pub(crate) fn calls(&self) -> (usize, usize, usize) {
        (
            self.create_calls.load(Ordering::SeqCst),
            self.open_calls.load(Ordering::SeqCst),
            self.recover_calls.load(Ordering::SeqCst),
        )
    }
}

impl NodeKeys for TestNodeKeys {
    fn create(&self, reference: &str, realm: Realm) -> Result<EnrolmentBundle, KeychainError> {
        self.create_calls.fetch_add(1, Ordering::SeqCst);
        self.keychain.create(reference, realm)
    }

    fn open(
        &self,
        reference: &str,
        fingerprint: &[u8; 32],
    ) -> Result<Arc<dyn KeyFacility>, KeychainError> {
        self.open_calls.fetch_add(1, Ordering::SeqCst);
        let facility = self.keychain.open(reference, fingerprint)?;
        Ok(Arc::new(facility))
    }

    fn recover(&self, reference: &str, realm: Realm) -> Result<EnrolmentBundle, KeychainError> {
        self.recover_calls.fetch_add(1, Ordering::SeqCst);
        self.keychain.recover(reference, realm)
    }
}

/// A confirmation that answers from a script and records every prompt shown.
pub(crate) struct ScriptedConfirmation {
    answers: VecDeque<Result<bool, NodeSetupError>>,
    prompts: Vec<String>,
}

impl ScriptedConfirmation {
    /// A confirmation that gives `answers` in order, then answers like end of input.
    pub(crate) fn new(answers: Vec<Result<bool, NodeSetupError>>) -> Self {
        Self {
            answers: answers.into(),
            prompts: Vec::new(),
        }
    }

    /// Every prompt shown so far, in order.
    pub(crate) fn prompts(&self) -> &[String] {
        &self.prompts
    }
}

impl Confirmation for ScriptedConfirmation {
    fn confirm(&mut self, prompt: &str) -> Result<bool, NodeSetupError> {
        self.prompts.push(prompt.to_owned());
        // An exhausted script is end of input, which `confirm_on` reads as a decline.
        self.answers.pop_front().unwrap_or(Ok(false))
    }
}

/// A service that dials a plain-HTTP loopback origin and counts its calls.
pub(crate) struct LoopbackService {
    url: String,
    calls: AtomicUsize,
}

impl LoopbackService {
    /// A service whose clients dial `url`, a plain-HTTP loopback origin.
    pub(crate) fn new(url: &str) -> Self {
        Self {
            url: url.to_owned(),
            calls: AtomicUsize::new(0),
        }
    }

    /// How many times `client` was called.
    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl NodeService for LoopbackService {
    fn client(
        &self,
        config: &NodeChannelConfig,
        facility: Arc<dyn KeyFacility>,
    ) -> Result<NodeClient, NodeSetupError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        NodeClient::test_http(
            binding_from(config)?,
            &self.url,
            Arc::new(EmptyOutbound),
            Arc::new(UnconfirmedPins),
            facility,
        )
        .map_err(|_| NodeSetupError {
            exit: 5,
            message: "loopback test client could not be built".to_owned(),
        })
    }
}

/// A service for commands that must never reach the service.
pub(crate) struct NoService;

impl NodeService for NoService {
    fn client(
        &self,
        _config: &NodeChannelConfig,
        _facility: Arc<dyn KeyFacility>,
    ) -> Result<NodeClient, NodeSetupError> {
        panic!("this command must not reach the service")
    }
}

#[derive(Parser, Debug)]
#[command(name = "node-test")]
struct Wrapper {
    #[command(flatten)]
    node: NodeArgs,
}

/// The outcome of one command run: its result and the bytes it wrote.
pub(crate) struct Run {
    /// What `run_with` returned.
    pub result: Result<(), NodeSetupError>,
    /// Bytes written to the output stream.
    pub stdout: Vec<u8>,
    /// Bytes written to the diagnostics stream.
    pub stderr: Vec<u8>,
}

/// Parses `argv` as `kkernel node` arguments and runs it with passing capabilities.
pub(crate) async fn run_node(
    argv: &[&str],
    keys: &TestNodeKeys,
    confirmation: &mut dyn Confirmation,
    service: &dyn NodeService,
    stdin: &[u8],
) -> Run {
    let mut full = vec!["node-test"];
    full.extend_from_slice(argv);
    let args = Wrapper::try_parse_from(full)
        .expect("test argv must parse")
        .node;
    let capabilities = NodeCapabilities {
        native_macos: true,
        macos_keychain: true,
    };
    let mut input = Cursor::new(stdin.to_vec());
    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    let result = run_with(
        args,
        capabilities,
        keys,
        service,
        confirmation,
        &mut input,
        &mut stdout,
        &mut stderr,
    )
    .await;
    Run {
        result,
        stdout,
        stderr,
    }
}

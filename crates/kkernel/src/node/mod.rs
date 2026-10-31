//! Node setup commands: the module root, the command-line entry and the exit wrapper.
//!
//! The parser, the typed error and the entry points exist in every build. The
//! command implementations and everything that touches key material are gated
//! on the `node-setup` feature; native Keychain calls are further gated on
//! macOS with the `macos-keychain` feature.

// Without the feature the parser still exists but nothing reads its fields.
#![cfg_attr(not(feature = "node-setup"), allow(dead_code))]

use std::io::Write;
use std::path::PathBuf;

#[cfg(feature = "node-setup")]
use std::io::{BufRead, Read};
#[cfg(feature = "node-setup")]
use std::path::Path;
#[cfg(feature = "node-setup")]
use std::sync::Arc;

#[cfg(feature = "node-setup")]
use khive_channel_node::client::NodeClient;
#[cfg(feature = "node-setup")]
use khive_channel_node::encoding::Realm;
#[cfg(feature = "node-setup")]
use khive_channel_node::keychain::KeychainError;
#[cfg(all(target_os = "macos", feature = "macos-keychain"))]
use khive_channel_node::keychain::MacOsKeyFacility;
#[cfg(feature = "node-setup")]
use khive_channel_node::keys::{EnrolmentBundle, KeyFacility};
#[cfg(feature = "node-setup")]
use khive_runtime::node_config::NodeChannelConfig;

#[cfg(feature = "node-setup")]
mod commands;
#[cfg(feature = "node-setup")]
mod config_edit;
#[cfg(feature = "node-setup")]
mod formats;

#[cfg(all(test, feature = "node-setup"))]
mod assign_tests;
#[cfg(test)]
mod cli_tests;
#[cfg(all(test, feature = "node-setup"))]
mod config_edit_tests;
#[cfg(all(test, feature = "node-setup"))]
mod formats_tests;
#[cfg(all(test, feature = "node-setup"))]
mod init_tests;
#[cfg(all(test, feature = "node-setup"))]
mod pin_tests;
#[cfg(all(test, feature = "node-setup"))]
mod service_tests;
#[cfg(all(test, feature = "node-setup"))]
mod status_tests;
#[cfg(all(test, feature = "node-setup"))]
mod test_support;

// The clap-derived items below use `//` comments on purpose: a `///` comment
// would become published help text.

// Arguments of `kkernel node`.
#[derive(clap::Args, Debug, Clone)]
pub(crate) struct NodeArgs {
    // The one configuration file to read and edit. It is never discovered.
    #[arg(long, env = "KHIVE_CONFIG", global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,
    #[command(subcommand)]
    pub command: NodeCommand,
}

// The node setup commands.
#[derive(clap::Subcommand, Debug, Clone)]
pub(crate) enum NodeCommand {
    #[command(about = "Initialize this Mac's node keys.")]
    Init(InitArgs),
    #[command(about = "Import this Mac's enrolment result.")]
    Assign {
        #[arg(long, value_name = "FILE")]
        input: PathBuf,
    },
    #[command(about = "Confirm a contact's public keys.")]
    Pin {
        #[arg(long, value_name = "FILE")]
        card: PathBuf,
    },
    #[command(about = "Show local node setup and export public information.")]
    Status(StatusArgs),
}

// Arguments of `node init`. With `--recover` no other init option is accepted
// except `--out`.
#[derive(clap::Args, Debug, Clone)]
pub(crate) struct InitArgs {
    #[arg(
        long,
        value_name = "ORIGIN",
        required_unless_present = "recover",
        conflicts_with = "recover"
    )]
    pub service_url: Option<String>,
    #[arg(
        long,
        value_name = "REALM",
        required_unless_present = "recover",
        conflicts_with = "recover"
    )]
    pub realm: Option<String>,
    #[arg(
        long,
        value_name = "LABEL",
        required_unless_present = "recover",
        conflicts_with = "recover"
    )]
    pub actor: Option<String>,
    #[arg(
        long,
        value_name = "SLUG",
        default_value = "node",
        conflicts_with = "recover"
    )]
    pub slug: String,
    #[arg(
        long,
        value_name = "NS",
        default_value = "local",
        conflicts_with = "recover"
    )]
    pub namespace: String,
    #[arg(long)]
    pub recover: bool,
    #[arg(long, value_name = "FILE", default_value = "-")]
    pub out: PathBuf,
}

// Arguments of `node status`. `--bundle` and `--card` are mutually exclusive
// and `--out` is accepted only with one of them; `None` means standard output.
#[derive(clap::Args, Debug, Clone)]
#[command(group(clap::ArgGroup::new("export").args(["bundle", "card"]).multiple(false)))]
pub(crate) struct StatusArgs {
    #[arg(long)]
    pub bundle: bool,
    #[arg(long)]
    pub card: bool,
    #[arg(long, value_name = "FILE", requires = "export")]
    pub out: Option<PathBuf>,
}

/// A typed refusal: the process exit code and the exact message printed after
/// `node: `. The message never carries secrets, source text or OS detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NodeSetupError {
    /// Exit code from the design table: 2, 3, 4, 5 or 6.
    pub exit: u8,
    /// Normative message text, without the `node: ` prefix.
    pub message: String,
}

impl std::fmt::Display for NodeSetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for NodeSetupError {}

fn feature_unavailable() -> NodeSetupError {
    NodeSetupError {
        exit: 4,
        message: "node setup requires a build with the macos-keychain feature".to_owned(),
    }
}

#[cfg(feature = "node-setup")]
fn platform_unavailable() -> NodeSetupError {
    NodeSetupError {
        exit: 4,
        message: "node setup requires macOS".to_owned(),
    }
}

#[cfg(feature = "node-setup")]
fn config_not_selected() -> NodeSetupError {
    NodeSetupError {
        exit: 2,
        message: "select the configuration file with --config".to_owned(),
    }
}

#[cfg(feature = "node-setup")]
fn tty_unavailable() -> NodeSetupError {
    // The design table gives no separate code for a missing terminal; it is
    // grouped with a declined confirmation, which exits 3.
    NodeSetupError {
        exit: 3,
        message: "contact confirmation requires a terminal".to_owned(),
    }
}

#[cfg(feature = "node-setup")]
fn local_io(message: &str) -> NodeSetupError {
    NodeSetupError {
        exit: 6,
        message: message.to_owned(),
    }
}

/// Every keychain failure is a refusal with exit 4 and the design's message.
#[cfg(feature = "node-setup")]
impl From<KeychainError> for NodeSetupError {
    fn from(error: KeychainError) -> Self {
        let message = match error {
            KeychainError::HomeUnavailable => "effective-user home directory is unavailable",
            KeychainError::LoginKeychainUnavailable => "login keychain is unavailable",
            KeychainError::ItemUnavailable => {
                "key item is unavailable; unlock the login keychain and retry"
            }
            KeychainError::InvalidMaterial => "stored key material is invalid",
            KeychainError::PublicKeyMismatch => {
                "stored keys do not match the expected fingerprint"
            }
            KeychainError::RandomnessUnavailable => "operating-system randomness is unavailable",
            KeychainError::AlreadyExists => {
                "a key item already exists for this reference; use node init --recover for an incomplete initialization"
            }
        };
        NodeSetupError {
            exit: 4,
            message: message.to_owned(),
        }
    }
}

/// The exact stderr text for a failure: `node: <message>` and one line feed.
fn failure_line(error: &NodeSetupError) -> String {
    format!("node: {}\n", error.message)
}

/// What this build can do, fixed at compile time in production.
#[cfg(feature = "node-setup")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NodeCapabilities {
    /// The target is macOS.
    pub native_macos: bool,
    /// The `macos-keychain` feature is compiled in.
    pub macos_keychain: bool,
}

#[cfg(feature = "node-setup")]
impl NodeCapabilities {
    /// The capabilities of this binary. No environment variable changes them.
    pub(crate) fn production() -> Self {
        Self {
            native_macos: cfg!(target_os = "macos"),
            macos_keychain: cfg!(feature = "macos-keychain"),
        }
    }
}

/// The key operations the commands need, behind a seam so tests never touch
/// the real login Keychain.
#[cfg(feature = "node-setup")]
pub(crate) trait NodeKeys {
    /// Creates the key item for `reference` and returns its public bundle.
    fn create(&self, reference: &str, realm: Realm) -> Result<EnrolmentBundle, KeychainError>;
    /// Opens the item for `reference`; its public keys must match `fingerprint`.
    fn open(
        &self,
        reference: &str,
        fingerprint: &[u8; 32],
    ) -> Result<Arc<dyn KeyFacility>, KeychainError>;
    /// Returns the public bundle of an existing item without changing it.
    fn recover(&self, reference: &str, realm: Realm) -> Result<EnrolmentBundle, KeychainError>;
}

/// The production keys: the effective user's login Keychain.
#[cfg(feature = "node-setup")]
pub(crate) struct LoginNodeKeys;

#[cfg(all(
    feature = "node-setup",
    target_os = "macos",
    feature = "macos-keychain"
))]
impl NodeKeys for LoginNodeKeys {
    fn create(&self, reference: &str, realm: Realm) -> Result<EnrolmentBundle, KeychainError> {
        MacOsKeyFacility::create(reference, realm)
    }

    fn open(
        &self,
        reference: &str,
        fingerprint: &[u8; 32],
    ) -> Result<Arc<dyn KeyFacility>, KeychainError> {
        let facility = MacOsKeyFacility::open(reference, fingerprint)?;
        Ok(Arc::new(facility))
    }

    fn recover(&self, reference: &str, realm: Realm) -> Result<EnrolmentBundle, KeychainError> {
        MacOsKeyFacility::recover_enrolment_bundle(reference, realm)
    }
}

// Off macOS, or without `macos-keychain`, `run_with` has already refused; these
// bodies are never reached in production and make no native call.
#[cfg(all(
    feature = "node-setup",
    not(all(target_os = "macos", feature = "macos-keychain"))
))]
impl NodeKeys for LoginNodeKeys {
    fn create(&self, _reference: &str, _realm: Realm) -> Result<EnrolmentBundle, KeychainError> {
        Err(KeychainError::LoginKeychainUnavailable)
    }

    fn open(
        &self,
        _reference: &str,
        _fingerprint: &[u8; 32],
    ) -> Result<Arc<dyn KeyFacility>, KeychainError> {
        Err(KeychainError::LoginKeychainUnavailable)
    }

    fn recover(&self, _reference: &str, _realm: Realm) -> Result<EnrolmentBundle, KeychainError> {
        Err(KeychainError::LoginKeychainUnavailable)
    }
}

/// Human consent for pinning a contact. `Ok(true)` only for the exact answer.
#[cfg(feature = "node-setup")]
pub(crate) trait Confirmation {
    /// Shows `prompt` and returns whether the human confirmed. A declined
    /// answer or end of input is `Ok(false)`; no terminal is an error.
    fn confirm(&mut self, prompt: &str) -> Result<bool, NodeSetupError>;
}

/// Confirmation through the controlling terminal. It reads and writes
/// `/dev/tty` and never touches standard input, which stays free for a piped
/// card.
#[cfg(feature = "node-setup")]
pub(crate) struct TtyConfirmation;

#[cfg(feature = "node-setup")]
impl Confirmation for TtyConfirmation {
    fn confirm(&mut self, prompt: &str) -> Result<bool, NodeSetupError> {
        let tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .map_err(|_| tty_unavailable())?;
        let mut reader = std::io::BufReader::new(&tty);
        let mut writer = &tty;
        confirm_on(&mut reader, &mut writer, prompt)
    }
}

/// Writes `prompt` (no line feed), reads one line of at most 256 bytes and
/// returns true only for the exact line `yes` ending in a line feed. Any other
/// line, end of input, over-long input or unreadable input cancels.
#[cfg(feature = "node-setup")]
fn confirm_on<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    prompt: &str,
) -> Result<bool, NodeSetupError> {
    writer
        .write_all(prompt.as_bytes())
        .and_then(|()| writer.flush())
        .map_err(|_| tty_unavailable())?;
    let mut line = String::new();
    match reader.by_ref().take(256).read_line(&mut line) {
        Ok(0) | Err(_) => Ok(false),
        Ok(_) => Ok(line == "yes\n"),
    }
}

/// The service connection a command uses. Production is HTTPS only.
#[cfg(feature = "node-setup")]
pub(crate) trait NodeService: Send + Sync {
    /// Builds the client a command uses to reach the channel service.
    fn client(
        &self,
        config: &NodeChannelConfig,
        facility: Arc<dyn KeyFacility>,
    ) -> Result<NodeClient, NodeSetupError>;
}

/// The production service connection: HTTPS origins only.
#[cfg(feature = "node-setup")]
pub(crate) struct HttpsNodeService;

#[cfg(feature = "node-setup")]
impl NodeService for HttpsNodeService {
    fn client(
        &self,
        config: &NodeChannelConfig,
        facility: Arc<dyn KeyFacility>,
    ) -> Result<NodeClient, NodeSetupError> {
        commands::setup_client(config, facility)
    }
}

/// Everything a command needs, passed by the dispatcher.
#[cfg(feature = "node-setup")]
pub(crate) struct CommandEnv<'a> {
    /// Absolute path of the one selected configuration file (cwd-resolved, never discovered).
    pub config_path: &'a Path,
    pub keys: &'a dyn NodeKeys,
    pub service: &'a dyn NodeService,
    pub confirmation: &'a mut dyn Confirmation,
    pub input: &'a mut dyn Read,
    pub output: &'a mut dyn Write,
    pub diagnostics: &'a mut dyn Write,
}

/// Refuses a build or platform that cannot run node setup, before any I/O.
/// The feature check comes first so a build without the feature says so on
/// every platform.
#[cfg(feature = "node-setup")]
#[rustfmt::skip]
fn refuse_unsupported(capabilities: NodeCapabilities) -> Result<(), NodeSetupError> {
    if !capabilities.macos_keychain { return Err(feature_unavailable()); }
    if !capabilities.native_macos {
        return Err(platform_unavailable());
    }
    Ok(())
}

/// Resolves the selected configuration path against the current directory.
/// No selection, including an empty one, is a refusal.
#[cfg(feature = "node-setup")]
fn select_config(selected: Option<&Path>) -> Result<PathBuf, NodeSetupError> {
    let path = match selected {
        Some(path) if !path.as_os_str().is_empty() => path,
        _ => return Err(config_not_selected()),
    };
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(|_| local_io("could not read the selected file"))
}

/// Runs one node command against injected keys, confirmation and streams.
/// Unsupported builds and a missing configuration selection are refused
/// before any configuration or Keychain access.
#[cfg(feature = "node-setup")]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_with(
    args: NodeArgs,
    capabilities: NodeCapabilities,
    keys: &dyn NodeKeys,
    service: &dyn NodeService,
    confirmation: &mut dyn Confirmation,
    input: &mut dyn Read,
    output: &mut dyn Write,
    diagnostics: &mut dyn Write,
) -> Result<(), NodeSetupError> {
    refuse_unsupported(capabilities)?;
    let config_path = select_config(args.config.as_deref())?;
    let mut env = CommandEnv {
        config_path: &config_path,
        keys,
        service,
        confirmation,
        input,
        output,
        diagnostics,
    };
    match &args.command {
        NodeCommand::Init(init) => commands::init(init, &mut env).await,
        NodeCommand::Assign { input } => commands::assign(input, &mut env).await,
        NodeCommand::Pin { card } => commands::pin(card, &mut env).await,
        NodeCommand::Status(status) => commands::status(status, &mut env).await,
    }
}

/// Runs a node command with the production keys, the controlling terminal and
/// the process streams. Output is held back and written only on success, so a
/// failure leaves standard output untouched.
#[cfg(feature = "node-setup")]
pub(crate) async fn run(args: NodeArgs) -> Result<(), NodeSetupError> {
    let mut input = std::io::stdin();
    let mut output = Vec::<u8>::new();
    let mut diagnostics = Vec::<u8>::new();
    run_with(
        args,
        NodeCapabilities::production(),
        &LoginNodeKeys,
        &HttpsNodeService,
        &mut TtyConfirmation,
        &mut input,
        &mut output,
        &mut diagnostics,
    )
    .await?;
    let mut stdout = std::io::stdout();
    stdout
        .write_all(&output)
        .and_then(|()| stdout.flush())
        .map_err(|_| {
            local_io("could not write public output; use node status to export it again")
        })?;
    let _ = std::io::stderr().write_all(&diagnostics);
    Ok(())
}

/// Without the `node-setup` feature the parser exists and every command is
/// refused before any configuration access.
#[cfg(not(feature = "node-setup"))]
pub(crate) async fn run(_args: NodeArgs) -> Result<(), NodeSetupError> {
    Err(feature_unavailable())
}

/// Command-local exit wrapper. A failure prints exactly one line to stderr and
/// exits with its typed code. `run` has returned by then, so every lock and
/// key facility is already dropped.
pub(crate) async fn run_cli(args: NodeArgs) -> anyhow::Result<()> {
    match run(args).await {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::io::stderr().write_all(failure_line(&error).as_bytes());
            std::process::exit(i32::from(error.exit));
        }
    }
}

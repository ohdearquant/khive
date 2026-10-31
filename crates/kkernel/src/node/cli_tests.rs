//! Tests for the node command-line entry: argument shapes, the feature and
//! platform refusals, consent reading, error mapping and the failure line.

use super::*;
use clap::{error::ErrorKind, Parser};

#[derive(Parser, Debug)]
#[command(name = "node-test")]
struct Wrapper {
    #[command(flatten)]
    node: NodeArgs,
}

fn parse(argv: &[&str]) -> Result<NodeArgs, clap::Error> {
    let mut full = vec!["node-test"];
    full.extend_from_slice(argv);
    Wrapper::try_parse_from(full).map(|wrapper| wrapper.node)
}

fn status_args(config: Option<PathBuf>) -> NodeArgs {
    NodeArgs {
        config,
        command: NodeCommand::Status(StatusArgs {
            bundle: false,
            card: false,
            out: None,
        }),
    }
}

#[test]
fn node_cli_parses_the_four_commands() {
    let init = parse(&[
        "init",
        "--config",
        "khive.toml",
        "--service-url",
        "https://service.example",
        "--realm",
        "realm-1",
        "--actor",
        "laptop",
    ])
    .expect("init parses");
    assert_eq!(
        init.config.as_deref(),
        Some(std::path::Path::new("khive.toml"))
    );
    match init.command {
        NodeCommand::Init(args) => {
            assert_eq!(args.service_url.as_deref(), Some("https://service.example"));
            assert_eq!(args.realm.as_deref(), Some("realm-1"));
            assert_eq!(args.actor.as_deref(), Some("laptop"));
            assert_eq!(args.slug, "node");
            assert_eq!(args.namespace, "local");
            assert!(!args.recover);
            assert_eq!(args.out, PathBuf::from("-"));
        }
        other => panic!("expected init, got {other:?}"),
    }

    let assign = parse(&["assign", "--config", "c.toml", "--input", "result.json"]).unwrap();
    assert!(matches!(
        assign.command,
        NodeCommand::Assign { ref input } if input == std::path::Path::new("result.json")
    ));

    let pin = parse(&["--config", "c.toml", "pin", "--card", "card.json"]).unwrap();
    assert!(matches!(
        pin.command,
        NodeCommand::Pin { ref card } if card == std::path::Path::new("card.json")
    ));

    let status = parse(&[
        "status", "--config", "c.toml", "--bundle", "--out", "b.json",
    ])
    .unwrap();
    match status.command {
        NodeCommand::Status(args) => {
            assert!(args.bundle && !args.card);
            assert_eq!(args.out.as_deref(), Some(std::path::Path::new("b.json")));
        }
        other => panic!("expected status, got {other:?}"),
    }
}

#[test]
fn node_cli_init_recover_accepts_no_other_init_option() {
    let recover = parse(&["init", "--config", "c.toml", "--recover", "--out", "b.json"])
        .expect("recover with --out parses");
    match recover.command {
        NodeCommand::Init(args) => {
            assert!(args.recover);
            assert!(args.service_url.is_none() && args.realm.is_none() && args.actor.is_none());
        }
        other => panic!("expected init, got {other:?}"),
    }
    for extra in [
        ["--realm", "r"],
        ["--service-url", "https://s.example"],
        ["--actor", "a"],
        ["--slug", "s"],
        ["--namespace", "n"],
    ] {
        let mut argv = vec!["init", "--config", "c.toml", "--recover"];
        argv.extend(extra);
        let error = parse(&argv).expect_err("recover with another init option must not parse");
        assert_eq!(error.kind(), ErrorKind::ArgumentConflict, "{extra:?}");
    }
}

#[test]
fn node_cli_rejects_incomplete_or_conflicting_arguments() {
    // init without --recover needs all three identity options.
    let missing = parse(&["init", "--config", "c.toml", "--realm", "r"]).unwrap_err();
    assert_eq!(missing.kind(), ErrorKind::MissingRequiredArgument);
    // --bundle and --card are mutually exclusive.
    let both = parse(&["status", "--config", "c.toml", "--bundle", "--card"]).unwrap_err();
    assert_eq!(both.kind(), ErrorKind::ArgumentConflict);
    // --out is only meaningful with --bundle or --card.
    let lone_out = parse(&["status", "--config", "c.toml", "--out", "f"]).unwrap_err();
    assert_eq!(lone_out.kind(), ErrorKind::MissingRequiredArgument);
    // assign and pin have no default selection.
    assert_eq!(
        parse(&["assign", "--config", "c.toml"]).unwrap_err().kind(),
        ErrorKind::MissingRequiredArgument
    );
    assert_eq!(
        parse(&["pin", "--config", "c.toml"]).unwrap_err().kind(),
        ErrorKind::MissingRequiredArgument
    );
    // There is no flag that skips confirmation.
    for flag in ["--yes", "--force"] {
        let error = parse(&["pin", "--config", "c.toml", "--card", "c.json", flag]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnknownArgument, "{flag}");
    }
}

#[test]
fn node_cli_failure_line_is_exact() {
    let error = NodeSetupError {
        exit: 2,
        message: "select the configuration file with --config".to_owned(),
    };
    assert_eq!(
        failure_line(&error),
        "node: select the configuration file with --config\n"
    );
    assert_eq!(error.to_string(), error.message);
}

#[cfg(feature = "node-setup")]
mod with_feature {
    use super::*;

    struct UntouchedKeys;

    impl NodeKeys for UntouchedKeys {
        fn create(&self, _: &str, _: Realm) -> Result<EnrolmentBundle, KeychainError> {
            unreachable!("a refusal must not touch the keychain")
        }
        fn open(&self, _: &str, _: &[u8; 32]) -> Result<Arc<dyn KeyFacility>, KeychainError> {
            unreachable!("a refusal must not touch the keychain")
        }
        fn recover(&self, _: &str, _: Realm) -> Result<EnrolmentBundle, KeychainError> {
            unreachable!("a refusal must not touch the keychain")
        }
    }

    struct UntouchedService;

    impl NodeService for UntouchedService {
        fn client(
            &self,
            _: &NodeChannelConfig,
            _: Arc<dyn KeyFacility>,
        ) -> Result<NodeClient, NodeSetupError> {
            unreachable!("a refusal must not reach the service")
        }
    }

    struct UntouchedConfirmation;

    impl Confirmation for UntouchedConfirmation {
        fn confirm(&mut self, _: &str) -> Result<bool, NodeSetupError> {
            unreachable!("a refusal must not ask for confirmation")
        }
    }

    async fn refusal(args: NodeArgs, capabilities: NodeCapabilities) -> NodeSetupError {
        let mut input = std::io::Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        let mut diagnostics = Vec::<u8>::new();
        let error = run_with(
            args,
            capabilities,
            &UntouchedKeys,
            &UntouchedService,
            &mut UntouchedConfirmation,
            &mut input,
            &mut output,
            &mut diagnostics,
        )
        .await
        .expect_err("the command must be refused");
        assert!(output.is_empty(), "a refusal writes nothing to output");
        assert!(diagnostics.is_empty(), "a refusal writes no diagnostics");
        error
    }

    fn capabilities(native_macos: bool, macos_keychain: bool) -> NodeCapabilities {
        NodeCapabilities {
            native_macos,
            macos_keychain,
        }
    }

    #[tokio::test]
    async fn node_cli_feature_and_platform_refusals() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("khive.toml");

        // Feature off on a macOS target: refused before any configuration access.
        let error = refusal(status_args(Some(config.clone())), capabilities(true, false)).await;
        assert_eq!(error.exit, 4);
        assert_eq!(
            error.message,
            "node setup requires a build with the macos-keychain feature"
        );

        // Feature on, not macOS: refused as a platform problem.
        let error = refusal(status_args(Some(config.clone())), capabilities(false, true)).await;
        assert_eq!(error.exit, 4);
        assert_eq!(error.message, "node setup requires macOS");

        // Both missing: the feature message wins on every platform.
        let error = refusal(
            status_args(Some(config.clone())),
            capabilities(false, false),
        )
        .await;
        assert_eq!(error.exit, 4);
        assert_eq!(
            error.message,
            "node setup requires a build with the macos-keychain feature"
        );

        // A supported build without a configuration selection is an input refusal.
        for selection in [None, Some(PathBuf::new())] {
            let error = refusal(status_args(selection), capabilities(true, true)).await;
            assert_eq!(error.exit, 2);
            assert_eq!(error.message, "select the configuration file with --config");
        }

        // None of the refusals created or touched anything.
        assert!(!config.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn node_cli_production_capabilities_follow_the_build() {
        let production = NodeCapabilities::production();
        assert_eq!(production.native_macos, cfg!(target_os = "macos"));
        assert_eq!(production.macos_keychain, cfg!(feature = "macos-keychain"));
    }

    #[test]
    fn node_cli_relative_config_resolves_against_the_current_directory() {
        let cwd = std::env::current_dir().unwrap();
        let relative = select_config(Some(std::path::Path::new("khive.toml"))).unwrap();
        assert_eq!(relative, cwd.join("khive.toml"));
        let absolute = cwd.join("elsewhere.toml");
        assert_eq!(select_config(Some(&absolute)).unwrap(), absolute);
    }

    const PROMPT: &str =
        "Have you compared every group with this contact on another channel? Type yes to confirm: ";

    fn answer(input: &str) -> (bool, String) {
        let mut reader = std::io::Cursor::new(input.as_bytes().to_vec());
        let mut writer = Vec::<u8>::new();
        let confirmed = confirm_on(&mut reader, &mut writer, PROMPT).unwrap();
        (confirmed, String::from_utf8(writer).unwrap())
    }

    #[test]
    fn node_cli_confirmation_accepts_only_the_exact_yes_line() {
        let (confirmed, shown) = answer("yes\n");
        assert!(confirmed);
        assert_eq!(
            shown, PROMPT,
            "the prompt is written once, without a line feed"
        );

        for refused in [
            "",
            "\n",
            "y\n",
            "Yes\n",
            "YES\n",
            " yes\n",
            "yes \n",
            "yes please\n",
            "no\n",
            "yes",
            "yes\r\n",
        ] {
            assert!(!answer(refused).0, "{refused:?} must not confirm");
        }
        // Only the first line counts.
        assert!(!answer("no\nyes\n").0);
        // Invalid UTF-8 and over-long input cancel.
        let mut reader = std::io::Cursor::new(vec![0xff, 0xfe, b'\n']);
        assert!(!confirm_on(&mut reader, &mut Vec::<u8>::new(), PROMPT).unwrap());
        let long = format!("{}\n", "y".repeat(1000));
        assert!(!answer(&long).0);
    }

    #[test]
    fn node_cli_keychain_failures_map_to_exit_four_messages() {
        let cases = [
            (
                KeychainError::HomeUnavailable,
                "effective-user home directory is unavailable",
            ),
            (
                KeychainError::LoginKeychainUnavailable,
                "login keychain is unavailable",
            ),
            (
                KeychainError::ItemUnavailable,
                "key item is unavailable; unlock the login keychain and retry",
            ),
            (
                KeychainError::InvalidMaterial,
                "stored key material is invalid",
            ),
            (
                KeychainError::PublicKeyMismatch,
                "stored keys do not match the expected fingerprint",
            ),
            (
                KeychainError::RandomnessUnavailable,
                "operating-system randomness is unavailable",
            ),
            (
                KeychainError::AlreadyExists,
                "a key item already exists for this reference; use node init --recover for an incomplete initialization",
            ),
        ];
        for (error, message) in cases {
            let mapped = NodeSetupError::from(error);
            assert_eq!(mapped.exit, 4);
            assert_eq!(mapped.message, message);
        }
    }
}

#[cfg(not(feature = "node-setup"))]
#[tokio::test]
async fn node_cli_feature_and_platform_refusals() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("khive.toml");
    let error = run(status_args(Some(config.clone())))
        .await
        .expect_err("a build without the feature refuses");
    assert_eq!(error.exit, 4);
    assert_eq!(
        error.message,
        "node setup requires a build with the macos-keychain feature"
    );
    assert!(!config.exists());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

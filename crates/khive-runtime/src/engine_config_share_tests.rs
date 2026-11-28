//! ADR-220 Amendment 1, item 2 and acceptance arm S11: the `[share]` table's
//! rules, read through the real configuration loader.

use super::*;
use crate::KhiveConfig;

const TARGET: &str = "[[share.targets]]\nname = \"build-1\"\nurl = \"https://build-1.example/mcp\"\nkey_file = \"/home/example/.secrets/share-build-1.key\"\n";

fn load(text: &str) -> Result<Option<KhiveConfig>, ConfigError> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, text).unwrap();
    KhiveConfig::load(Some(&path))
}

fn accepted(text: &str) -> ShareSectionConfig {
    match load(text) {
        Ok(Some(config)) => config.share.expect("the [share] table is read"),
        Ok(None) => panic!("the configuration file was not found"),
        Err(error) => panic!("expected acceptance of:\n{text}\ngot: {error}"),
    }
}

fn refused(text: &str) -> String {
    match load(text) {
        Ok(_) => panic!("expected a refusal of:\n{text}"),
        // The loader names the file after the rule; each arm checks the rule.
        Err(error) => match error.to_string().split_once(" (config file: ") {
            Some((rule, _)) => rule.to_owned(),
            None => error.to_string(),
        },
    }
}

/// A `[share]` table with `extra` lines, then one target.
fn table(extra: &str) -> String {
    format!("[share]\nroot = \"/home/example/share\"\n{extra}\n{TARGET}")
}

#[test]
fn a_table_with_one_target_loads_with_the_defaults_and_an_absent_one_is_none() {
    assert_eq!(
        accepted(&table("")),
        ShareSectionConfig {
            root: PathBuf::from("/home/example/share"),
            max_file_bytes: 67_108_864,
            max_total_bytes: 268_435_456,
            max_files: 256,
            targets: vec![ShareTargetConfig {
                name: "build-1".to_owned(),
                url: "https://build-1.example/mcp".to_owned(),
                key_file: PathBuf::from("/home/example/.secrets/share-build-1.key"),
                call_timeout_s: 3700,
            }],
        }
    );
    let config = load("[actor]\nid = \"lambda:test\"\n").unwrap().unwrap();
    assert!(config.share.is_none());
}

#[test]
fn each_limit_is_accepted_at_its_maximum() {
    let share = accepted(&table(
        "max_file_bytes = 67108864\nmax_total_bytes = 1073741824\nmax_files = 1000",
    ));
    assert_eq!(share.max_file_bytes, MAX_SHARE_FILE_BYTES);
    assert_eq!(share.max_total_bytes, MAX_SHARE_TOTAL_BYTES);
    assert_eq!(share.max_files, MAX_SHARE_FILES);
}

#[test]
fn s11_a_relative_root_fails_startup() {
    let text = TARGET.to_owned();
    let error = refused(&format!("[share]\nroot = \"share\"\n\n{text}"));
    assert_eq!(error, "[share].root: is not an absolute path");
}

#[test]
fn s11_a_plain_http_target_on_a_host_that_is_not_loopback_fails_startup() {
    let text = "[share]\nroot = \"/home/example/share\"\n\n[[share.targets]]\nname = \"build-1\"\n\
                url = \"http://build-1.example/mcp\"\nkey_file = \"/k\"\n";
    let error = refused(text);
    assert!(
        error
            .starts_with("[share].targets[0].url: plain http is accepted only for a loopback host"),
        "{error}"
    );
    // The same URL on a loopback host is accepted.
    accepted(&text.replace("build-1.example", "127.0.0.1:8700"));
}

#[test]
fn s11_two_targets_with_one_name_fail_startup() {
    let error = refused(&format!("{}\n{TARGET}", table("")));
    assert_eq!(
        error,
        "[share].targets[1].name: \"build-1\" names an earlier target too; target names are distinct"
    );
}

#[test]
fn s11_a_limit_over_its_maximum_or_zero_fails_startup() {
    for (line, key, max) in [
        ("max_file_bytes = 67108865", "max_file_bytes", "67108864"),
        (
            "max_total_bytes = 1073741825",
            "max_total_bytes",
            "1073741824",
        ),
        ("max_files = 1001", "max_files", "1000"),
        ("max_file_bytes = 0", "max_file_bytes", "67108864"),
        ("max_files = 0", "max_files", "1000"),
    ] {
        let error = refused(&table(line));
        let got = line.rsplit(' ').next().unwrap();
        assert_eq!(
            error,
            format!("[share].{key}: must be between 1 and {max}; got {got}"),
            "{line}"
        );
    }
}

#[test]
fn s11_a_table_with_no_target_fails_startup() {
    let error = refused("[share]\nroot = \"/home/example/share\"\n");
    assert!(
        error.starts_with("[share].targets: names no target"),
        "{error}"
    );
}

#[test]
fn the_table_and_each_target_are_closed() {
    let error = refused(&table("maximum_files = 3"));
    assert!(error.contains("maximum_files"), "{error}");
    let error = refused(&format!("{}packs = [\"blob\"]\n", table("")));
    assert!(error.contains("packs"), "{error}");
}

#[test]
fn a_target_follows_the_computer_rules_for_its_name_key_file_and_timeout() {
    let error = refused(&table("").replace("\"build-1\"", "\"Build_1\""));
    assert!(
        error.starts_with("[share].targets[0].name: must be 1 to 64 bytes"),
        "{error}"
    );
    let error =
        refused(&table("").replace("\"/home/example/.secrets/share-build-1.key\"", "\"k\""));
    assert_eq!(
        error,
        "[share].targets[0].key_file: is not an absolute path"
    );
    let error = refused(&format!("{}call_timeout_s = 7201\n", table("")));
    assert_eq!(
        error,
        "[share].targets[0].call_timeout_s: must be between 1 and 7200 seconds; got 7201"
    );
    let error = refused(&table("").replace(
        "https://build-1.example/mcp",
        "https://u@build-1.example/mcp",
    ));
    assert!(
        error.starts_with("[share].targets[0].url: carries a user name or password"),
        "{error}"
    );
}

#[test]
fn target_finds_a_target_by_its_name_only() {
    let share = accepted(&table(""));
    assert_eq!(
        share.target("build-1").map(|t| t.name.as_str()),
        Some("build-1")
    );
    assert!(share.target("build-2").is_none());
}

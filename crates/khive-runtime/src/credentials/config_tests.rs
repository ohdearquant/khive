use crate::engine_config::{ConfigError, KhiveConfig};

use super::*;

const DECLARATION: &str = r#"
[[credentials]]
name = "receipt-key"
kind = "signing_key"
provider = "env"
env_var = "KHIVE_S1_RECEIPT_KEY"
"#;

const RING: &str = r#"
[visibility_receipts]
[[visibility_receipts.keys]]
id = "current-1"
credential = "receipt-key"
encrypt = true
"#;

fn key(id: &str, encrypt: bool) -> VisibilityReceiptKeyConfig {
    VisibilityReceiptKeyConfig {
        id: id.to_owned(),
        credential: "receipt-key".to_owned(),
        encrypt,
    }
}

#[test]
fn config_load_accepts_references_without_resolving_environment() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.toml");
    let text = format!("{DECLARATION}{RING}");
    std::fs::write(&path, &text).unwrap();
    let config = KhiveConfig::load(Some(&path)).unwrap().unwrap();
    assert_eq!(config.credentials.len(), 1);
    let ring = config.visibility_receipts.as_ref().unwrap();
    assert_eq!(ring.keys.len(), 1);
    assert!(ring.keys[0].encrypt);
    assert!(KhiveConfig::load_with_home_fallback(Some(&path), None)
        .unwrap()
        .is_some());
    assert!(
        KhiveConfig::load_with_home_fallback_and_source(Some(&path), None)
            .unwrap()
            .is_some()
    );

    let custom = DECLARATION.replace(
        "provider = \"env\"\nenv_var = \"KHIVE_S1_RECEIPT_KEY\"",
        "provider = \"host-vault\"",
    );
    let config: KhiveConfig = toml::from_str(&format!("{custom}{RING}")).unwrap();
    config.validate().unwrap();
    let registry = CredentialRegistry::new(config.credentials).unwrap();
    assert!(matches!(
        registry.resolve("receipt-key"),
        Err(CredentialError::UnknownProvider { .. })
    ));
}

#[test]
fn closed_credential_and_receipt_tables_reject_unknown_fields_at_load() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.toml");
    for text in [
        format!("{DECLARATION}secret = 'disallowed-inline-value'\n{RING}"),
        format!("{DECLARATION}env_vra = 'TYPO'\n{RING}"),
        format!("{DECLARATION}[visibility_receipts]\nunknown = true\n"),
        format!("{DECLARATION}{RING}unknown = true\n"),
        format!("{DECLARATION}{RING}key_bytes = 'disallowed-inline-value'\n"),
    ] {
        std::fs::write(&path, text).unwrap();
        for error in [
            KhiveConfig::load(Some(&path)).unwrap_err(),
            KhiveConfig::load_with_home_fallback(Some(&path), None).unwrap_err(),
            KhiveConfig::load_with_home_fallback_and_source(Some(&path), None).unwrap_err(),
        ] {
            assert!(matches!(error, ConfigError::Parse { .. }));
        }
    }
}

#[test]
fn receipt_ring_requires_one_encrypting_key_and_valid_unique_ids() {
    let config: KhiveConfig = toml::from_str(DECLARATION).unwrap();
    for keys in [
        vec![],
        vec![key("current", false)],
        vec![key("current", true), key("old", true)],
        vec![key("same", true), key("same", false)],
        vec![key("", true)],
        vec![key(&"a".repeat(65), true)],
        vec![key("../bad", true)],
        vec![key("unicode-é", true)],
    ] {
        let ring = VisibilityReceiptConfig { keys };
        assert!(ring.validate(&config.credentials).is_err());
        let with_ring = KhiveConfig {
            visibility_receipts: Some(ring),
            ..config.clone()
        };
        assert!(with_ring.validate().is_err());
    }
    for id in [
        "a".to_owned(),
        "a".repeat(64),
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._".to_owned(),
    ] {
        VisibilityReceiptConfig {
            keys: vec![key(&id, true), key("old-key", false)],
        }
        .validate(&config.credentials)
        .unwrap();
    }
}

#[test]
fn logical_credential_errors_reject_all_startup_load_paths() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.toml");
    let valid = format!("{DECLARATION}{RING}");
    for text in [
        valid.replace("encrypt = true", "encrypt = false"),
        format!("{valid}\n[[visibility_receipts.keys]]\nid='other'\ncredential='receipt-key'\nencrypt=true\n"),
        valid.replace("credential = \"receipt-key\"", "credential = \"missing\""),
        valid.replace("kind = \"signing_key\"", "kind = \"basic\""),
        valid.replace("id = \"current-1\"", "id = \"bad/key\""),
        format!("{DECLARATION}{DECLARATION}{RING}"),
        valid.replace("env_var = \"KHIVE_S1_RECEIPT_KEY\"", "env_var = \"\""),
        format!("{DECLARATION}header = 'X-Api-Key'\n{RING}"),
        valid.replace("kind = \"signing_key\"", "kind = \"header\""),
    ] {
        std::fs::write(&path, text).unwrap();
        assert!(KhiveConfig::load(Some(&path)).is_err());
        assert!(KhiveConfig::load_with_home_fallback(Some(&path), None).is_err());
        assert!(KhiveConfig::load_with_home_fallback_and_source(Some(&path), None).is_err());
    }
}

#[test]
fn declaration_kinds_fields_and_default_decrypt_only_are_checked() {
    for kind in ["header", "basic", "cookie_jar", "signing_key"] {
        let header = if kind == "header" {
            "header='X-Api-Key'\n"
        } else {
            ""
        };
        let text = format!("[[credentials]]\nname='token'\nkind='{kind}'\nprovider='env'\nenv_var='TOKEN'\n{header}");
        let config: KhiveConfig = toml::from_str(&text).unwrap();
        config.validate().unwrap();
    }
    for replacement in ["name = \"\"", "provider = \"\"", "env_var = \"BAD=NAME\""] {
        let field = replacement.split(' ').next().unwrap();
        let text = DECLARATION
            .lines()
            .map(|line| {
                if line.starts_with(&format!("{field} =")) {
                    replacement
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let config: KhiveConfig = toml::from_str(&text).unwrap();
        assert!(config.validate().is_err());
    }
    let default_key: VisibilityReceiptKeyConfig =
        toml::from_str("id='old'\ncredential='receipt-key'").unwrap();
    assert!(!default_key.encrypt);
    let bad_kind = DECLARATION.replace("signing_key", "password");
    assert!(toml::from_str::<KhiveConfig>(&bad_kind).is_err());
    let bad_header = DECLARATION.replace("signing_key", "header") + "header='X-Invalid Header'\n";
    assert!(toml::from_str::<KhiveConfig>(&bad_header)
        .unwrap()
        .validate()
        .is_err());
}

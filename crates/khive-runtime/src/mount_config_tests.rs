use crate::engine_config::KhiveConfig;

fn config(tail: &str) -> String {
    format!("[[mounts]]\nname = \"demo\"\ntransport = \"stdio\"\ncommand = \"fixture\"\n{tail}\n")
}

fn validation_error(input: &str) -> String {
    match toml::from_str::<KhiveConfig>(input) {
        Err(error) => error.to_string(),
        Ok(config) => config
            .validate()
            .expect_err("invalid mount was silently accepted")
            .to_string(),
    }
}

#[test]
fn mount_config_accepts_credential_reference() {
    let cfg: KhiveConfig = toml::from_str(&config("credential = \"MOUNT_API_KEY\"\nenv = [\"PATH\"]\ntools = [{ name = \"echo\", effect = \"read\" }]" )).unwrap();
    cfg.validate().unwrap();
}

#[test]
fn mount_config_refuses_inline_credential_without_echoing_it() {
    let secret = "sk-DO-NOT-LOG-this-example-value";
    let error = validation_error(&config(&format!("credential = {secret:?}")));
    assert!(error.contains("credential"), "{error}");
    assert!(!error.contains(secret), "credential was exposed: {error}");
}

#[test]
fn mount_config_refuses_http() {
    let error = validation_error(&config("").replace("stdio", "http"));
    assert!(error.contains("stdio"), "{error}");
}

#[test]
fn mount_config_refuses_invalid_names() {
    for name in ["", "Demo", "demo.tool", "two words", "é"] {
        let error =
            validation_error(&config("").replace("name = \"demo\"", &format!("name = {name:?}")));
        assert!(error.contains("name"), "{error}");
    }
}

#[test]
fn mount_config_refuses_environment_values() {
    let error = validation_error(&config("env = [\"TOKEN=DO-NOT-LOG\"]"));
    assert!(error.contains("env"), "{error}");
    assert!(!error.contains("DO-NOT-LOG"), "{error}");
}

#[test]
fn mount_config_refuses_duplicate_mounts_and_tools() {
    assert!(validation_error(&(config("") + &config(""))).contains("duplicate"));
    assert!(
        validation_error(&config("tools = [{name=\"echo\"}, {name=\"echo\"}]"))
            .contains("duplicate")
    );
}

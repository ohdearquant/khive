//! The lightweight identity probe reports the build of the binary serving it.

use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

fn command(home: &TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kkernel"));
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("KHIVE_") {
            command.env_remove(name);
        }
    }
    command
        .current_dir(home.path())
        .env("HOME", home.path())
        .env("KHIVE_NO_DAEMON", "1")
        .env("KHIVE_PACKS", "kg")
        .env("RUST_LOG", "error");
    command
}

fn successful(output: Output) -> Vec<u8> {
    assert!(
        output.status.success(),
        "command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn request(home: &TempDir, ops: &str) -> Value {
    let output = command(home)
        .args([
            "exec",
            ops,
            "--db",
            ":memory:",
            "--presentation",
            "verbose",
            "--strict",
        ])
        .output()
        .expect("run identity probe");
    let envelope: Value = serde_json::from_slice(&successful(output)).expect("JSON envelope");
    assert_eq!(envelope["results"][0]["ok"], true);
    envelope["results"][0]["result"].clone()
}

#[test]
fn whoami_build_matches_binary_version() {
    let home = TempDir::new().expect("isolated home");
    let version = String::from_utf8(successful(
        command(&home)
            .arg("--version")
            .output()
            .expect("run binary version"),
    ))
    .expect("version is UTF-8");
    let identity = request(&home, "whoami()");
    let build = identity["build"].as_object().expect("build object");
    assert_eq!(build.len(), 2, "only version and revision are exposed");
    let package_version = build["version"].as_str().expect("package version");
    let revision = build["revision"].as_str().expect("source revision");
    let version_prefix = format!("kkernel {package_version} (revision {revision}, built ");
    assert!(
        version.starts_with(&version_prefix),
        "whoami build must match the same executable's --version: {version:?}, {build:?}"
    );
    assert_eq!(identity["actor_id"], "local");
    assert_eq!(identity["unattributed"], true);
}

#[test]
fn whoami_help_describes_build_fields() {
    let home = TempDir::new().expect("isolated home");
    let help = request(&home, "whoami(help=true)");
    let description = help["description"].as_str().expect("help description");
    assert!(description.contains("build.version"), "{description}");
    assert!(description.contains("build.revision"), "{description}");
    assert!(help["params"].as_array().expect("parameters").is_empty());
}

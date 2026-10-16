//! Pack versions are visible through both command output formats.

use std::collections::BTreeSet;
use std::process::Command;

// Retain the same linked pack factories as the command.
use kkernel as _;

#[test]
fn pack_list_reports_linked_versions_in_json_and_human_output() {
    let root = tempfile::tempdir().expect("private command directory");
    let run = |human: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kkernel"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("KHIVE_") {
                command.env_remove(key);
            }
        }
        command
            .current_dir(root.path())
            .env("HOME", root.path())
            .env_remove("LATTICE_MODEL_CACHE")
            .env("KHIVE_VOLUME_LOCK_DIR", root.path().join("volume-locks"))
            .env("KHIVE_LOCK", root.path().join("recovery.lock"))
            .env("KHIVE_NO_DAEMON", "1")
            .env("KHIVE_NO_EMBED", "true")
            .args(["pack", "list"]);
        if human {
            command.arg("--human");
        }
        let output = command.output().expect("run pack list");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("UTF-8 command output")
    };
    let json: serde_json::Value = serde_json::from_str(&run(false)).expect("JSON pack list");
    let packs = json.as_array().expect("pack list array");
    assert!(!packs.is_empty(), "linked packs must be present");
    assert!(packs.iter().any(|pack| pack["name"] == "kg"));
    let names: BTreeSet<_> = packs
        .iter()
        .map(|pack| pack["name"].as_str().expect("pack name"))
        .collect();
    let discovered: BTreeSet<_> = khive_runtime::PackRegistry::discovered_names()
        .into_iter()
        .collect();
    assert_eq!(names, discovered, "no linked pack may lose its descriptor");
    let human = run(true);
    let headers: Vec<_> = human
        .lines()
        .filter(|line| line.starts_with("# "))
        .collect();
    assert_eq!(headers.len(), packs.len());
    for pack in packs {
        let name = pack["name"].as_str().expect("pack name");
        let version = pack["version"].as_str().expect("version string");
        assert!(!version.is_empty(), "{name} has no version");
        let registration = inventory::iter::<khive_runtime::PackRegistration>
            .into_iter()
            .find(|registration| registration.0.name() == name)
            .expect("listed pack must have a named factory");
        assert_eq!(version, registration.0.version(), "{name}");
        let verb_count = pack["verbs"].as_array().expect("verb list").len();
        let expected = format!("# {name} {version} ({verb_count} verbs)");
        assert!(
            headers.contains(&expected.as_str()),
            "missing header: {expected}"
        );
    }
}

use std::collections::BTreeMap;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::process::{Command, Stdio};

use super::*;

#[test]
fn natural_ids_are_domain_separated_and_stable() {
    assert_eq!(
        natural_id("module", &["a", "b"]),
        natural_id("module", &["a", "b"])
    );
    assert_ne!(
        natural_id("module", &["a", "b"]),
        natural_id("commit", &["a", "b"])
    );
    assert_ne!(
        natural_id("module", &["ab"]),
        natural_id("module", &["a", "b"])
    );
}

#[test]
fn derives_code_ingest_rust_module_paths() {
    let fixture = tempfile::tempdir().unwrap();
    fs::write(
        fixture.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    let paths = [
        "Cargo.toml",
        "src/lib.rs",
        "src/main.rs",
        "src/a/mod.rs",
        "src/a/b/mod.rs",
        "build.rs",
        "tests/support/mod.rs",
        "benches/measure.rs",
        "examples/hello.rs",
    ]
    .map(str::to_string);
    for path in paths.iter().filter(|path| path.as_str() != "Cargo.toml") {
        let path = fixture.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
    }

    let actual = derive_rust_module_keys(fixture.path(), &paths)
        .unwrap()
        .into_iter()
        .map(|(path, key, _)| (path, key.unwrap().module_path))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(actual["src/lib.rs"], "crate");
    assert_eq!(actual["src/main.rs"], "crate::main");
    assert_eq!(actual["src/a/mod.rs"], "a");
    assert_eq!(actual["src/a/b/mod.rs"], "a::b");
    assert_eq!(actual["build.rs"], "build");
    assert_eq!(actual["tests/support/mod.rs"], "tests::support");
    assert_eq!(actual["benches/measure.rs"], "benches::measure");
    assert_eq!(actual["examples/hello.rs"], "examples::hello");
}

#[test]
fn rejects_oversized_or_special_cargo_manifests_before_reading() {
    let oversized = tempfile::tempdir().unwrap();
    let manifest = oversized.path().join("Cargo.toml");
    let file = fs::File::create(&manifest).unwrap();
    file.set_len(MAX_CARGO_MANIFEST_BYTES + 1).unwrap();
    let error = derive_rust_module_keys(
        oversized.path(),
        &["Cargo.toml".into(), "src/lib.rs".into()],
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("exceeding the 1048576-byte limit"),
        "{error}"
    );

    let special = tempfile::tempdir().unwrap();
    fs::create_dir(special.path().join("Cargo.toml")).unwrap();
    let error =
        derive_rust_module_keys(special.path(), &["Cargo.toml".into(), "src/lib.rs".into()])
            .unwrap_err()
            .to_string();
    assert!(error.contains("must be a regular file"), "{error}");

    let unsafe_path = tempfile::tempdir().unwrap();
    let error = derive_rust_module_keys(
        unsafe_path.path(),
        &["../Cargo.toml".into(), "src/lib.rs".into()],
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("not a safe repository-relative path"),
        "{error}"
    );
}

#[cfg(unix)]
#[test]
fn rejects_cargo_manifest_symlinks() {
    use std::os::unix::fs::symlink;

    let fixture = tempfile::tempdir().unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    symlink(outside.path(), fixture.path().join("Cargo.toml")).unwrap();
    let error =
        derive_rust_module_keys(fixture.path(), &["Cargo.toml".into(), "src/lib.rs".into()])
            .unwrap_err()
            .to_string();
    assert!(error.contains("must not be a symlink"), "{error}");
}

#[test]
fn tracked_paths_follow_head_even_when_index_changes() {
    let fixture = tempfile::tempdir().unwrap();
    let repo = fixture.path();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q"]);
    fs::create_dir(repo.join("src")).unwrap();
    fs::write(repo.join("src/lib.rs"), "pub fn live() {}\n").unwrap();
    fs::write(repo.join("src/gone.rs"), "pub fn still_in_head() {}\n").unwrap();
    git(&["add", "src/lib.rs", "src/gone.rs"]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "-qm",
        "base",
    ]);
    let head = head_sha(repo).unwrap();

    fs::write(repo.join("src/extra.rs"), "pub fn staged_only() {}\n").unwrap();
    fs::remove_file(repo.join("src/gone.rs")).unwrap();
    git(&["add", "-A"]);

    assert_eq!(
        tracked_paths(repo, &head).unwrap(),
        vec!["src/gone.rs", "src/lib.rs"]
    );
}

/// A repository whose HEAD carries a signature header and whose local
/// configuration points signature display and verification at a helper
/// that records its invocation. Returns the helper's marker path.
#[cfg(unix)]
fn signed_head_with_marker_verifier(repo: &Path) -> PathBuf {
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    git(&["init", "-q"]);
    fs::write(repo.join("README.md"), "signed fixture\n").unwrap();
    git(&["add", "README.md"]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "-qm",
        "base",
    ]);

    // Add a gpgsig header without depending on a real signing key.
    let original = git(&["cat-file", "commit", "HEAD"]);
    let split = original
        .windows(2)
        .position(|pair| pair == b"\n\n")
        .unwrap();
    let mut signed = Vec::new();
    signed.extend_from_slice(&original[..=split]);
    signed.extend_from_slice(
        b"gpgsig -----BEGIN PGP SIGNATURE-----\n fake\n -----END PGP SIGNATURE-----\n",
    );
    signed.extend_from_slice(&original[split + 1..]);
    let mut writer = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["hash-object", "-t", "commit", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    writer.stdin.take().unwrap().write_all(&signed).unwrap();
    let output = writer.wait_with_output().unwrap();
    assert!(output.status.success());
    let oid = String::from_utf8(output.stdout).unwrap();
    git(&["update-ref", "HEAD", oid.trim()]);

    let verifier = repo.join("fake-verifier.sh");
    let marker = repo.join("fake-verifier.sh.marker");
    fs::write(&verifier, "#!/bin/sh\n: > \"$0.marker\"\nexit 1\n").unwrap();
    fs::set_permissions(&verifier, fs::Permissions::from_mode(0o700)).unwrap();
    git(&["config", "log.showSignature", "true"]);
    git(&["config", "gpg.format", "openpgp"]);
    git(&["config", "gpg.program", verifier.to_str().unwrap()]);
    marker
}

#[cfg(unix)]
#[test]
fn signed_head_does_not_run_repo_configured_verifier() {
    let fixture = tempfile::tempdir().unwrap();
    let repo = fixture.path();
    let marker = signed_head_with_marker_verifier(repo);

    let _ = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["show", "-s", "--format=%cI", "HEAD"])
        .output()
        .unwrap();
    assert!(
        marker.exists(),
        "fixture must execute the unguarded verifier"
    );
    fs::remove_file(&marker).unwrap();

    assert!(!head_committed_at(repo).unwrap().is_empty());
    assert!(
        !marker.exists(),
        "showcase Git read must not run the verifier"
    );
}

#[cfg(unix)]
#[test]
fn explicit_signature_request_does_not_run_repo_configured_verifier() {
    let fixture = tempfile::tempdir().unwrap();
    let repo = fixture.path();
    let marker = signed_head_with_marker_verifier(repo);
    let args = ["log", "-1", "--show-signature", "--format=%cI", "HEAD"];

    let _ = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        marker.exists(),
        "fixture must execute the unguarded verifier"
    );
    fs::remove_file(&marker).unwrap();

    assert!(!git_text(repo, &args).unwrap().is_empty());
    assert!(
        !marker.exists(),
        "showcase Git read must not run the verifier"
    );
}

#[test]
fn rejects_non_utf8_tracked_paths() {
    let error = decode_nul_paths(b"invalid-\xff.rs\0".to_vec(), "tracked repository path")
        .unwrap_err()
        .to_string();
    assert!(error.contains("not valid UTF-8"), "{error}");
}

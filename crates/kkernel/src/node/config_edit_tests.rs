//! Tests for `config_edit`: lossless editing and the crash/concurrency contract.
//! Exact strings are copied from the design's refusal table, not from the
//! implementation.

use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::PathBuf;

use khive_runtime::node_config::{NodeChannelConfig, NodePinConfig};
use tempfile::TempDir;

use super::config_edit::{
    apply_edits, lock_config, plan_edit, publish_config, read_config, save_public_file,
    ConfigSnapshot, NodeEdit, TextEdit,
};
use super::NodeSetupError;

const LAYOUT: &str = "node configuration must use normal tables and array-of-table pins";
const UNSAFE: &str =
    "configuration must be an owned regular file without links, extended attributes or an extended ACL";
const GUARD: &str = "configuration edit would change unrelated content";
const BUSY: &str = "another node setup command is using this configuration";
const CHANGED: &str = "configuration changed during setup; run the command again";

fn hex64(pair: &str) -> String {
    pair.repeat(32)
}

fn node_cfg() -> NodeChannelConfig {
    toml::from_str(&format!(
        r#"
enabled = false
slug = "n1"
namespace = "local"
actor = "a"
realm = "r1"
service_url = "https://hub.example"
agent_id = "11111111-1111-4111-8111-111111111111"
device_id = "22222222-2222-4222-8222-222222222222"
key_epoch = 1
key_reference = "ref1"
key_fingerprint = "{}"
"#,
        hex64("ab")
    ))
    .expect("node config fixture deserializes")
}

fn pin() -> NodePinConfig {
    toml::from_str(&format!(
        r#"
agent_id = "33333333-3333-4333-8333-333333333333"
device_id = "44444444-4444-4444-8444-444444444444"
key_epoch = 1
kem_public_key = "{}"
signing_public_key = "{}"
fingerprint = "{}"
"#,
        hex64("01"),
        hex64("02"),
        hex64("03")
    ))
    .expect("pin fixture deserializes")
}

fn snapshot_of(bytes: &str) -> ConfigSnapshot {
    ConfigSnapshot {
        path: PathBuf::from("unused.toml"),
        bytes: bytes.as_bytes().to_vec(),
        config: toml::from_str("").expect("empty config deserializes"),
    }
}

fn refusal<T>(r: Result<T, NodeSetupError>) -> (u8, String) {
    match r {
        Ok(_) => panic!("expected a refusal"),
        Err(e) => (e.exit, e.message),
    }
}

fn plan_and_apply(original: &str, edit: &NodeEdit) -> String {
    let snap = snapshot_of(original);
    let edits = plan_edit(&snap, edit).expect("plan succeeds");
    String::from_utf8(apply_edits(original.as_bytes(), &edits).expect("apply succeeds"))
        .expect("utf8")
}

#[test]
fn node_config_preserves_comments_and_unrelated_bytes() {
    for nl in ["\n", "\r\n"] {
        let original = [
            "# header comment",
            "[channel.node]  # node comment",
            "\"slug\" = \"old\"  # keep slug comment",
            &format!("key_fingerprint = \"{}\"  # fp comment", hex64("ab")),
            "",
        ]
        .join(nl);

        // One existing scalar value replaced; every other byte identical.
        let recovered = plan_and_apply(&original, &NodeEdit::RecoverFingerprint(hex64("cd")));
        assert_eq!(recovered, original.replace(&hex64("ab"), &hex64("cd")));

        // Prepare: existing value replaced in place, missing members inserted
        // immediately after the header line using the file's newline style.
        let prepared = plan_and_apply(&original, &NodeEdit::Prepare(node_cfg()));
        assert!(prepared.starts_with(&format!(
            "# header comment{nl}[channel.node]  # node comment{nl}"
        )));
        assert!(prepared.contains(&format!("\"slug\" = \"n1\"  # keep slug comment{nl}")));
        assert!(prepared.contains(&format!("  # fp comment{nl}")));
        assert!(prepared.contains(&format!("enabled = false{nl}")));
        if nl == "\r\n" {
            assert_eq!(
                prepared.matches('\n').count(),
                prepared.matches("\r\n").count()
            );
        }
        let reparsed: toml::Table = toml::from_str(&prepared).expect("candidate reparses");
        assert_eq!(reparsed["channel"]["node"]["slug"].as_str(), Some("n1"));

        // Missing node table: original kept as an exact prefix, new table at EOF.
        let bare = ["# only a comment", ""].join(nl);
        let appended = plan_and_apply(&bare, &NodeEdit::Prepare(node_cfg()));
        assert!(appended.starts_with(&bare));
        assert!(appended.contains(&format!("[channel.node]{nl}")));
    }

    // Unsupported layouts are refused with the layout message, never rewritten.
    let layouts = [
        "[channel]\nnode = { slug = \"x\" }\n",
        "channel.node.slug = \"x\"\n",
        "[channel.node]\npins = [{ agent_id = \"x\", device_id = \"x\", key_epoch = 1, kem_public_key = \"x\", signing_public_key = \"x\", fingerprint = \"x\" }]\n",
    ];
    for layout in layouts {
        let snap = snapshot_of(layout);
        let (exit, message) = refusal(plan_edit(&snap, &NodeEdit::AppendPin(pin())));
        assert_eq!((exit, message.as_str()), (2, LAYOUT), "layout: {layout}");
        let (exit, message) = refusal(plan_edit(&snap, &NodeEdit::Prepare(node_cfg())));
        assert_eq!((exit, message.as_str()), (2, LAYOUT), "layout: {layout}");
    }

    // Overlapping or out-of-range edits are refused.
    let original = b"abcdef";
    let overlap = [
        TextEdit {
            range: 1..4,
            replacement: "X".into(),
        },
        TextEdit {
            range: 3..5,
            replacement: "Y".into(),
        },
    ];
    assert_eq!(
        refusal(apply_edits(original, &overlap)),
        (2, GUARD.to_string())
    );
    let range = [TextEdit {
        range: 4..9,
        replacement: "Z".into(),
    }];
    assert_eq!(
        refusal(apply_edits(original, &range)),
        (2, GUARD.to_string())
    );
}

fn write_config(dir: &TempDir, bytes: &str) -> PathBuf {
    let path = dir.path().join("config.toml");
    std::fs::write(&path, bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    path
}

#[test]
fn node_config_refuses_concurrent_change_and_partial_publish() {
    let original = format!("[channel.node]\nkey_fingerprint = \"{}\"\n", hex64("ab"));

    // A held lock refuses a second writer; releasing it allows the next.
    let dir = TempDir::new().unwrap();
    let path = write_config(&dir, &original);
    let first = lock_config(read_config(&path, false).unwrap()).unwrap();
    let (exit, message) = refusal(lock_config(read_config(&path, false).unwrap()));
    assert_eq!((exit, message.as_str()), (6, BUSY));
    drop(first);
    drop(lock_config(read_config(&path, false).unwrap()).unwrap());

    // An edit by another writer before rename is detected; its bytes survive.
    let snap = read_config(&path, false).unwrap();
    let edits = plan_edit(&snap, &NodeEdit::RecoverFingerprint(hex64("cd"))).unwrap();
    let new_bytes = apply_edits(&snap.bytes, &edits).unwrap();
    let mut guard = lock_config(snap).unwrap();
    let external = format!("# edited elsewhere\n{original}");
    std::fs::write(&path, &external).unwrap();
    let (exit, message) = refusal(publish_config(&mut guard, &new_bytes));
    assert_eq!((exit, message.as_str()), (6, CHANGED));
    assert_eq!(std::fs::read(&path).unwrap(), external.as_bytes());
    drop(guard);

    // Successful publish: exact new bytes, original mode, no staging leftovers.
    let dir = TempDir::new().unwrap();
    let path = write_config(&dir, &original);
    let snap = read_config(&path, false).unwrap();
    let edits = plan_edit(&snap, &NodeEdit::RecoverFingerprint(hex64("cd"))).unwrap();
    let new_bytes = apply_edits(&snap.bytes, &edits).unwrap();
    let mut guard = lock_config(snap).unwrap();
    publish_config(&mut guard, &new_bytes).unwrap();
    drop(guard);
    assert_eq!(std::fs::read(&path).unwrap(), new_bytes);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        original.replace(&hex64("ab"), &hex64("cd")).as_bytes()
    );
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o640);
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "staging files left: {leftovers:?}");

    // Two publishes under one guard: the second compares against the bytes the
    // first one wrote, and a write by another process after that is refused.
    let dir = TempDir::new().unwrap();
    let path = write_config(&dir, &original);
    let snap = read_config(&path, false).unwrap();
    let edits = plan_edit(&snap, &NodeEdit::RecoverFingerprint(hex64("cd"))).unwrap();
    let first = apply_edits(&snap.bytes, &edits).unwrap();
    let mut guard = lock_config(snap).unwrap();
    publish_config(&mut guard, &first).unwrap();
    let second = format!("{}# second\n", String::from_utf8(first.clone()).unwrap());
    publish_config(&mut guard, second.as_bytes()).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), second.as_bytes());
    let external = format!("# edited elsewhere\n{second}");
    std::fs::write(&path, &external).unwrap();
    let (exit, message) = refusal(publish_config(&mut guard, &first));
    assert_eq!((exit, message.as_str()), (6, CHANGED));
    assert_eq!(std::fs::read(&path).unwrap(), external.as_bytes());
    drop(guard);

    // A target the guard created is tracked the same way.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("new.toml");
    let snap = read_config(&path, true).unwrap();
    let mut guard = lock_config(snap).unwrap();
    publish_config(&mut guard, b"[channel.node]\n").unwrap();
    publish_config(&mut guard, b"[channel.node]\nenabled = false\n").unwrap();
    drop(guard);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"[channel.node]\nenabled = false\n"
    );

    // New target: create-only publication never replaces a file that appeared.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("new.toml");
    let snap = read_config(&path, true).unwrap();
    let mut guard = lock_config(snap).unwrap();
    std::fs::write(&path, b"appeared meanwhile\n").unwrap();
    let (exit, message) = refusal(publish_config(&mut guard, b"[channel.node]\n"));
    assert_eq!((exit, message.as_str()), (6, CHANGED));
    assert_eq!(std::fs::read(&path).unwrap(), b"appeared meanwhile\n");
    drop(guard);

    // Unsafe targets: symlink and hard link are refused with the exact text.
    let dir = TempDir::new().unwrap();
    let real = write_config(&dir, &original);
    let link = dir.path().join("link.toml");
    symlink(&real, &link).unwrap();
    let (exit, message) = refusal(read_config(&link, false));
    assert_eq!((exit, message.as_str()), (2, UNSAFE));
    let hard = dir.path().join("hard.toml");
    std::fs::hard_link(&real, &hard).unwrap();
    let (exit, message) = refusal(read_config(&real, false));
    assert_eq!((exit, message.as_str()), (2, UNSAFE));

    // Missing parent directory.
    let (exit, message) = refusal(read_config(&dir.path().join("nope/c.toml"), true));
    assert_eq!(
        (exit, message.as_str()),
        (2, "the configuration directory does not exist")
    );

    // Public output is create-only: an existing file is never overwritten.
    let out = dir.path().join("bundle.json");
    save_public_file(&out, b"first").unwrap();
    let (exit, message) = refusal(save_public_file(&out, b"second"));
    assert_eq!(
        (exit, message.as_str()),
        (6, "the output file already exists")
    );
    assert_eq!(std::fs::read(&out).unwrap(), b"first");
}

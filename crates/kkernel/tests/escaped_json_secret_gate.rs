//! Black-box: JSON text holding a credential with one escaped character is
//! refused by every write verb that takes free text, like its plain form.

use std::process::{Command, Output};

use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use khive_storage::Entity;
use tempfile::TempDir;

const PACKS: &str = "kg,memory,gtd,comm";

/// Every loaded pack on the scratch database with no embedding model.
fn no_embed_config(home: &TempDir) -> std::path::PathBuf {
    let config = home.path().join("khive.toml");
    let sections: String = PACKS
        .split(',')
        .map(|pack| format!("[packs.{pack}]\nbackend = \"main\"\nno_embed = true\n"))
        .collect();
    std::fs::write(&config, sections).expect("write config");
    config
}

fn exec(home: &TempDir, db: &str, ops: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kkernel"))
        .arg("exec")
        .arg(ops)
        .args(["--db", db, "--actor", "lambda:test"])
        .arg("--config")
        .arg(no_embed_config(home))
        .current_dir(home.path())
        .env("HOME", home.path())
        .env("KHIVE_VOLUME_LOCK_DIR", home.path().join("volume-locks"))
        .env("KHIVE_NO_DAEMON", "1")
        .env("KHIVE_PACKS", PACKS)
        .env("RUST_LOG", "error")
        .env_remove("KHIVE_ACTOR")
        .env_remove("KHIVE_ADDITIONAL_EMBEDDING_MODELS")
        .env_remove("KHIVE_CONFIG")
        .env_remove("KHIVE_DAEMON_STRICT")
        .env_remove("KHIVE_DB")
        .env_remove("KHIVE_EMBEDDING_MODEL")
        .env_remove("KHIVE_OUTPUT_FORMAT")
        .env_remove("KHIVE_REQUIRE_ATTRIBUTED_ACTOR")
        .output()
        .expect("run kkernel exec")
}

fn response(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout was not JSON: {error}; stdout={:?}; stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// (name, credential as written in JSON text: plain, then with one escape)
fn credentials() -> Vec<(&'static str, String, String)> {
    let aws = ["AK", "IA", "QWERTYUIOPASDFGH"].concat();
    let github = ["gh", "p_", "aB3dE5gH7jK9mN1pQ3sT5vW7yZ9bC1dF3hJ5"].concat();
    let url = ["https://user:", "Zx9", "Qw7Er5Ty3Ui1", "@example.com/x"].concat();
    vec![
        ("aws", aws.clone(), format!("\\u0041{}", &aws[1..])),
        ("github", github.clone(), format!("\\u0067{}", &github[1..])),
        ("url", url.clone(), url.replace('/', "\\/")),
    ]
}

/// Each write verb with its free-text field set to `text`.
fn write_ops(text: &str, entity_id: &str) -> Vec<(&'static str, serde_json::Value)> {
    use serde_json::json;
    let sentinel = "00000000-0000-0000-0000-000000000000";
    vec![
        (
            "create",
            json!({"tool": "create", "args": {"kind": "note", "name": "n", "content": text}}),
        ),
        (
            "update",
            json!({"tool": "update", "args": {"id": entity_id, "description": text}}),
        ),
        (
            "memory.remember",
            json!({"tool": "memory.remember", "args": {"content": text}}),
        ),
        (
            "gtd.assign",
            json!({"tool": "gtd.assign", "args": {"title": "t", "description": text}}),
        ),
        (
            "gtd.transition",
            json!({"tool": "gtd.transition", "args": {"id": sentinel, "status": "active", "note": text}}),
        ),
        (
            "comm.send",
            json!({"tool": "comm.send", "args": {"to": "lambda:peer", "content": text}}),
        ),
    ]
}

/// The `update` target, written by a runtime with no embedding model so the
/// test needs no model files (a concept write through `exec` embeds).
async fn seed_entity(home: &TempDir, db: &std::path::Path) -> String {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(db.to_path_buf()),
        volume_lock_dir: Some(home.path().join("volume-locks")),
        packs: vec!["kg".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("model-less seed runtime");
    let token = runtime.authorize(Namespace::local()).unwrap();
    let entity = Entity::new("local", "concept", "EscapedJsonTarget");
    let id = entity.id.to_string();
    runtime
        .entities(&token)
        .unwrap()
        .upsert_entity(entity)
        .await
        .unwrap();
    id
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn escaped_credentials_in_json_text_are_refused_by_every_write_verb() {
    let home = TempDir::new().unwrap();
    let db_path = home.path().join("escaped.sqlite");
    let entity_id = seed_entity(&home, &db_path).await;
    let db = db_path.to_string_lossy().into_owned();

    let mut rows = Vec::new();
    for (name, plain, escaped) in credentials() {
        for (form, credential) in [("plain", &plain), ("escaped", &escaped)] {
            let text = format!("{{\"v\": \"{credential}\"}}");
            for (verb, op) in write_ops(&text, &entity_id) {
                let ops = serde_json::Value::Array(vec![op]).to_string();
                let reply = response(&exec(&home, &db, &ops));
                let code = reply["results"][0]["error"]["code"].clone();
                rows.push(format!("{name}/{form}/{verb}: {code}"));
            }
        }
    }
    let refused = |row: &String| row.ends_with(": \"secret_detected\"");
    assert!(rows.iter().all(refused), "{rows:#?}");
}

//! A daemonless exec child must settle its admitted recall telemetry before exit.
//!
//! This checks the successful path using a scratch SQLite database. It does not
//! guarantee admission, storage success, or serve-ledger durability in general.
#![cfg(unix)]

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::process::Command;

// The CLI actor is a raw ID; event filters use the caller's stamped label.
const ACTOR: &str = "actor:recall-drain-fixture";
const STAMPED_ACTOR: &str = "actor:actor:recall-drain-fixture";
const SINCE: &str = "2000-01-01T00:00:00Z";
const QUERY: &str = "violet saffron atlas recall canary";
const WATCHDOG: Duration = Duration::from_secs(60);

struct Fixture {
    _root: TempDir,
    root: PathBuf,
    config: PathBuf,
    db: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("exec-recall-drain-")
            .tempdir_in("/tmp")
            .expect("isolated scratch directory");
        let path = root.path().to_path_buf();
        std::fs::create_dir_all(path.join("home")).unwrap();
        std::fs::create_dir_all(path.join("tmp")).unwrap();
        let config = path.join("khive.toml");
        // No [[backends]]: --db supplies the single writable scratch backend.
        // This follows the no-backends exec fixture and enables no-embed recall.
        std::fs::write(
            &config,
            "[runtime]\npacks = [\"kg\", \"memory\", \"brain\"]\n\
             [display]\ntimezone = \"UTC\"\n\
             [packs.kg]\nbackend = \"main\"\nno_embed = true\n\
             [packs.memory]\nbackend = \"main\"\nno_embed = true\n\
             [[credentials]]\nname = \"test-receipt\"\nkind = \"signing_key\"\n\
             provider = \"env\"\nenv_var = \"KHIVE_TEST_RECEIPT_KEY\"\n\
             [[visibility_receipts.keys]]\nid = \"test-receipt-key\"\n\
             credential = \"test-receipt\"\nencrypt = true\n",
        )
        .unwrap();
        Self {
            _root: root,
            root: path.clone(),
            config,
            db: path.join("scratch.db"),
        }
    }

    async fn exec(&self, tool: &str, args: Value) -> Value {
        let ops = json!([{ "tool": tool, "args": args }]).to_string();
        let mut command = Command::new(env!("CARGO_BIN_EXE_kkernel"));
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.root.join("home"))
            .env("USERPROFILE", self.root.join("home"))
            .env("TMPDIR", self.root.join("tmp"))
            .env("TZ", "UTC")
            .env("RUST_LOG", "error")
            .env("KHIVE_NO_DAEMON", "1")
            .env("KHIVE_PACKS", "kg,memory,brain")
            .env("KHIVE_EVENTS_SPLIT", "0")
            .env("KHIVE_TEST_RECEIPT_KEY", "A".repeat(43))
            .env("KHIVE_VOLUME_LOCK_DIR", self.root.join("volume-locks"))
            .env("KHIVE_SOCKET", self.root.join("s"))
            .env("KHIVE_PID", self.root.join("p"))
            .env("KHIVE_LOCK", self.root.join("l"))
            .current_dir(&self.root)
            .args([
                "exec",
                &ops,
                "--actor",
                ACTOR,
                "--strict",
                "--presentation",
                "verbose",
                "--output-format",
                "json",
                "--config",
            ])
            .arg(&self.config)
            .arg("--db")
            .arg(&self.db)
            .kill_on_drop(true);
        let output = tokio::time::timeout(WATCHDOG, command.output())
            .await
            .expect("exec timed out")
            .expect("spawn isolated exec");
        assert!(
            output.status.success(),
            "{tool} failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let envelope: Value = serde_json::from_slice(&output.stdout).expect("JSON exec envelope");
        assert_eq!(envelope["summary"]["failed"], 0, "{tool}: {envelope}");
        let results = envelope["results"].as_array().expect("operation receipts");
        assert_eq!(results.len(), 1, "one operation: {envelope}");
        assert_eq!(results[0]["ok"], true, "{tool}: {envelope}");
        results[0]["result"].clone()
    }

    async fn recall_events(&self) -> HashMap<String, Value> {
        let mut events_by_id = HashMap::new();
        let mut after: Option<String> = None;
        let mut cursors = HashSet::new();
        for _ in 0..100 {
            let mut args = json!({
                "since": SINCE, "kind": "recall_executed", "actor": STAMPED_ACTOR, "limit": 1
            });
            // The cursor retains the first page's frozen until bound.
            if let Some(cursor) = after.as_ref() {
                args["after"] = json!(cursor);
            }
            let page = self.exec("brain.event_page", args).await;
            let events = page["events"].as_array().expect("event_page events array");
            assert_eq!(page["count"].as_u64(), Some(events.len() as u64));
            for event in events {
                let id = event["id"].as_str().expect("event id").to_owned();
                assert!(
                    events_by_id.insert(id, event.clone()).is_none(),
                    "duplicate event"
                );
            }
            if !page["has_more"].as_bool().expect("has_more") {
                assert!(page["next_after"].is_null(), "last page has no cursor");
                return events_by_id;
            }
            let next = page["next_after"]
                .as_str()
                .expect("continuation when has_more")
                .to_owned();
            assert!(cursors.insert(next.clone()), "repeated continuation");
            after = Some(next);
        }
        panic!("recall event page limit exceeded");
    }
}

#[tokio::test]
async fn exec_recall_event_is_stored_before_child_exit() {
    let fixture = Fixture::new();
    // Each invocation is a fresh process. The process-local ledger gate starts
    // with its full available capacity (load_limits.rs:328-334, 379-382,
    // 390-392 at 014e2ff0); this is a source premise, not a parent snapshot.
    let identity = fixture.exec("whoami", json!({})).await;
    assert_eq!(identity["actor_id"], ACTOR);
    assert_eq!(identity["actor_kind"], "actor");
    assert_eq!(identity["unattributed"], false);

    let seed = fixture
        .exec(
            "memory.remember",
            json!({"content": QUERY, "memory_type": "semantic", "salience": 0.8}),
        )
        .await;
    let seed_id = seed["id"].as_str().expect("remembered ID");
    let before = fixture.recall_events().await;
    let recall = fixture.exec("memory.recall", json!({"query": QUERY})).await;
    let hits = recall
        .as_array()
        .or_else(|| recall["results"].as_array())
        .expect("recall hits");
    assert!(
        hits.iter().any(|hit| hit["id"] == seed_id),
        "recall must return the seeded memory: {recall}"
    );

    // The exec process has exited before this separate child pages the store.
    let after = fixture.recall_events().await;
    let new_events: Vec<_> = after
        .iter()
        .filter(|(id, _)| !before.contains_key(*id))
        .map(|(_, event)| event)
        .collect();
    assert_eq!(new_events.len(), 1, "one new actor-scoped recall event");
    let event = new_events[0];
    assert_eq!(event["actor"], STAMPED_ACTOR);
    assert_eq!(event["verb"], "memory.recall");
    assert_eq!(event["payload"]["query"], QUERY);
    assert!(
        event["payload"]["selected"]
            .as_array()
            .expect("selected IDs")
            .iter()
            .any(|id| id == seed_id),
        "event must describe the memory returned by the child: {event}"
    );
}

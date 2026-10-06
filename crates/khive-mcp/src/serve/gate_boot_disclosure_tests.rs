use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use khive_runtime::{ActorRef, GateRequest, Namespace, RuntimeConfig};
use tracing::field::{Field, Visit};

use super::super::{resolve_runtime_config, RuntimeConfigInputs};

#[derive(Debug, Default)]
struct CapturedEvent {
    fields: BTreeMap<String, String>,
}

impl Visit for CapturedEvent {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

struct CaptureSubscriber(Arc<Mutex<Vec<CapturedEvent>>>);

impl tracing::Subscriber for CaptureSubscriber {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        if event.metadata().target() != "khive.boot" {
            return;
        }
        let mut captured = CapturedEvent::default();
        event.record(&mut captured);
        if captured.fields.get("message").map(String::as_str)
            == Some("gate configuration selection resolved")
        {
            assert_eq!(*event.metadata().level(), tracing::Level::INFO);
            self.0.lock().unwrap().push(captured);
        }
    }

    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

fn resolve_and_capture(
    config: Option<&Path>,
    db: &str,
    no_embed: bool,
    actor_explicit: bool,
) -> (RuntimeConfig, CapturedEvent) {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let subscriber = CaptureSubscriber(Arc::clone(&captured));
    let resolved = tracing::subscriber::with_default(subscriber, || {
        resolve_runtime_config(RuntimeConfigInputs {
            db: Some(db),
            config,
            namespace: Namespace::local(),
            namespace_explicit: actor_explicit,
            actor_explicit,
            no_embed,
            packs: Some(vec!["kg".to_string()]),
            brain_profile: None,
        })
        .expect("resolve the actual runtime configuration")
    });
    let mut events = captured.lock().unwrap();
    assert_eq!(
        events.len(),
        1,
        "exactly one intended resolver event must execute: {events:?}"
    );
    (resolved, events.pop().unwrap())
}

fn assert_count(event: &CapturedEvent, configured: bool, attributed: usize, anonymous: bool) {
    assert_eq!(
        event.fields.keys().map(String::as_str).collect::<Vec<_>>(),
        vec![
            "config_source",
            "configured_attributed_principal_count",
            "configured_principal_count",
            "grant_unattributed",
            "message",
            "roster_configured",
        ],
        "only source and aggregate roster facts are disclosed"
    );
    for (field, expected) in [
        ("roster_configured", configured.to_string()),
        (
            "configured_attributed_principal_count",
            attributed.to_string(),
        ),
        ("grant_unattributed", anonymous.to_string()),
        (
            "configured_principal_count",
            (attributed + usize::from(anonymous)).to_string(),
        ),
    ] {
        assert_eq!(
            event.fields.get(field),
            Some(&expected),
            "field {field}: {event:?}"
        );
    }
}

fn admitted(config: &RuntimeConfig, actor: ActorRef) -> bool {
    config
        .gate
        .check(&GateRequest::new(
            actor,
            Namespace::local(),
            "get",
            serde_json::json!({}),
        ))
        .expect("valid enrollment policy evaluates")
        .is_allow()
}

fn write_config(root: &Path, body: &str) -> PathBuf {
    let path = root.join("selected-config.toml");
    std::fs::write(&path, body).unwrap();
    path
}

#[test]
fn gate_config_disclosure_matches_enrollment_in_both_resolver_paths() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        dir.path(),
        r#"
[actor]
id = "actor:boot-unlisted"
[gate]
granted_actors = ["actor:duty", "actor:duty", "actor:Duty", "local"]
grant_unattributed = true
deny_writes_for = ["*:duty", "*:duty"]
[runtime]
brain_profile = "diagnostic-content-must-not-leak"
"#,
    );
    for no_embed in [false, true] {
        let (resolved, event) = resolve_and_capture(Some(&path), ":memory:", no_embed, true);
        assert_count(&event, true, 3, true);
        assert_eq!(
            event.fields["config_source"],
            format!("{:?}", Some(std::fs::canonicalize(&path).unwrap()))
        );
        for id in ["actor:duty", "actor:Duty", "local"] {
            assert!(
                admitted(&resolved, ActorRef::new("actor", id)),
                "configured read principal {id}"
            );
        }
        assert!(admitted(&resolved, ActorRef::anonymous()));
        assert!(!admitted(
            &resolved,
            ActorRef::new("actor", "actor:boot-unlisted")
        ));
        assert!(!admitted(
            &resolved,
            ActorRef::new("actor", "actor:unlisted")
        ));
        for marker in [
            "actor:duty",
            "actor:Duty",
            "actor:boot-unlisted",
            "*:duty",
            "diagnostic-content-must-not-leak",
        ] {
            assert!(
                !event
                    .fields
                    .iter()
                    .filter(|(name, _)| name.as_str() != "config_source")
                    .any(|(_, value)| value.contains(marker)),
                "no config contents: {event:?}"
            );
        }
    }
}

#[test]
fn gate_config_disclosure_distinguishes_absent_empty_and_no_file() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    for (body, configured, admits) in [("", false, true), ("[gate]\n", true, false)] {
        let path = write_config(dir.path(), body);
        for no_embed in [false, true] {
            let (resolved, event) = resolve_and_capture(Some(&path), ":memory:", no_embed, true);
            assert_count(&event, configured, 0, false);
            assert_eq!(
                event.fields["config_source"],
                format!("{:?}", Some(std::fs::canonicalize(&path).unwrap()))
            );
            assert_eq!(
                admitted(&resolved, ActorRef::new("actor", "actor:unlisted")),
                admits
            );
            assert_eq!(admitted(&resolved, ActorRef::anonymous()), admits);
        }
    }
    for no_embed in [false, true] {
        let (resolved, event) = resolve_and_capture(None, ":memory:", no_embed, true);
        assert_count(&event, false, 0, false);
        assert_eq!(event.fields["config_source"], "None");
        assert!(admitted(
            &resolved,
            ActorRef::new("actor", "actor:unlisted")
        ));
    }
}

#[test]
fn gate_config_disclosure_counts_anonymous_separately_from_attributed_local() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    for anonymous in [false, true] {
        let path = write_config(
            dir.path(),
            &format!("[gate]\ngranted_actors=['local','local']\ngrant_unattributed={anonymous}\n"),
        );
        for no_embed in [false, true] {
            let (resolved, event) = resolve_and_capture(Some(&path), ":memory:", no_embed, true);
            assert_count(&event, true, 1, anonymous);
            assert!(admitted(&resolved, ActorRef::new("actor", "local")));
            assert_eq!(admitted(&resolved, ActorRef::anonymous()), anonymous);
        }
    }
}

struct RestoreCwd(PathBuf);
impl Drop for RestoreCwd {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.0).unwrap();
    }
}

#[test]
fn gate_config_disclosure_db_source_is_shared_while_cwd_actor_remains_local() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    std::env::remove_var("KHIVE_ACTOR");
    let store = tempfile::tempdir().unwrap();
    let hidden = store.path().join(".khive");
    std::fs::create_dir(&hidden).unwrap();
    let database = hidden.join("shared.db");
    std::fs::write(&database, b"").unwrap();
    let config_path = hidden.join("config.toml");
    std::fs::write(
        &config_path,
        "[gate]\ngranted_actors=['actor:a','actor:b']\n",
    )
    .unwrap();
    let project_a = tempfile::tempdir().unwrap();
    let project_b = tempfile::tempdir().unwrap();
    let _restore = RestoreCwd(std::env::current_dir().unwrap());
    for (project, actor) in [(&project_a, "actor:a"), (&project_b, "actor:b")] {
        std::fs::create_dir(project.path().join(".khive")).unwrap();
        std::fs::write(
            project.path().join(".khive/config.toml"),
            format!("[actor]\nid='{actor}'\n"),
        )
        .unwrap();
        std::env::set_current_dir(project.path()).unwrap();
        for no_embed in [false, true] {
            let (resolved, event) =
                resolve_and_capture(None, database.to_str().unwrap(), no_embed, false);
            assert_count(&event, true, 2, false);
            assert_eq!(
                event.fields["config_source"],
                format!("{:?}", Some(std::fs::canonicalize(&config_path).unwrap()))
            );
            assert_eq!(resolved.actor_id.as_deref(), Some(actor));
            assert!(admitted(&resolved, ActorRef::new("actor", actor)));
            assert!(!admitted(
                &resolved,
                ActorRef::new("actor", "actor:unlisted")
            ));
        }
    }
}

#[cfg(unix)]
#[test]
fn gate_config_disclosure_escapes_real_control_characters_once() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config\n\u{1b}.toml");
    std::fs::write(&path, "[gate]\ngranted_actors=['actor:only']\n").unwrap();
    let selected = std::fs::canonicalize(&path).unwrap();
    assert!(
        selected.to_string_lossy().contains('\n') && selected.to_string_lossy().contains('\u{1b}')
    );
    for no_embed in [false, true] {
        let (_, event) = resolve_and_capture(Some(&path), ":memory:", no_embed, true);
        let rendered = &event.fields["config_source"];
        assert_eq!(rendered, &format!("{:?}", Some(&selected)));
        assert!(!rendered.contains('\n') && !rendered.contains('\u{1b}'));
        assert!(rendered.contains("\\n") && rendered.contains("\\u{1b}"));
        assert!(!rendered.contains("\\\\n") && !rendered.contains("\\\\u{1b}"));
    }
}

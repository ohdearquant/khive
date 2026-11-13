use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use clap::Parser;
use khive_runtime::{KhiveConfig, Namespace, RuntimeConfig};

use super::{build_server, resolve_runtime_config, Args, RuntimeConfigInputs};

const WARNING: &str =
    "legacy [embed] settings are ignored; [[engines]] controls embedding providers";

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Capture {
    fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync {
        let writer = self.clone();
        tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish()
    }

    fn warnings(&self) -> Vec<String> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .filter(|line| line.contains(WARNING))
            .map(str::to_owned)
            .collect()
    }
}

fn isolated() -> bool {
    if crate::test_isolation::rerun_with_private_home() {
        return true;
    }
    for name in [
        "KHIVE_EMBEDDING_MODEL",
        "KHIVE_ADDITIONAL_EMBEDDING_MODELS",
        "KHIVE_NO_EMBED",
        "KHIVE_CONFIG",
        "KHIVE_PACKS",
        "KHIVE_BRAIN_PROFILE",
        "KHIVE_ACTOR",
        "KHIVE_REQUIRE_ATTRIBUTED_ACTOR",
        "KHIVE_EVENTS_DB",
        "KHIVE_EVENTS_SOCKET",
    ] {
        std::env::remove_var(name);
    }
    false
}

fn resolve(path: &Path, no_embed: bool) -> RuntimeConfig {
    resolve_selected(Some(path), no_embed)
}

fn resolve_selected(path: Option<&Path>, no_embed: bool) -> RuntimeConfig {
    resolve_runtime_config(RuntimeConfigInputs {
        db: Some(":memory:"),
        config: path,
        namespace: Namespace::local(),
        namespace_explicit: true,
        actor_explicit: true,
        no_embed,
        packs: Some(vec!["kg".to_owned()]),
        brain_profile: None,
    })
    .expect("real configuration resolution without provider binding")
}

#[test]
fn legacy_embed_warning_is_boot_scoped() {
    if isolated() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("selected.toml");
    std::fs::write(
        &path,
        "[embed]\nauto_embed = false\nmodel = 'legacy-private-name'\ndimensions = 768\ndevice = 'cpu'\n[[engines]]\nname = 'unregistered-peer'\ndims = 384\n",
    )
    .unwrap();
    for no_embed in [false, true] {
        let capture = Capture::default();
        tracing::subscriber::with_default(capture.subscriber(), || {
            for _ in 0..3 {
                KhiveConfig::load(Some(&path)).unwrap().unwrap();
            }
            assert!(capture.warnings().is_empty(), "raw loads are not boots");
            for expected in 1..=2 {
                let resolved = resolve(&path, no_embed);
                let engines = resolved.configured_engines();
                if no_embed {
                    assert!(engines.is_empty());
                    assert_eq!(resolved.engines, Some(Vec::new()));
                    assert!(resolved.embedding_model.is_none());
                    assert!(resolved.additional_embedding_models.is_empty());
                } else {
                    assert_eq!(engines.len(), 1);
                    assert_eq!(engines[0].name, "unregistered-peer");
                    assert_eq!(engines[0].dims, Some(384));
                }
                KhiveConfig::load(Some(&path)).unwrap().unwrap();
                let warnings = capture.warnings();
                assert_eq!(warnings.len(), expected, "{warnings:?}");
                let warning = warnings.last().unwrap();
                for key in [
                    "embed.model",
                    "embed.dimensions",
                    "embed.device",
                    "[[engines]]",
                ] {
                    assert!(warning.contains(key), "{warning}");
                }
                assert!(!warning.contains("legacy-private-name"), "{warning}");
                assert!(!warning.contains("unregistered-peer"), "{warning}");
            }
        });
    }
}

#[test]
fn legacy_comparison_uses_declared_dimensions_and_exact_default_name() {
    if isolated() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("selected.toml");
    let cases = [
        ("[embed]\nmodel = 'custom-peer'\ndimensions = 384\n[[engines]]\nname = 'custom-peer'\ndims = 384\n", vec![]),
        ("[embed]\nmodel = 'custom-peer'\ndimensions = 5000000000\n[[engines]]\nname = 'custom-peer'\n", vec![]),
        ("[embed]\nmodel = 'default'\n[[engines]]\nname = 'custom-peer'\n", vec!["embed.model"]),
        ("[embed]\nmodel = 'default'\n[[engines]]\nname = 'default'\n", vec![]),
        ("engines = []\n[embed]\nmodel = 'custom-peer'\ndimensions = 384\n", vec!["embed.model"]),
        ("[embed]\ndevice = 'cpu'\n[[engines]]\nname = 'custom-peer'\n", vec!["embed.device"]),
        ("[embed]\n[[engines]]\nname = 'custom-peer'\n", vec![]),
    ];
    for (raw, expected) in cases {
        std::fs::write(&path, raw).unwrap();
        let mut reference_reasons = None;
        for no_embed in [false, true] {
            let capture = Capture::default();
            let resolved = tracing::subscriber::with_default(capture.subscriber(), || {
                resolve(&path, no_embed)
            });
            if no_embed || raw.starts_with("engines = []") {
                assert_eq!(resolved.engines, Some(Vec::new()));
                assert!(resolved.configured_engines().is_empty());
            }
            let warnings = capture.warnings();
            assert_eq!(
                warnings.len(),
                usize::from(!expected.is_empty()),
                "{raw}: {warnings:?}"
            );
            for key in ["embed.model", "embed.dimensions", "embed.device"] {
                assert_eq!(
                    warnings.iter().any(|warning| warning.contains(key)),
                    expected.contains(&key),
                    "{raw}: {warnings:?}"
                );
            }
            if let Some(reference) = &reference_reasons {
                assert_eq!(
                    &warnings, reference,
                    "suppression must not change comparison"
                );
            } else {
                reference_reasons = Some(warnings);
            }
        }
    }
}

#[test]
fn no_embed_implicit_peer_comparison_preserves_returned_model_shape() {
    if isolated() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("selected.toml");
    for model in [None, Some("bge-small-en-v1.5")] {
        match model {
            Some(model) => std::env::set_var("KHIVE_EMBEDDING_MODEL", model),
            None => std::env::remove_var("KHIVE_EMBEDDING_MODEL"),
        }
        std::env::set_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS", "all-minilm");
        let implicit = RuntimeConfig::default().configured_engines();
        let primary = implicit.first().unwrap();
        std::fs::write(
            &path,
            format!(
                "[embed]\nauto_embed = false\nmodel = {:?}\ndimensions = 5000000000\n",
                primary.name
            ),
        )
        .unwrap();
        let capture = Capture::default();
        tracing::subscriber::with_default(capture.subscriber(), || {
            let disabled = resolve(&path, true);
            assert_eq!(disabled.engines, Some(Vec::new()));
            assert!(disabled.embedding_model.is_none());
            assert!(disabled.additional_embedding_models.is_empty());
            let enabled = resolve(&path, false);
            assert_eq!(enabled.engines, None);
            assert_eq!(enabled.configured_engines(), implicit);
            assert!(
                capture.warnings().is_empty(),
                "an implicit peer has no declared dimensions"
            );
            std::fs::remove_file(&path).unwrap();
            let absent = resolve_selected(None, true);
            assert_eq!(absent.engines, None);
            assert!(absent.embedding_model.is_none());
            assert!(absent.additional_embedding_models.is_empty());
            assert!(capture.warnings().is_empty());
        });
    }
    std::env::set_var("KHIVE_EMBEDDING_MODEL", "all-minilm");
    std::fs::write(
        &path,
        "[embed]\nmodel = 'explicit-peer'\n[[engines]]\nname = 'explicit-peer'\n",
    )
    .unwrap();
    let capture = Capture::default();
    tracing::subscriber::with_default(capture.subscriber(), || resolve(&path, true));
    assert!(
        capture.warnings().is_empty(),
        "declared engines override implicit peers"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn actual_server_boot_reloads_config_without_multiplying_legacy_warning() {
    if isolated() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("selected.toml");
    std::fs::write(&path, "[embed]\nauto_embed = false\nmodel = 'unregistered-peer'\ndimensions = 5000000000\ndevice = 'cuda'\n[[engines]]\nname = 'unregistered-peer'\n").unwrap();
    let args = Args::parse_from([
        "mcp",
        "--db",
        ":memory:",
        "--config",
        path.to_str().unwrap(),
        "--no-embed",
        "--pack",
        "kg",
    ]);
    let capture = Capture::default();
    // The current-thread runtime keeps this scoped subscriber on every poll of
    // the boot future; no process-global subscriber or cross-test capture.
    let _subscriber = tracing::subscriber::set_default(capture.subscriber());
    for expected in 1..=2 {
        let (server, schedule_runtime) = build_server(&args)
            .await
            .expect("no-embed boot must not resolve an unregistered peer to infer dimensions");
        assert!(schedule_runtime.is_none());
        KhiveConfig::load(Some(&path)).unwrap().unwrap();
        let warnings = capture.warnings();
        assert_eq!(warnings.len(), expected, "{warnings:?}");
        assert!(warnings.last().unwrap().contains("embed.device"));
        assert!(!warnings.last().unwrap().contains("embed.model"));
        assert!(!warnings.last().unwrap().contains("embed.dimensions"));
        server
            .shutdown_audit_batch()
            .await
            .expect("drain this boot's audit worker");
        drop(server);
    }
}

//! Git, Code, agent, telemetry and web are optional features of `kkernel` and
//! `khive-mcp`: a build links and registers a pack exactly when its feature is on.
//!
//! Registry assertions exercise the features compiled into this test binary.
//! Manifest assertions keep Git and Code default-on, the other three packs default-off,
//! and Kernel-to-MCP forwarding explicit. The operator build enables the three
//! default-off packs without switching on MCP defaults through a dev dependency.

use std::collections::BTreeSet;

use khive_runtime::pack::PackRegistry;
use khive_runtime::RuntimeConfig;
// Keep kkernel's production force-link anchors, including feature-gated packs.
use kkernel as _;

const KKERNEL_MANIFEST: &str = include_str!("../Cargo.toml");
const KHIVE_MCP_MANIFEST: &str = include_str!("../../khive-mcp/Cargo.toml");
const MAKEFILE: &str = include_str!("../../../Makefile");
const DEFAULT_OFF_PACKS: [&str; 3] = ["agent", "telemetry", "web"];

/// The optional packs, each paired with whether this build links it.
const OPTIONAL_PACKS: [(&str, bool); 5] = [
    ("code", cfg!(feature = "pack-code")),
    ("git", cfg!(feature = "pack-git")),
    ("agent", cfg!(feature = "pack-agent")),
    ("telemetry", cfg!(feature = "pack-telemetry")),
    ("web", cfg!(feature = "pack-web")),
];

fn discovered() -> BTreeSet<String> {
    PackRegistry::discovered_names()
        .into_iter()
        .map(str::to_owned)
        .collect()
}

fn manifest(text: &str) -> toml::Table {
    toml::from_str(text).expect("the manifest is valid TOML")
}

fn feature(manifest: &toml::Table, name: &str) -> Vec<String> {
    manifest["features"][name]
        .as_array()
        .unwrap_or_else(|| panic!("feature {name} is a list"))
        .iter()
        .map(|entry| entry.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn discovered_packs_include_the_default_and_explicitly_selected_packs() {
    let mut expected: BTreeSet<String> = RuntimeConfig::built_in_packs().into_iter().collect();
    assert!(
        !expected.contains("charter"),
        "the schema-only pack is opt-in"
    );
    expected.insert("charter".to_string());
    for (name, linked) in OPTIONAL_PACKS {
        if linked {
            expected.insert(name.to_string());
        }
    }
    if cfg!(feature = "pack-formal") {
        expected.insert("formal".to_string());
    }
    if cfg!(feature = "pack-moodboard") {
        expected.insert("moodboard".to_string());
    }
    assert_eq!(
        discovered(),
        expected,
        "the linked packs must include charter and the packs whose feature is on"
    );
}

#[test]
fn a_pack_whose_feature_is_off_is_neither_discovered_nor_selectable() {
    let discovered = discovered();
    for (name, linked) in OPTIONAL_PACKS {
        let selection = vec!["kg".to_string(), name.to_string()];
        let result = PackRegistry::validate_pack_selection(&selection);
        if linked {
            assert!(
                discovered.contains(name),
                "{name} is linked but not discovered"
            );
            assert!(
                result.is_ok(),
                "{name} is linked but selecting it was refused: {result:?}"
            );
        } else {
            assert!(
                !discovered.contains(name),
                "{name} is not linked but was discovered"
            );
            let refusal = result.expect_err("an unlinked pack cannot be selected");
            assert_eq!(refusal.to_string(), format!("unknown pack {name:?}"));
        }
    }
}

#[test]
fn default_features_leave_the_three_optional_packs_out() {
    for (crate_name, text) in [
        ("kkernel", KKERNEL_MANIFEST),
        ("khive-mcp", KHIVE_MCP_MANIFEST),
    ] {
        let manifest = manifest(text);
        let default = feature(&manifest, "default");
        for name in DEFAULT_OFF_PACKS {
            let pack_feature = format!("pack-{name}");
            assert!(
                !default.contains(&pack_feature),
                "{crate_name}: the default features must not link the {name} pack, they are {default:?}"
            );
        }
        for (name, _) in OPTIONAL_PACKS {
            let dependency = format!("khive-pack-{name}");
            let optional = manifest["dependencies"][dependency.as_str()]
                .get("optional")
                .and_then(toml::Value::as_bool);
            assert_eq!(
                optional,
                Some(true),
                "{crate_name}: {dependency} must be an optional dependency"
            );
        }
    }
}

#[test]
fn the_operator_build_links_the_three_optional_packs() {
    let recipe: Vec<&str> = MAKEFILE
        .lines()
        .skip_while(|line| !line.starts_with("build-local:"))
        .skip(1)
        .take_while(|line| line.starts_with('\t'))
        .collect();
    let features: Vec<&str> = recipe
        .iter()
        .filter_map(|line| line.trim().strip_prefix("--features "))
        .flat_map(|list| list.trim_end_matches(" \\").trim().split(','))
        .collect();
    assert!(
        !features.is_empty(),
        "the build-local recipe names no --features list: {recipe:?}"
    );
    for name in DEFAULT_OFF_PACKS {
        let pack_feature = format!("pack-{name}");
        assert!(
            features.contains(&pack_feature.as_str()),
            "make build-local must enable {pack_feature}, it enables {features:?}"
        );
    }
}

#[test]
fn kkernel_forwards_each_pack_feature_and_keeps_khive_mcp_defaults_off() {
    let kkernel = manifest(KKERNEL_MANIFEST);
    for (name, _) in OPTIONAL_PACKS {
        let forwarded = feature(&kkernel, &format!("pack-{name}"));
        let expected_forward = format!("khive-mcp/pack-{name}");
        assert!(
            forwarded.contains(&expected_forward),
            "pack-{name} must enable {expected_forward}"
        );
        let dependency = format!("khive-pack-{name}");
        assert!(
            forwarded.contains(&dependency) || forwarded.contains(&format!("dep:{dependency}")),
            "pack-{name} must enable its local optional dependency"
        );
    }
    for section in ["dependencies", "dev-dependencies"] {
        let defaults = kkernel[section]["khive-mcp"]
            .get("default-features")
            .and_then(toml::Value::as_bool);
        assert_eq!(
            defaults,
            Some(false),
            "kkernel [{section}] must not enable khive-mcp's default features"
        );
    }
}

#[test]
fn git_is_optional_default_on_and_required_by_repository_showcase() {
    for text in [KKERNEL_MANIFEST, KHIVE_MCP_MANIFEST] {
        let manifest = manifest(text);
        assert!(feature(&manifest, "default").contains(&"pack-git".to_string()));
        assert_eq!(
            manifest["dependencies"]["khive-pack-git"]["optional"].as_bool(),
            Some(true)
        );
    }
    let kernel = manifest(KKERNEL_MANIFEST);
    assert!(feature(&kernel, "repo-showcase").contains(&"pack-git".to_string()));
    let mcp = manifest(KHIVE_MCP_MANIFEST);
    assert!(feature(&mcp, "pack-git").contains(&"khive-runtime/pack-git".to_string()));
    let runtime = manifest(include_str!("../../khive-runtime/Cargo.toml"));
    assert!(feature(&runtime, "pack-git").is_empty());
}

#[test]
fn code_is_optional_default_on_and_required_by_repository_showcase() {
    for text in [KKERNEL_MANIFEST, KHIVE_MCP_MANIFEST] {
        let manifest = manifest(text);
        assert_eq!(feature(&manifest, "default"), vec!["pack-git", "pack-code"]);
        assert_eq!(
            manifest["dependencies"]["khive-pack-code"]["optional"].as_bool(),
            Some(true)
        );
    }
    let kernel = manifest(KKERNEL_MANIFEST);
    assert!(feature(&kernel, "repo-showcase").contains(&"pack-code".to_string()));
    let mcp = manifest(KHIVE_MCP_MANIFEST);
    assert!(feature(&mcp, "pack-code").contains(&"khive-runtime/pack-code".to_string()));
    let runtime = manifest(include_str!("../../khive-runtime/Cargo.toml"));
    assert!(feature(&runtime, "pack-code").is_empty());
}

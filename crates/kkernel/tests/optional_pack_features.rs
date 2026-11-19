//! The agent, telemetry and web packs are optional features of `kkernel` and
//! `khive-mcp`: a build links and registers a pack exactly when its feature is on.
//!
//! The first two tests read the registry this binary actually linked and compare it
//! with the features the binary was built with. The last three pin the manifests and
//! the operator build: the default feature set leaves the three packs out, the
//! operator build (`make build-local`) turns all three on, and `kkernel` must not
//! let its `khive-mcp` dependency switch them back on when defaults are off. Any slip
//! changes which packs a plain or an operator build links without failing any other
//! test.

use std::collections::BTreeSet;

use khive_runtime::pack::PackRegistry;
use khive_runtime::RuntimeConfig;
// Keep kkernel's production force-link anchors, including feature-gated packs.
use kkernel as _;

const KKERNEL_MANIFEST: &str = include_str!("../Cargo.toml");
const KHIVE_MCP_MANIFEST: &str = include_str!("../../khive-mcp/Cargo.toml");
const MAKEFILE: &str = include_str!("../../../Makefile");

/// The optional packs, each paired with whether this build links it.
const OPTIONAL_PACKS: [(&str, bool); 3] = [
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
        for (name, _) in OPTIONAL_PACKS {
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
    for (name, _) in OPTIONAL_PACKS {
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
        for expected in [
            format!("khive-mcp/pack-{name}"),
            format!("khive-pack-{name}"),
        ] {
            assert!(
                forwarded.contains(&expected),
                "pack-{name} must enable {expected}, it enables {forwarded:?}"
            );
        }
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

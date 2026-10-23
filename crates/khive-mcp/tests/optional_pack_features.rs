use std::sync::Arc;

use khive_mcp as _;
use khive_runtime::{
    KhiveRuntime, PackMetadataRegistry, PackRegistry, RuntimeConfig, StorageBackend,
    VerbRegistryBuilder,
};

fn registry(names: &[&str]) -> PackMetadataRegistry {
    let packs: Vec<String> = names.iter().map(|name| (*name).to_string()).collect();
    let config = RuntimeConfig {
        packs: packs.clone(),
        brain_profile: None,
        actor_id: None,
        events_split: None,
        mounts: Vec::new(),
        ..RuntimeConfig::no_embeddings().for_metadata_registry()
    };
    let runtime = KhiveRuntime::from_backend(
        Arc::new(StorageBackend::memory().expect("private memory backend")),
        config,
    );
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs(&packs, runtime, &mut builder).expect("selected packs");
    builder.build_metadata().expect("selected pack metadata")
}

#[test]
fn git_factory_and_explicit_selection_follow_the_feature() {
    assert_eq!(
        PackRegistry::discovered_names().contains(&"git"),
        cfg!(feature = "pack-git")
    );
    let selection = vec!["kg".to_string(), "git".to_string()];
    let result = PackRegistry::validate_pack_selection(&selection);
    if cfg!(feature = "pack-git") {
        result.expect("Git can be selected with its KG prerequisite");
    } else {
        assert_eq!(
            result.expect_err("Git is unavailable").to_string(),
            "unknown pack \"git\""
        );
    }
}

#[test]
fn an_unselected_git_pack_contributes_no_handlers_or_vocabulary() {
    let registry = registry(&["kg"]);
    assert_eq!(registry.pack_names(), vec!["kg"]);
    assert!(registry.pack_verbs("git").is_none());
    assert!(!registry.has_verb("git.status"));
    assert!(registry.pack_note_kinds("git").is_none());
    assert!(registry
        .all_handlers_with_names()
        .iter()
        .all(|(pack, handler)| { *pack != "git" && !handler.name.starts_with("git.") }));
}

#[cfg(feature = "pack-git")]
#[test]
fn a_selected_git_pack_exposes_its_real_handlers() {
    let registry = registry(&["kg", "git"]);
    let mut names = registry.pack_names();
    names.sort_unstable();
    assert_eq!(names, vec!["git", "kg"]);
    let handlers = registry.pack_verbs("git").expect("selected Git pack");
    for expected in ["git.digest", "git.status", "git.commit"] {
        assert!(
            handlers.iter().any(|handler| handler.name == expected),
            "{expected}"
        );
    }
}

#[test]
fn code_factory_and_explicit_selection_follow_the_feature() {
    assert_eq!(
        PackRegistry::discovered_names().contains(&"code"),
        cfg!(feature = "pack-code")
    );
    let selection = vec!["kg".to_string(), "code".to_string()];
    let result = PackRegistry::validate_pack_selection(&selection);
    if cfg!(feature = "pack-code") {
        result.expect("Code can be selected with its KG prerequisite");
    } else {
        assert_eq!(
            result.expect_err("Code is unavailable").to_string(),
            "unknown pack \"code\""
        );
    }
}

#[test]
fn an_unselected_code_pack_contributes_no_handlers_or_vocabulary() {
    let registry = registry(&["kg"]);
    assert!(registry.pack_verbs("code").is_none());
    assert!(registry.pack_note_kinds("code").is_none());
    assert!(!registry.has_verb("code.list"));
    assert!(registry
        .all_handlers_with_names()
        .iter()
        .all(|(pack, handler)| { *pack != "code" && !handler.name.starts_with("code.") }));
}

#[cfg(feature = "pack-code")]
#[test]
fn a_selected_code_pack_exposes_its_real_handlers() {
    let registry = registry(&["kg", "code"]);
    let mut names = registry.pack_names();
    names.sort_unstable();
    assert_eq!(names, vec!["code", "kg"]);
    let handlers = registry.pack_verbs("code").expect("selected Code pack");
    for expected in ["code.list", "code.search", "code.deps", "code.ingest"] {
        assert!(
            handlers.iter().any(|handler| handler.name == expected),
            "{expected}"
        );
    }
}

#[test]
fn tool_factory_and_explicit_selection_follow_the_feature() {
    assert_eq!(
        PackRegistry::discovered_names().contains(&"tool"),
        cfg!(feature = "pack-tool")
    );
    let selection = vec!["kg".to_string(), "tool".to_string()];
    let result = PackRegistry::validate_pack_selection(&selection);
    if cfg!(feature = "pack-tool") {
        result.expect("Tool can be selected with its KG prerequisite");
    } else {
        assert_eq!(
            result.expect_err("Tool is unavailable").to_string(),
            "unknown pack \"tool\""
        );
    }
}

#[test]
fn an_unselected_tool_pack_contributes_no_handlers_or_vocabulary() {
    let registry = registry(&["kg"]);
    assert!(registry.pack_verbs("tool").is_none());
    assert!(registry.pack_note_kinds("tool").is_none());
    assert!(registry.pack_entity_kinds("tool").is_none());
    assert!(!registry.has_verb("tool.check"));
    assert!(registry
        .all_handlers_with_names()
        .iter()
        .all(|(pack, handler)| { *pack != "tool" && !handler.name.starts_with("tool.") }));
}

#[cfg(feature = "pack-tool")]
#[test]
fn a_selected_tool_pack_exposes_its_real_handlers() {
    let registry = registry(&["kg", "tool"]);
    let mut names = registry.pack_names();
    names.sort_unstable();
    assert_eq!(names, vec!["kg", "tool"]);
    let handlers = registry.pack_verbs("tool").expect("selected Tool pack");
    for expected in ["tool.register", "tool.check", "tool.request", "tool.policy"] {
        assert!(
            handlers.iter().any(|handler| handler.name == expected),
            "{expected}"
        );
    }
}

#[test]
fn exec_factory_and_explicit_selection_follow_the_feature() {
    assert_eq!(
        PackRegistry::discovered_names().contains(&"exec"),
        cfg!(feature = "pack-exec")
    );
    let mut selection = vec!["kg".to_string(), "exec".to_string()];
    if cfg!(feature = "pack-exec") {
        assert!(cfg!(feature = "pack-tool"));
        selection.extend(["blob".to_string(), "tool".to_string()]);
    }
    let result = PackRegistry::validate_pack_selection(&selection);
    if cfg!(feature = "pack-exec") {
        result.expect("Exec can be selected with its explicit prerequisites");
    } else {
        assert_eq!(
            result.expect_err("Exec is unavailable").to_string(),
            "unknown pack \"exec\""
        );
    }
}

#[test]
fn an_unselected_exec_pack_contributes_no_handlers_or_prerequisites() {
    let registry = registry(&["kg"]);
    assert_eq!(registry.pack_names(), vec!["kg"]);
    assert!(registry.pack_verbs("exec").is_none());
    assert!(registry.pack_verbs("tool").is_none());
    assert!(registry.pack_note_kinds("exec").is_none());
    assert!(registry.pack_entity_kinds("exec").is_none());
    assert!(!registry.has_verb("exec.run"));
    assert!(registry
        .all_handlers_with_names()
        .iter()
        .all(|(pack, handler)| { *pack != "exec" && !handler.name.starts_with("exec.") }));
}

#[cfg(feature = "pack-exec")]
#[test]
fn selected_exec_still_requires_explicit_blob_and_tool_selection() {
    for (names, missing) in [
        (&["kg", "blob", "exec"][..], "tool"),
        (&["kg", "tool", "exec"][..], "blob"),
    ] {
        let selection: Vec<String> = names.iter().map(|name| (*name).to_string()).collect();
        let error = PackRegistry::validate_pack_selection(&selection)
            .expect_err("compiled prerequisites must not be selected implicitly");
        assert!(
            matches!(error, khive_runtime::PackLoadError::MissingDependency { pack, dep }
                if pack == "exec" && dep == missing)
        );
    }
}

#[cfg(feature = "pack-exec")]
#[test]
fn a_selected_exec_pack_exposes_its_real_handlers() {
    let registry = registry(&["kg", "blob", "tool", "exec"]);
    let mut names = registry.pack_names();
    names.sort_unstable();
    assert_eq!(names, vec!["blob", "exec", "kg", "tool"]);
    let handlers = registry.pack_verbs("exec").expect("selected Exec pack");
    for expected in ["exec.tree_get", "exec.run", "exec.receipt"] {
        assert!(
            handlers.iter().any(|handler| handler.name == expected),
            "{expected}"
        );
    }
    assert!(registry.has_verb("tool.check"));
}

#[test]
fn workspace_factory_and_explicit_selection_follow_the_feature() {
    assert_eq!(
        PackRegistry::discovered_names().contains(&"workspace"),
        cfg!(feature = "pack-workspace")
    );
    let mut selection = vec!["kg".to_string(), "workspace".to_string()];
    if cfg!(feature = "pack-workspace") {
        assert!(cfg!(feature = "pack-git"));
        selection.extend(["git".to_string(), "gtd".to_string(), "session".to_string()]);
    }
    let result = PackRegistry::validate_pack_selection(&selection);
    if cfg!(feature = "pack-workspace") {
        result.expect("Workspace can be selected with its explicit prerequisites");
    } else {
        assert_eq!(
            result.expect_err("Workspace is unavailable").to_string(),
            "unknown pack \"workspace\""
        );
    }
}

#[test]
fn an_unselected_workspace_pack_contributes_no_vocabulary_or_endpoints() {
    let registry = registry(&["kg"]);
    assert_eq!(registry.pack_names(), vec!["kg"]);
    assert!(registry.pack_verbs("workspace").is_none());
    assert!(registry.pack_entity_kinds("workspace").is_none());
    assert!(!registry.all_entity_kinds().contains(&"workspace"));
    assert!(registry
        .all_edge_rules_with_packs()
        .iter()
        .all(|(pack, _)| *pack != "workspace"));
}

#[cfg(feature = "pack-workspace")]
#[test]
fn selected_workspace_still_requires_explicit_git_selection() {
    let selection = ["kg", "gtd", "session", "workspace"].map(str::to_string);
    let error = PackRegistry::validate_pack_selection(&selection)
        .expect_err("compiled Git must not be selected implicitly");
    assert!(
        matches!(error, khive_runtime::PackLoadError::MissingDependency { pack, dep }
            if pack == "workspace" && dep == "git")
    );
}

#[cfg(feature = "pack-workspace")]
#[test]
fn selected_workspace_has_zero_verbs_and_exact_membership_vocabulary() {
    use khive_types::{EdgeEndpointRule, EdgeRelation, EndpointKind};

    let registry = registry(&["kg", "git", "gtd", "session", "workspace"]);
    let mut names = registry.pack_names();
    names.sort_unstable();
    assert_eq!(names, vec!["git", "gtd", "kg", "session", "workspace"]);
    assert!(registry
        .pack_verbs("workspace")
        .expect("selected Workspace pack")
        .is_empty());
    assert_eq!(
        registry.pack_entity_kinds("workspace").unwrap(),
        &["workspace"]
    );
    assert!(registry.all_entity_kinds().contains(&"workspace"));
    assert!(registry
        .all_handlers_with_names()
        .iter()
        .all(|(pack, _)| *pack != "workspace"));
    let rules: Vec<_> = registry
        .all_edge_rules_with_packs()
        .into_iter()
        .filter_map(|(pack, rule)| (pack == "workspace").then_some(rule))
        .collect();
    assert_eq!(rules.len(), 5);
    for target in ["issue", "pull_request", "commit", "task", "session"] {
        assert!(
            rules.contains(&EdgeEndpointRule {
                relation: EdgeRelation::Contains,
                source: EndpointKind::EntityOfKind("workspace"),
                target: EndpointKind::NoteOfKind(target),
            }),
            "missing workspace contains {target}"
        );
    }
}

fn expected_automatic_packs() -> Vec<String> {
    let mut expected: Vec<String> = [
        "kg",
        "gtd",
        "memory",
        "brain",
        "comm",
        "schedule",
        "knowledge",
        "session",
        "blob",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    for (pack, enabled) in [
        ("git", cfg!(feature = "pack-git")),
        ("code", cfg!(feature = "pack-code")),
        ("tool", cfg!(feature = "pack-tool")),
        ("exec", cfg!(feature = "pack-exec")),
        ("workspace", cfg!(feature = "pack-workspace")),
    ] {
        if enabled {
            expected.push(pack.to_string());
        }
    }
    expected.sort_unstable();
    expected
}

#[test]
fn automatic_selection_matches_the_frontend_profile_contract() {
    let expected = expected_automatic_packs();
    let mut actual = RuntimeConfig::built_in_packs();
    actual.sort_unstable();
    assert_eq!(actual, expected);
    PackRegistry::validate_pack_selection(&actual)
        .expect("automatic selection must contain only linked packs and all prerequisites");
}

#[test]
fn discovered_factories_match_the_standalone_mcp_feature_set() {
    let mut expected = expected_automatic_packs();
    expected.push("charter".to_string());
    for (pack, enabled) in [
        ("agent", cfg!(feature = "pack-agent")),
        ("telemetry", cfg!(feature = "pack-telemetry")),
        ("web", cfg!(feature = "pack-web")),
        ("moodboard", cfg!(feature = "pack-moodboard")),
    ] {
        if enabled {
            expected.push(pack.to_string());
        }
    }
    expected.sort_unstable();
    let mut actual: Vec<String> = PackRegistry::discovered_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    actual.sort_unstable();
    assert_eq!(actual, expected);
}

#[test]
fn agent_telemetry_and_web_selection_follow_their_features() {
    let core = registry(&["kg"]);
    for (pack, enabled) in [
        ("agent", cfg!(feature = "pack-agent")),
        ("telemetry", cfg!(feature = "pack-telemetry")),
        ("web", cfg!(feature = "pack-web")),
    ] {
        assert!(core.pack_verbs(pack).is_none());
        assert!(core.pack_note_kinds(pack).is_none());
        assert!(core.pack_entity_kinds(pack).is_none());
        let selection = vec!["kg".to_string(), pack.to_string()];
        let result = PackRegistry::validate_pack_selection(&selection);
        if enabled {
            result.expect("linked optional pack can be explicitly selected");
            let selected = registry(&["kg", pack]);
            assert!(!selected.pack_verbs(pack).expect("selected pack").is_empty());
        } else {
            assert_eq!(
                result
                    .expect_err("unlinked pack must be refused")
                    .to_string(),
                format!("unknown pack {pack:?}")
            );
        }
    }
}

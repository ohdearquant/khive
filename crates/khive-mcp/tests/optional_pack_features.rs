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

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

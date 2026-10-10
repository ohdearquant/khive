/// Build the same `RuntimeConfig` `run_reindex` would resolve for `db`, so
/// the tests below drive `open_validated_reindex_backend` (the actual open
/// path) without running a full reindex.
fn resolve_reindex_test_config(
    db: Option<&str>,
    config: &std::path::Path,
) -> khive_runtime::RuntimeConfig {
    resolve_runtime_config(RuntimeConfigInputs {
        db,
        config: Some(config),
        namespace: Namespace::parse("local").expect("ns"),
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: None,
        brain_profile: None,
    })
    .expect("resolve reindex runtime config")
}

#[test]
#[serial]
fn nothing_changed_between_validation_and_open_succeeds() {
    if crate::test_process::run_in_child() {
        return;
    }

    let dir = tempfile::tempdir().expect("temp dir");
    let a_db = dir.path().join("a.db");
    let config = dir.path().join("khive.toml");
    std::fs::write(
        &config,
        format!(
            "[[backends]]\nname = \"main\"\nkind = \"sqlite\"\npath = \"{}\"\n",
            a_db.display()
        ),
    )
    .expect("write config");

    let validated = validate_declared_reindex_target(a_db.to_str(), Some(&config))
        .expect("a.db is the declared main backend")
        .expect("backends declared: a validated target must be returned");

    let cfg = resolve_reindex_test_config(a_db.to_str(), &config);
    open_validated_reindex_backend(cfg, Some(&validated))
        .expect("an unchanged declared target must open normally (control)");
}

#[test]
#[serial]
fn declared_secondary_backend_open_targets_its_own_file_not_main() {
    if crate::test_process::run_in_child() {
        return;
    }

    let dir = tempfile::tempdir().expect("temp dir");
    let (config, main_db, knowledge_db) = write_declared_backend_test_config(dir.path());

    let validated = validate_declared_reindex_target(knowledge_db.to_str(), Some(&config))
        .expect("knowledge.db is a declared secondary backend")
        .expect("backends declared: a validated target must be returned");
    assert_eq!(
        validated.path.file_name(),
        knowledge_db.file_name(),
        "the validated target must be knowledge.db, not main.db"
    );

    let cfg = resolve_reindex_test_config(knowledge_db.to_str(), &config);
    open_validated_reindex_backend(cfg, Some(&validated))
        .expect("declared secondary backend must open");

    assert!(
        knowledge_db.exists(),
        "reindex must create/open the targeted secondary backend"
    );
    assert!(
        !main_db.exists(),
        "no override normalization on the reindex path may redirect the secondary \
             target to main (control)"
    );
}

#[test]
#[serial]
#[cfg(unix)]
fn symlink_retargeted_after_validation_does_not_redirect_the_open() {
    if crate::test_process::run_in_child() {
        return;
    }

    let dir = tempfile::tempdir().expect("temp dir");
    let a_db = dir.path().join("a.db");
    let b_db = dir.path().join("b.db");
    let link_db = dir.path().join("link.db");
    std::fs::write(&a_db, b"").expect("create a.db");
    std::os::unix::fs::symlink(&a_db, &link_db).expect("create symlink");

    let config = dir.path().join("khive.toml");
    std::fs::write(
        &config,
        format!(
            "[[backends]]\nname = \"main\"\nkind = \"sqlite\"\npath = \"{}\"\n",
            link_db.display()
        ),
    )
    .expect("write config");

    let validated = validate_declared_reindex_target(link_db.to_str(), Some(&config))
        .expect("link.db resolves through the symlink to the declared backend")
        .expect("backends declared: a validated target must be returned");
    assert_eq!(
        validated.path,
        a_db.canonicalize().expect("canonicalize a.db"),
        "validation must resolve the symlink to a.db's own canonical path"
    );

    // Attacker retargets the symlink to an undeclared file after
    // validation, before anything opens it.
    std::fs::remove_file(&link_db).expect("remove symlink");
    std::os::unix::fs::symlink(&b_db, &link_db).expect("retarget symlink");

    // Expected before the fix: `run_reindex` resolved `cfg.db_path` from
    // the RAW `--db` string alone (tilde-expansion only, never
    // canonicalized), so `KhiveRuntime::new` opened straight through
    // `link.db` and the OS followed its CURRENT target — this
    // demonstrates that pre-fix behavior directly, since
    // `open_validated_reindex_backend` did not exist before this fix.
    let pre_fix_cfg = resolve_reindex_test_config(link_db.to_str(), &config);
    KhiveRuntime::new(pre_fix_cfg)
        .expect("red before the fix: the retargeted symlink opens without complaint");
    assert!(
        b_db.exists(),
        "red before the fix: resolving the raw --db string at open time follows the \
             retargeted symlink and creates the undeclared b.db"
    );
    std::fs::remove_file(&b_db).expect("reset the undeclared file for the fixed path");

    // Fixed path: the open is pinned to the canonical path validation
    // observed, so the retargeted symlink has no effect.
    let cfg = resolve_reindex_test_config(link_db.to_str(), &config);
    open_validated_reindex_backend(cfg, Some(&validated))
        .expect("open the declared backend through its validated identity");
    assert!(
        !b_db.exists(),
        "the retargeted undeclared file must never be touched"
    );
}

#[test]
#[serial]
fn file_replaced_in_place_after_validation_is_refused_at_open() {
    if crate::test_process::run_in_child() {
        return;
    }

    let dir = tempfile::tempdir().expect("temp dir");
    let a_db = dir.path().join("a.db");
    let other_db = dir.path().join("other.db");
    std::fs::write(&a_db, b"declared backend").expect("create a.db");
    std::fs::write(&other_db, b"a different file entirely").expect("create other.db");

    let config = dir.path().join("khive.toml");
    std::fs::write(
        &config,
        format!(
            "[[backends]]\nname = \"main\"\nkind = \"sqlite\"\npath = \"{}\"\n",
            a_db.display()
        ),
    )
    .expect("write config");

    let validated = validate_declared_reindex_target(a_db.to_str(), Some(&config))
        .expect("a.db is the declared main backend")
        .expect("backends declared: a validated target must be returned");

    // Same path string, different underlying file: replace a.db in place
    // between validation and open. The pre-fix validator only ever
    // compared canonicalized path STRINGS, never filesystem identity, so
    // this is the arm that proves the check binds to identity, not to
    // the string that survived the rename.
    std::fs::rename(&other_db, &a_db).expect("swap a.db's contents in place");

    let cfg = resolve_reindex_test_config(a_db.to_str(), &config);
    let error = open_validated_reindex_backend(cfg, Some(&validated))
        .map(|_| ())
        .expect_err("a file swapped in place after validation must be refused, not opened");
    let message = error.to_string();
    assert!(message.contains("changed identity between validation and open"));
    assert!(message.contains(&a_db.display().to_string()));
}

    #[test]
    fn sidecar_name_derives_from_the_full_file_name() {
        let path = events_db_path_beside(Path::new("/data/khive.db"));
        assert!(path.ends_with("khive.db.events.db"), "got {path:?}");
    }

    #[test]
    fn databases_sharing_a_stem_get_distinct_sidecars() {
        let a = events_db_path_beside(Path::new("/data/a.db"));
        let b = events_db_path_beside(Path::new("/data/a.sqlite"));
        assert_ne!(a, b, "a.db and a.sqlite must not share an event plane");
        assert!(a.ends_with("a.db.events.db"), "got {a:?}");
        assert!(b.ends_with("a.sqlite.events.db"), "got {b:?}");
    }

    #[cfg(unix)]
    #[test]
    fn directory_aliases_resolve_to_one_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        assert_eq!(
            events_db_path_beside(&real.join("khive.db")),
            events_db_path_beside(&alias.join("khive.db")),
            "a symlinked spelling of one directory must not mint a second sidecar"
        );
    }

    #[cfg(unix)]
    #[test]
    fn final_component_aliases_of_one_database_share_one_sidecar() {
        // Backend identity canonicalizes the whole database path, so a
        // final-component symlink alias is the same database; its sidecar
        // and socket must be the same too, or a process opening one spelling
        // writes event rows a process opening the other never reads.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.db");
        std::fs::write(&real, b"").unwrap();
        let alias = dir.path().join("alias.db");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let real_sidecar = events_db_path_beside(&real);
        let alias_sidecar = events_db_path_beside(&alias);
        assert_eq!(
            real_sidecar, alias_sidecar,
            "a symlink alias of one database file must not mint a second event store"
        );
        assert_eq!(
            events_socket_path_beside(&real_sidecar),
            events_socket_path_beside(&alias_sidecar),
        );
        // Control: a genuinely distinct database in the same directory keeps
        // its own sidecar — resolution must not collapse different files.
        let other = dir.path().join("other.db");
        std::fs::write(&other, b"").unwrap();
        assert_ne!(events_db_path_beside(&other), real_sidecar);
    }

    #[cfg(unix)]
    #[test]
    fn dangling_alias_derives_the_target_sidecar_on_cold_start() {
        // Event-path derivation runs before backend creation, so the alias
        // can be consulted while its target does not exist yet — the first
        // open through the alias is what creates the target. A dangling
        // link must therefore already derive the TARGET's sidecar, or an
        // alias-first cold start and a later target-spelled process split
        // the event store between them.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.db");
        let alias = dir.path().join("alias.db");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        // Neither real.db nor its sidecar exists at this point.
        assert_eq!(
            events_db_path_beside(&alias),
            events_db_path_beside(&real),
            "a dangling alias must derive the same sidecar its target will use"
        );
        // Chain: a link to a link resolves all the way down.
        let chain = dir.path().join("chain.db");
        std::os::unix::fs::symlink(&alias, &chain).unwrap();
        assert_eq!(events_db_path_beside(&chain), events_db_path_beside(&real));
        // Control: a distinct nonexistent file still gets its own sidecar.
        assert_ne!(
            events_db_path_beside(&dir.path().join("unrelated.db")),
            events_db_path_beside(&real)
        );
    }

    #[cfg(unix)]
    #[test]
    fn planted_symlink_at_the_sidecar_path_is_refused() {
        // The sidecar path is derived, never user-chosen, so a pre-existing
        // symlink there is a planted redirect: following it would tighten
        // permissions on and write event rows into the link's target.
        let dir = tempfile::tempdir().unwrap();
        let _registry_guard = TestRegistryGuard::new(dir.path());
        let victim = dir.path().join("victim.txt");
        std::fs::write(&victim, b"victim-bytes").unwrap();
        let mode_before = victim.metadata().unwrap().permissions();
        let sidecar = dir.path().join("khive.db.events.db");
        std::os::unix::fs::symlink(&victim, &sidecar).unwrap();
        let err = match direct_backend_for(&sidecar) {
            Ok(_) => panic!("a planted symlink at the sidecar path must be refused"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("symlink"), "got: {err}");
        // The link's target is untouched: content intact, mode not tightened.
        assert_eq!(std::fs::read(&victim).unwrap(), b"victim-bytes");
        assert_eq!(victim.metadata().unwrap().permissions(), mode_before);
        // A dangling link is refused too, and nothing is created at its
        // target — without the refusal, the open itself would mint the
        // redirect target.
        let ghost_target = dir.path().join("ghost.db");
        let dangling = dir.path().join("other.db.events.db");
        std::os::unix::fs::symlink(&ghost_target, &dangling).unwrap();
        assert!(direct_backend_for(&dangling).is_err());
        assert!(
            !ghost_target.exists(),
            "refusal must not create the redirect target"
        );
        // Control: a regular path proceeds and the backend opens.
        let regular = dir.path().join("plain.db.events.db");
        direct_backend_for(&regular).expect("regular sidecar path must open");
        assert!(regular.exists());
    }

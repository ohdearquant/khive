fn chain(scratch: &Scratch, count: usize) {
    scratch.file("chain/payload");
    symlink("chain/0", scratch.path().join("entry")).unwrap();
    for index in 0..count - 1 {
        let target = if index + 1 == count - 1 {
            "payload".into()
        } else {
            (index + 1).to_string()
        };
        symlink(target, scratch.path().join(format!("chain/{index}"))).unwrap();
    }
}

#[test]
fn link_chain_budget_is_exact_and_pure_cycles_refuse() {
    for count in [TREE_LINK_BUDGET, TREE_LINK_BUDGET + 1] {
        let scratch = Scratch::new();
        chain(&scratch, count);
        let result = walk_tree(scratch.path(), limits(0, 2, true), |path, _| {
            path.as_path() != Path::new("chain")
        });
        if count == TREE_LINK_BUDGET {
            assert_eq!(paths(result.unwrap()), expected(&["entry"]));
        } else {
            assert!(matches!(
                result,
                Err(WalkError::LinkBudgetExceeded {
                    max_links: TREE_LINK_BUDGET,
                    ..
                })
            ));
        }
    }
    let scratch = Scratch::new();
    symlink("b", scratch.path().join("a")).unwrap();
    symlink("a", scratch.path().join("b")).unwrap();
    assert!(matches!(
        walk_tree(scratch.path(), limits(1, 2, true), |_, _| true),
        Err(WalkError::LinkBudgetExceeded { .. })
    ));
}

#[test]
fn link_budget_is_global_across_separate_entries() {
    let scratch = Scratch::new();
    scratch.file("payload");
    for index in 0..TREE_LINK_BUDGET {
        symlink("payload", scratch.path().join(format!("link{index:02}"))).unwrap();
    }
    assert_eq!(
        walk_tree(scratch.path(), limits(0, 100, true), |_, _| true)
            .unwrap()
            .len(),
        TREE_LINK_BUDGET + 1
    );
    symlink("payload", scratch.path().join("one-more")).unwrap();
    assert!(matches!(
        walk_tree(scratch.path(), limits(0, 100, true), |_, _| true),
        Err(WalkError::LinkBudgetExceeded { .. })
    ));
}

#[test]
fn renaming_root_and_replacing_its_path_cannot_redirect_relative_descendants() {
    let scratch = Scratch::new();
    scratch.file("root/a-trigger");
    scratch.file("root/b-dir/original");
    scratch.file("outside/b-dir/wrong");
    let root = scratch.path().join("root");
    symlink("b-dir/original", root.join("c-relative")).unwrap();
    let result = walk_tree(&root, limits(2, 10, true), |path, _| {
        if path.as_path() == Path::new("a-trigger") {
            fs::rename(&root, scratch.path().join("moved")).unwrap();
            symlink(scratch.path().join("outside"), &root).unwrap();
        }
        true
    })
    .unwrap();
    assert_eq!(
        paths(result),
        expected(&["a-trigger", "b-dir/original", "c-relative"])
    );
}

#[test]
fn absolute_link_through_replaced_root_refuses_identity_mismatch() {
    let scratch = Scratch::new();
    scratch.file("root/a-trigger");
    scratch.file("root/payload");
    scratch.file("replacement/payload");
    let root = scratch.path().join("root");
    symlink(root.join("payload"), root.join("z-absolute")).unwrap();
    let result = walk_tree(&root, limits(1, 10, true), |path, _| {
        if path.as_path() == Path::new("a-trigger") {
            fs::rename(&root, scratch.path().join("moved")).unwrap();
            fs::rename(scratch.path().join("replacement"), &root).unwrap();
        }
        true
    });
    assert!(matches!(
        result,
        Err(WalkError::RootChanged { source: None, .. })
    ));
}

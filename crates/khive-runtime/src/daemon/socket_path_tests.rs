/// A symlink component owned by neither the daemon euid nor root is
/// refused on the symlink itself: its owner can retarget it after
/// validation, so where it currently points is irrelevant. A non-root
/// test cannot create a foreign-owned symlink, so this injects a
/// mismatched euid instead — and the fixture must live under an
/// all-root-owned chain (the platform `/tmp`), because a self-owned
/// ancestor would already fail the injected euid before the walk ever
/// reached the link. Root cannot exercise this fixture because its symlink is
/// trusted; the test also skips where `/tmp` is not root-owned.
#[test]
fn foreign_owned_symlink_component_is_refused() {
    // SAFETY: `geteuid` is always successful and takes no arguments.
    let euid = unsafe { libc::geteuid() } as u32;
    if euid == 0 {
        eprintln!(
            "skipped: root-owned symlinks are trusted, so this fixture requires a non-root uid"
        );
        return;
    }
    let tmp_owner = std::fs::metadata("/tmp").expect("stat /tmp").uid();
    if tmp_owner != 0 {
        eprintln!("skipped: /tmp is owned by uid {tmp_owner}, not root");
        return;
    }
    let link = std::path::PathBuf::from(format!("/tmp/khive-swaptest-link-{}", std::process::id()));
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink("/tmp", &link).expect("symlink");

    let not_my_euid = euid.wrapping_add(1);
    let result = ensure_socket_path_is_swap_resistant(&link, not_my_euid);
    std::fs::remove_file(&link).expect("cleanup");

    let err = result.expect_err("a symlink owned by another uid must be refused");
    assert!(
        err.to_string().contains("symlink component"),
        "the refusal should strike the symlink itself, got: {err}"
    );
    assert!(
        err.to_string().contains("khive-swaptest-link"),
        "the refusal should name the offending link, got: {err}"
    );
}

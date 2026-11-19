use super::*;
use std::os::unix::fs::PermissionsExt;

fn snapshot(directory: &Path) -> BTreeMap<std::path::PathBuf, Option<Vec<u8>>> {
    fn visit(root: &Path, path: &Path, rows: &mut BTreeMap<std::path::PathBuf, Option<Vec<u8>>>) {
        for entry in std::fs::read_dir(path).expect("read fixture directory") {
            let entry = entry.expect("fixture entry");
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .expect("fixture-relative path")
                .to_path_buf();
            if entry.file_type().expect("fixture type").is_dir() {
                rows.insert(relative, None);
                visit(root, &path, rows);
            } else {
                rows.insert(
                    relative,
                    Some(std::fs::read(&path).expect("read fixture file")),
                );
            }
        }
    }
    let mut rows = BTreeMap::new();
    visit(directory, directory, &mut rows);
    rows
}

#[tokio::test]
async fn init_refuses_bare_worktree_and_gitfile_targets_without_changing_them() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let temp = tempfile::tempdir().expect("private fixtures");
    let program = Path::new("git");
    for layout in ["bare", "worktree", "gitfile"] {
        let target = temp.path().join(layout);
        std::fs::create_dir(&target).expect("target directory");
        let separate = temp.path().join("separate.git");
        let mut args = vec!["init", "--quiet", "--template=", "-b", "existing"];
        if layout == "bare" {
            args.push("--bare");
        } else if layout == "gitfile" {
            args.extend([
                "--separate-git-dir",
                separate.to_str().expect("fixture path"),
            ]);
        }
        run_git(program, &target, &args, None, None, false).expect("native repository fixture");
        if layout == "bare" {
            assert!(!target.join(".git").exists());
            assert!(target.join("HEAD").is_file());
        } else if layout == "gitfile" {
            assert!(target.join(".git").is_file());
        }
        let before = snapshot(temp.path());
        let error = init(program, &target, "replacement")
            .await
            .expect_err("existing target must refuse");
        assert_eq!(error.code(), "already_initialized", "{layout}: {error}");
        assert_eq!(
            snapshot(temp.path()),
            before,
            "{layout}: init modified an existing repository"
        );
    }
}

#[tokio::test]
async fn init_accepts_empty_targets_and_does_not_confuse_parent_or_marker_files_with_a_repository()
{
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let temp = tempfile::tempdir().expect("private fixtures");
    let program = Path::new("git");
    for (name, enclosing_repository, ordinary_markers) in [
        ("empty", false, false),
        ("nested", true, false),
        ("markers", false, true),
    ] {
        let parent = temp.path().join(name);
        std::fs::create_dir(&parent).expect("parent directory");
        if enclosing_repository {
            run_git(
                program,
                &parent,
                &["init", "--quiet", "--template=", "-b", "parent"],
                None,
                None,
                false,
            )
            .expect("enclosing repository");
        }
        let target = parent.join("target");
        std::fs::create_dir(&target).expect("target directory");
        if ordinary_markers {
            std::fs::write(target.join("HEAD"), b"ordinary application data\n")
                .expect("ordinary HEAD file");
            std::fs::create_dir(target.join("objects")).expect("ordinary objects directory");
            std::fs::create_dir(target.join("refs")).expect("ordinary refs directory");
        }
        assert_eq!(
            init(program, &target, "created")
                .await
                .expect("nonrepository target must initialize"),
            "created"
        );
        assert!(target.join(".git/HEAD").is_file());
        if enclosing_repository {
            assert_eq!(
                std::fs::read(parent.join(".git/HEAD")).unwrap(),
                b"ref: refs/heads/parent\n"
            );
        }
        if ordinary_markers {
            assert_eq!(
                std::fs::read(target.join("HEAD")).unwrap(),
                b"ordinary application data\n"
            );
        }
    }
}

#[tokio::test]
async fn init_probe_failures_never_start_initialization() {
    let _env_guard = crate::cache::ENV_MUTEX.lock().await;
    let temp = tempfile::tempdir().expect("private fixtures");
    for (index, (probe, config, expected_code)) in [
        (
            "printf 'unexpected failure\\n' >&2; exit 128",
            "exit 1",
            "git_failed",
        ),
        (
            "printf \"fatal: not a gitdir '.'\\n\" >&2; printf unexpected; exit 128",
            "exit 1",
            "git_failed",
        ),
        (
            "printf \"fatal: not a gitdir '.'\\n\" >&2; printf '%70000s' '' >&2; exit 128",
            "exit 1",
            "git_failed",
        ),
        (
            "printf 'unexpected status\\n' >&2; exit 23",
            "exit 1",
            "git_failed",
        ),
        ("kill -TERM $$", "exit 1", "git_failed"),
        ("exit 0", "/bin/rm \"$0\"; exit 1", "git_spawn"),
        ("printf unexpected; exit 0", "exit 1", "already_initialized"),
    ]
    .into_iter()
    .enumerate()
    {
        let target = temp.path().join(format!("target-{index}"));
        std::fs::create_dir(&target).expect("target directory");
        let program = temp.path().join(format!("git-probe-{index}"));
        let script = format!(
            "#!/bin/sh\nwhile [ \"$1\" != -C ]; do shift; done\nshift\nrepo=$1\nshift\ncase \"$1\" in\n config) {config} ;;\n rev-parse) {probe} ;;\n init) printf started > \"$repo/init-ran\"; exit 0 ;;\n *) exit 23 ;;\nesac\n"
        );
        std::fs::write(&program, script).expect("probe failure fixture");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))
            .expect("executable fixture");
        let error = init(&program, &target, "created")
            .await
            .expect_err("unknown probe outcome must refuse");
        assert_eq!(error.code(), expected_code, "{error}");
        assert!(
            !error.to_string().contains("unexpected"),
            "probe output leaked"
        );
        assert!(!target.join("init-ran").exists());
        assert!(!target.join(".git").exists());
    }
    let target = temp.path().join("missing-program");
    std::fs::create_dir(&target).expect("target directory");
    let error = init(&temp.path().join("absent-git"), &target, "created")
        .await
        .expect_err("spawn failure must refuse");
    assert_eq!(error.code(), "git_config");
    assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
}

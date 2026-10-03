use super::*;
use std::os::unix::fs::PermissionsExt;

fn git_succeeds(repo: &Path, argv: &[&str]) {
    assert!(base_command(Path::new("git"))
        .arg("-C")
        .arg(repo)
        .args(argv)
        .status()
        .unwrap()
        .success());
}

fn write_trace_wrapper(directory: &Path) -> std::path::PathBuf {
    let wrapper = directory.join("git-trace-wrapper");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\nrepo=\nnext_repo=0\nconfig=0\nhash=0\nfor arg do\n if [ \"$next_repo\" = 1 ]; then repo=\"$arg\"; next_repo=0; fi\n if [ \"$arg\" = -C ]; then next_repo=1; fi\n if [ \"$arg\" = config ]; then config=1; fi\n if [ \"$arg\" = hash-object ]; then hash=1; fi\ndone\nif [ \"$config\" = 1 ]; then printf 'config\\n' >> \"$repo/config-count\"; fi\nif [ \"$hash\" = 1 ]; then printf 'hash-object\\n' >> \"$repo/hash-count\"; fi\nexec git \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    wrapper
}

#[tokio::test]
async fn tree_write_hashes_each_distinct_content_once_and_preserves_manifest() {
    for population in [1, 16, 64] {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git_succeeds(&repo, &["init", "--quiet", "--template="]);
        let contents: [&[u8]; 3] = [b"", b"shared content\n", b"another shared content\n"];
        for index in 0..population {
            let directory = repo.join(format!("directory-{}", index % 2));
            std::fs::create_dir_all(&directory).unwrap();
            let path = directory.join(format!("file-{index:04}.txt"));
            std::fs::write(&path, contents[index % contents.len()]).unwrap();
            std::fs::set_permissions(
                &path,
                std::fs::Permissions::from_mode(if index % 2 == 0 { 0o644 } else { 0o755 }),
            )
            .unwrap();
        }
        git_succeeds(&repo, &["add", "--all"]);
        git_succeeds(
            &repo,
            &[
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );
        let expected = base_command(Path::new("git"))
            .arg("-C")
            .arg(&repo)
            .args(["rev-parse", "HEAD^{tree}"])
            .output()
            .unwrap();
        assert!(expected.status.success());
        let expected_tree = oid_output(&expected.stdout).unwrap();
        let mut config = khive_runtime::RuntimeConfig::no_embeddings();
        config.db_path = Some(temp.path().join("runtime.db"));
        config.git_write.program = Some(write_trace_wrapper(temp.path()));
        let runtime = KhiveRuntime::new(config).unwrap();
        runtime
            .install_blob_store(std::sync::Arc::new(
                khive_db::stores::blob::FsBlobStore::new(temp.path().join("blobs"), 0).unwrap(),
            ))
            .unwrap();
        let checked_out = checkout(&runtime, &repo, "HEAD").await.unwrap();
        let manifest = tree::load(&runtime, &checked_out.tree).await.unwrap();
        let manifest_ref = ContentRef::from_hex(&checked_out.tree).unwrap();
        let hydrator = runtime.blob_hydrator().unwrap();
        let manifest_bytes = hydrator
            .hydrate_verified(&manifest_ref, tree::MAX_MANIFEST_BYTES)
            .await
            .unwrap()
            .bytes()
            .to_vec();
        assert_eq!(manifest.len(), population);
        let distinct_content = manifest
            .iter()
            .map(|entry| entry.content_ref.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        assert_eq!(distinct_content, population.min(contents.len()));
        std::fs::write(repo.join("config-count"), "").unwrap();
        std::fs::write(repo.join("hash-count"), "").unwrap();
        let written = write_manifest_tree(&runtime, &repo, &checked_out.tree)
            .await
            .unwrap();
        assert_eq!(
            written, expected_tree,
            "native Git tree oid must be identical"
        );
        assert_eq!(
            tree::load(&runtime, &checked_out.tree).await.unwrap(),
            manifest,
            "tree writing must not change manifest paths or modes"
        );
        assert_eq!(
            hydrator
                .hydrate_verified(&manifest_ref, tree::MAX_MANIFEST_BYTES)
                .await
                .unwrap()
                .bytes(),
            manifest_bytes.as_slice(),
            "stored manifest bytes must be identical"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("hash-count"))
                .unwrap()
                .lines()
                .count(),
            distinct_content,
            "real hash-object children must follow distinct content, not path count"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("config-count"))
                .unwrap()
                .lines()
                .count(),
            1,
            "the existing per-operation filter snapshot must survive"
        );
        std::fs::write(repo.join("config-count"), "").unwrap();
        std::fs::write(repo.join("hash-count"), "").unwrap();
        assert_eq!(
            write_manifest_tree(&runtime, &repo, &checked_out.tree)
                .await
                .unwrap(),
            expected_tree
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("hash-count"))
                .unwrap()
                .lines()
                .count(),
            distinct_content,
            "a later operation must perform its own content writes"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("config-count"))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }
}

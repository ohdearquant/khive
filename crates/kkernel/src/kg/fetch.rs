//! `kkernel kg fetch` — fetch a remote KG archive.

use anyhow::{Context, Result};

use super::types::FetchArgs;
use crate::sync::RemoteName;

pub(super) async fn cmd_fetch(args: FetchArgs) -> Result<()> {
    let name = RemoteName::parse(args.remote).context("invalid --remote")?;

    let pin = args
        .pin
        .as_deref()
        .map(khive_vcs::SnapshotId::from_prefixed)
        .transpose()
        .context("invalid --pin")?;

    let remote = crate::sync::RemoteConfig {
        name,
        url: args.url,
        git_ref: args.git_ref,
        namespace: args.namespace,
        pin,
    };

    let report = crate::sync::run_sync_remote(&args.repo, &remote, args.repin)
        .await
        .with_context(|| format!("fetch remote {:?}", remote.name))?;
    let json = serde_json::to_string(&report).expect("serialize RemoteSyncReport");
    println!("{json}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        // Hermetic: machine-wide hooks (e.g. leak-guard via core.hooksPath)
        // must not block commits inside throwaway test repos.
        let status = std::process::Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap_or_else(|e| panic!("git {} failed to spawn: {e}", args.join(" ")));
        assert!(
            status.success(),
            "git {} exited with {}",
            args.join(" "),
            status
        );
    }

    fn make_git_remote_for_kg(dir: &std::path::Path) -> String {
        let kg_dir = dir.join(".khive/kg");
        std::fs::create_dir_all(&kg_dir).unwrap();
        let entity_id = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let entities = format!(
            r#"{{"id":"{entity_id}","kind":"concept","name":"RemoteEntity","properties":{{}},"tags":[]}}"#
        );
        std::fs::write(kg_dir.join("entities.ndjson"), &entities).unwrap();
        std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();
        run_git(dir, &["init", "-b", "main"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
        run_git(dir, &["config", "user.name", "Test"]);
        run_git(dir, &["add", "-A"]);
        run_git(dir, &["commit", "-m", "init"]);
        dir.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn fetch_populates_temp_remote_cache() {
        let remote_dir = TempDir::new().unwrap();
        let repo_dir = TempDir::new().unwrap();
        let remote_url = make_git_remote_for_kg(remote_dir.path());

        let args = FetchArgs {
            remote: "upstream".to_string(),
            repo: repo_dir.path().to_path_buf(),
            url: remote_url,
            git_ref: "main".to_string(),
            namespace: "remote-ns".to_string(),
            pin: None,
            repin: false,
        };

        cmd_fetch(args).await.unwrap();

        let cache = repo_dir.path().join(".khive/kg/remotes/upstream");
        assert!(
            cache.join("entities.ndjson").exists(),
            "entities.ndjson in cache"
        );
        assert!(cache.join("edges.ndjson").exists(), "edges.ndjson in cache");
        assert!(cache.join("meta.json").exists(), "meta.json in cache");
    }

    #[tokio::test]
    async fn fetch_protects_cache_with_legacy_parent_gitignore() {
        let remote_dir = TempDir::new().unwrap();
        let repo_dir = TempDir::new().unwrap();
        let remote_url = make_git_remote_for_kg(remote_dir.path());
        run_git(repo_dir.path(), &["init", "--quiet"]);
        let kg_dir = repo_dir.path().join(".khive/kg");
        std::fs::create_dir_all(&kg_dir).unwrap();
        std::fs::write(
            repo_dir.path().join(".khive/.gitignore"),
            "*\n!.gitignore\n!kg/\n!kg/**\nkg/.remote-cache/\nkg/.remote-cache/**\n",
        )
        .unwrap();
        std::fs::write(kg_dir.join("entities.ndjson"), "local export\n").unwrap();
        let before = std::process::Command::new("git")
            .args([
                "-c",
                "core.excludesFile=/dev/null",
                "check-ignore",
                "--no-index",
                "-q",
                "--",
                ".khive/kg/remotes/upstream/meta.json",
            ])
            .current_dir(repo_dir.path())
            .output()
            .unwrap();
        assert_eq!(
            before.status.code(),
            Some(1),
            "the old parent rule alone leaves the cache trackable"
        );

        cmd_fetch(FetchArgs {
            remote: "upstream".to_string(),
            repo: repo_dir.path().to_path_buf(),
            url: remote_url,
            git_ref: "main".to_string(),
            namespace: "remote-ns".to_string(),
            pin: None,
            repin: false,
        })
        .await
        .unwrap();

        let remotes = kg_dir.join("remotes");
        assert_eq!(
            std::fs::read_to_string(remotes.join(".gitignore")).unwrap(),
            "*\n"
        );
        let backup = remotes.join("upstream.replaced~4242");
        std::fs::create_dir_all(&backup).unwrap();
        std::fs::write(backup.join(".khive-backup-owner"), "test marker\n").unwrap();

        for relative in [
            ".khive/kg/remotes/upstream/meta.json",
            ".khive/kg/remotes/upstream.replaced~4242/.khive-backup-owner",
        ] {
            let check = std::process::Command::new("git")
                .args([
                    "-c",
                    "core.excludesFile=/dev/null",
                    "check-ignore",
                    "--no-index",
                    "-q",
                    "--",
                    relative,
                ])
                .current_dir(repo_dir.path())
                .output()
                .unwrap();
            assert!(check.status.success(), "{relative} must be ignored");
        }
        let export_check = std::process::Command::new("git")
            .args([
                "-c",
                "core.excludesFile=/dev/null",
                "check-ignore",
                "--no-index",
                "-q",
                "--",
                ".khive/kg/entities.ndjson",
            ])
            .current_dir(repo_dir.path())
            .output()
            .unwrap();
        assert_eq!(
            export_check.status.code(),
            Some(1),
            "KG export remains trackable"
        );
    }
}

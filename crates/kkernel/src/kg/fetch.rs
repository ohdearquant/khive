//! `kkernel kg fetch` — fetch a remote KG archive.

use anyhow::{bail, Context, Result};

use super::types::FetchArgs;

pub(super) fn is_safe_remote_name(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

pub(super) async fn cmd_fetch(args: FetchArgs) -> Result<()> {
    if !is_safe_remote_name(&args.remote) {
        bail!(
            "invalid remote name {:?}: must be [A-Za-z0-9._-]+ and not . or ..",
            args.remote
        );
    }

    let pin = args
        .pin
        .as_deref()
        .map(khive_vcs::SnapshotId::from_prefixed)
        .transpose()
        .context("invalid --pin")?;

    let remote = crate::sync::RemoteConfig {
        name: args.remote,
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
        let status = std::process::Command::new("git")
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
}

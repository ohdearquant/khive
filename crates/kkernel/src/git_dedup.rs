//! Explicit, bounded issue/PR note repair for one canonical project.

use anyhow::{Context, Result};
use clap::Parser;
use uuid::Uuid;

use khive_mcp::serve::{resolve_runtime_config, RuntimeConfigInputs};
use khive_pack_git::dedup::{run_dedup, DedupOptions};
use khive_runtime::{IngestAuditStore, KhiveRuntime, Namespace, PackRegistry};

#[derive(Parser, Debug)]
pub struct GitDedupArgs {
    /// Exact live canonical project UUID; prefixes are not accepted.
    #[arg(long)]
    pub project: Uuid,
    /// Apply the reported per-pair merges; omission only previews.
    #[arg(long)]
    pub apply: bool,
    #[arg(long, env = "KHIVE_DB")]
    pub db: Option<String>,
    #[arg(long, default_value = "local")]
    pub namespace: String,
}

pub async fn run(args: GitDedupArgs) -> Result<()> {
    let namespace =
        Namespace::parse(&args.namespace).map_err(|error| anyhow::anyhow!("{error}"))?;
    let config = resolve_runtime_config(RuntimeConfigInputs {
        db: args.db.as_deref(),
        config: None,
        namespace,
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: None,
        brain_profile: None,
    })?;
    let runtime = KhiveRuntime::new(config).map_err(|error| anyhow::anyhow!("{error}"))?;
    let token = runtime
        .authorize(runtime.config().default_namespace.clone())
        .map_err(|error| anyhow::anyhow!("{error}"))
        .context("authorizing namespace")?;
    let registry = PackRegistry::build_ingest_registry(&runtime, IngestAuditStore::Detach)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    runtime.install_kind_registry(
        registry
            .all_entity_kinds()
            .into_iter()
            .map(str::to_owned)
            .collect(),
        registry
            .all_note_kinds()
            .into_iter()
            .map(str::to_owned)
            .collect(),
    );
    runtime.install_pack_owned_note_kinds(
        registry
            .pack_owned_note_kinds()
            .into_iter()
            .map(str::to_owned)
            .collect(),
    );
    let report = run_dedup(
        &runtime,
        &token,
        DedupOptions {
            project_id: args.project,
            apply: args.apply,
        },
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    if !report.success {
        anyhow::bail!("git-note deduplication did not complete; inspect the JSON report");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_requires_full_project_and_defaults_to_preview() {
        let id = Uuid::new_v4().to_string();
        let preview = GitDedupArgs::try_parse_from(["git-dedup", "--project", &id]).unwrap();
        assert!(!preview.apply);
        assert_eq!(preview.namespace, "local");
        let apply = GitDedupArgs::try_parse_from([
            "git-dedup",
            "--project",
            &id,
            "--apply",
            "--namespace",
            "actor:fixture",
        ])
        .unwrap();
        assert!(apply.apply);
        assert_eq!(apply.namespace, "actor:fixture");
        assert!(GitDedupArgs::try_parse_from(["git-dedup", "--project", "abcd1234"]).is_err());
        assert!(GitDedupArgs::try_parse_from(["git-dedup"]).is_err());
    }
}

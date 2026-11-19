//! Explicit repair of duplicate issue/PR notes for one canonical project.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use clap::Parser;
use uuid::Uuid;

use khive_mcp::serve::{resolve_runtime_config, RuntimeConfigInputs};
use khive_pack_git::dedup::{run_dedup, DedupOptions};
use khive_runtime::{IngestAuditStore, KhiveRuntime, Namespace, PackRegistry, RuntimeConfig};

#[derive(Parser, Debug)]
pub struct GitDedupArgs {
    /// Exact live canonical project UUID; prefixes are not accepted.
    #[arg(long)]
    pub project: Uuid,
    /// Leave every note whose `project_id` is this anchor untouched. Repeatable.
    #[arg(long = "refuse-anchor")]
    pub refuse_anchor: Vec<Uuid>,
    /// Apply the planned merges; omission only previews.
    #[arg(long)]
    pub apply: bool,
    #[arg(long, env = "KHIVE_DB")]
    pub db: Option<String>,
    #[arg(long, default_value = "local")]
    pub namespace: String,
}

/// The runtime embeds merged notes the way ingest does, so a survivor's vector
/// follows its merged body.
fn runtime_config(args: &GitDedupArgs) -> Result<RuntimeConfig> {
    let namespace =
        Namespace::parse(&args.namespace).map_err(|error| anyhow::anyhow!("{error}"))?;
    resolve_runtime_config(RuntimeConfigInputs {
        db: args.db.as_deref(),
        config: None,
        namespace,
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: false,
        packs: None,
        brain_profile: None,
    })
}

pub async fn run(args: GitDedupArgs) -> Result<()> {
    let runtime =
        KhiveRuntime::new(runtime_config(&args)?).map_err(|error| anyhow::anyhow!("{error}"))?;
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
            refused_anchors: args.refuse_anchor.iter().copied().collect::<BTreeSet<_>>(),
            apply: args.apply,
        },
    )
    .await?;
    for line in report.summary_lines() {
        eprintln!("{line}");
    }
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
        assert!(preview.refuse_anchor.is_empty());
        assert_eq!(preview.namespace, "local");
        assert!(GitDedupArgs::try_parse_from(["git-dedup", "--project", "abcd1234"]).is_err());
        assert!(GitDedupArgs::try_parse_from(["git-dedup"]).is_err());
    }

    #[test]
    fn dedup_accepts_repeated_full_anchor_ids_to_refuse() {
        let (project, one, two) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let args = GitDedupArgs::try_parse_from([
            "git-dedup",
            "--project",
            &project.to_string(),
            "--refuse-anchor",
            &one.to_string(),
            "--refuse-anchor",
            &two.to_string(),
            "--apply",
            "--namespace",
            "actor:fixture",
        ])
        .unwrap();
        assert!(args.apply);
        assert_eq!(args.namespace, "actor:fixture");
        assert_eq!(args.refuse_anchor, vec![one, two]);
        assert!(GitDedupArgs::try_parse_from([
            "git-dedup",
            "--project",
            &project.to_string(),
            "--refuse-anchor",
            "8fde4762",
        ])
        .is_err());
    }

    #[test]
    fn dedup_runtime_embeds_merged_notes() {
        let directory = tempfile::tempdir().unwrap();
        let db = directory.path().join("dedup.db");
        let args = GitDedupArgs::try_parse_from([
            "git-dedup",
            "--project",
            &Uuid::new_v4().to_string(),
            "--db",
            db.to_str().unwrap(),
        ])
        .unwrap();
        let config = runtime_config(&args).unwrap();
        assert!(config.embedding_model.is_some());
        assert_eq!(config.db_path.as_deref(), Some(db.as_path()));
    }
}

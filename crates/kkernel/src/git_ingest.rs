//! `kkernel git-ingest` — one-shot batch ingest of commit/issue/PR provenance
//! notes into the graph (ADR-088). Builds a `VerbRegistry` the same way
//! `khive-mcp`'s `KhiveMcpServer::with_packs` does (see `server.rs`), then
//! drives `khive_pack_git::ingest::run_ingest` against it. NOT a daemon loop,
//! NOT a webhook, NOT a poller — one pass per invocation.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use khive_mcp::serve::{resolve_runtime_config, RuntimeConfigInputs};
use khive_pack_git::ingest::{run_ingest, IngestOptions};
use khive_runtime::{KhiveRuntime, Namespace, PackRegistry, VerbRegistryBuilder};

/// Arguments for `kkernel git-ingest`.
#[derive(Parser, Debug)]
pub struct GitIngestArgs {
    /// Path to the local git repository to walk.
    #[arg(long)]
    pub repo: PathBuf,

    /// The repo-anchor `project` entity commits/issues/PRs annotate — full
    /// UUID or an 8+ hex prefix.
    #[arg(long)]
    pub project: String,

    /// Database path (defaults to `~/.khive/khive.db`).
    #[arg(long, env = "KHIVE_DB")]
    pub db: Option<String>,

    /// Namespace to operate in.
    #[arg(long, default_value = "local")]
    pub namespace: String,

    /// Print human-readable output instead of JSON.
    #[arg(long)]
    pub human: bool,
}

/// Run one `kkernel git-ingest` pass: resolve config, build a registry with
/// the configured pack set (which includes `git` by default), and dispatch
/// the ingester against it.
pub async fn run_git_ingest(args: GitIngestArgs) -> Result<()> {
    let ns = Namespace::parse(&args.namespace).map_err(|e| anyhow::anyhow!("{e}"))?;
    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db: args.db.as_deref(),
        config: None,
        namespace: ns,
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: false,
        packs: None,
        brain_profile: None,
    })?;

    let runtime = KhiveRuntime::new(cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
    let resolved_ns = runtime.config().default_namespace.clone();
    let token = runtime
        .authorize(resolved_ns)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to authorize namespace")?;

    // Mirrors `KhiveMcpServer::with_packs` (khive-mcp/src/server.rs): same
    // gate/namespace/visibility/actor wiring, built from the SAME
    // `runtime.config().packs` list a live server would use, so the ingester
    // observes identical `KindHook`/edge-rule/verb behavior.
    let mut builder = VerbRegistryBuilder::new();
    builder.with_gate(runtime.config().gate.clone());
    builder.with_default_namespace(runtime.config().default_namespace.as_str());
    builder.with_visible_namespaces(runtime.config().visible_namespaces.clone());
    builder.with_actor_id(runtime.config().actor_id.clone());
    PackRegistry::register_packs(
        &runtime.config().packs.clone(),
        runtime.clone(),
        &mut builder,
    )
    .map_err(|e| anyhow::anyhow!("pack registration failed: {e:?}"))?;
    let registry = builder.build().map_err(|e| anyhow::anyhow!("{e}"))?;
    runtime.install_edge_rules(registry.all_edge_rules());

    let report = run_ingest(
        &runtime,
        &token,
        &registry,
        IngestOptions::unbounded(args.repo, args.project),
    )
    .await?;

    if args.human {
        println!(
            "commits: {} ingested, {} skipped\nissues: {} ingested, {} skipped\nprs: {} ingested, {} skipped\ngh_available: {}",
            report.commits_ingested,
            report.commits_skipped_existing,
            report.issues_ingested,
            report.issues_skipped_existing,
            report.prs_ingested,
            report.prs_skipped_existing,
            report.gh_available,
        );
        for w in &report.warnings {
            eprintln!("warning: {w}");
        }
    } else {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }
    Ok(())
}

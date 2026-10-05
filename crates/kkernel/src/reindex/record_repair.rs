//! Single-record mode for the existing reindex command.

use anyhow::{ensure, Result};
use khive_runtime::{KhiveRuntime, NamespaceToken};
use uuid::Uuid;

use super::ReindexArgs;

pub(super) fn validate_args(args: &ReindexArgs) -> Result<()> {
    if args.id.is_some() {
        ensure!(
            args.model.is_none()
                && args.batch_size == 128
                && !args.knowledge_only
                && !args.no_sections
                && !args.sections_only
                && !args.rebuild_fts,
            "--id cannot be combined with --model, --batch-size, --knowledge-only, --no-sections, --sections-only, or --rebuild-fts"
        );
    }
    Ok(())
}

pub(super) async fn run(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    human: bool,
    best_effort: bool,
) -> Result<()> {
    let report = runtime.repair_record_indexes(token, id).await?;
    if human {
        println!(
            "Index repair for {} ({}, {}):",
            report.id, report.substrate, report.namespace
        );
        if report.repaired.is_empty() && report.failures.is_empty() {
            println!("Nothing to repair.");
        }
        for stage in &report.repaired {
            println!("Repaired: {stage}");
        }
        for failure in &report.failures {
            println!("Failed ({}): {}", failure.stage, failure.error);
        }
    } else {
        println!("{}", serde_json::to_string(&report)?);
    }
    // Emit the complete stage report before returning failure: successful
    // stages remain committed even when another model could not be repaired.
    if !report.failures.is_empty() {
        if best_effort {
            eprintln!("warning: single-record index repair remains incomplete");
        } else {
            anyhow::bail!(
                "single-record index repair remains incomplete; inspect failures in the report"
            );
        }
    }
    Ok(())
}

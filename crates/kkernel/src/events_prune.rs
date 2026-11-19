//! `kkernel db events-prune`: the operator command for `khive_db::events_prune` (ADR-212 §4).
//!
//! One invocation runs one or more phases, in order: the plan (always), the backup (`--backup-to`) and the apply
//! (`--apply`). Without `--backup-to` or `--apply` it is a dry run that writes nothing anywhere. stdout carries one
//! JSON document with a member per phase that ran; stderr carries a short table and, on failure, one line that names
//! the outcome. Exit codes: 0 done, 3 refused-schema, 4 digest-mismatch, 5 no-backup, 6 control-mismatch,
//! 7 interrupted-resumable (rerun the same command to continue), 1 anything else.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use khive_db::events_prune::{self, ApplyOptions, Plan, PruneError, PruneTarget};

/// Arguments of `kkernel db events-prune`.
#[derive(clap::Parser, Debug)]
pub struct EventsPruneArgs {
    /// The events store to prune (the `.events.db` file beside a main store).
    #[arg(long)]
    pub events_db: PathBuf,

    /// A store whose graph edges are checked for references to a candidate, opened read-only. Repeat it for every
    /// store that holds edges.
    #[arg(long = "edges-db", required = true)]
    pub edges_db: Vec<PathBuf>,

    /// Select rows created before this instant (microseconds since the Unix epoch) instead of seven days before
    /// now. Pass the dry run's `cutoff_us` to the backup and the apply so they select the same set. A cutoff later
    /// than seven days before now is refused.
    #[arg(long)]
    pub cutoff_us: Option<i64>,

    /// The candidate-set digest from the reviewed dry run. The backup and the apply refuse a set with another digest.
    #[arg(long)]
    pub digest: Option<String>,

    /// Copy the events store to this new file through the SQLite backup API, verify the copy, and write
    /// `<path>.receipt.json` beside it.
    #[arg(long)]
    pub backup_to: Option<PathBuf>,

    /// Delete the selected rows. Requires --digest, --cutoff-us, --state-dir, and a backup: --backup-receipt, or
    /// --backup-to in the same invocation.
    #[arg(long)]
    pub apply: bool,

    /// The receipt of a backup taken for this digest and cutoff.
    #[arg(long)]
    pub backup_receipt: Option<PathBuf>,

    /// Directory for the apply's checkpoint and expended-ID file. Rerunning with the same directory resumes.
    #[arg(long)]
    pub state_dir: Option<PathBuf>,

    /// Rows per delete transaction (at most 10,000).
    #[arg(long, default_value_t = events_prune::DEFAULT_BATCH_ROWS)]
    pub batch_rows: usize,

    /// Pause between delete transactions, in milliseconds.
    #[arg(long, default_value_t = 200)]
    pub pause_ms: u64,

    /// How long one delete transaction waits for the write lock before the batch is retried later, in milliseconds.
    #[arg(long, default_value_t = 2_000)]
    pub busy_timeout_ms: u64,

    /// Stop after this many committed batches; the run exits 7 and the same command resumes it.
    #[arg(long)]
    pub max_batches: Option<usize>,
}

fn table(plan: &Plan) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "events-prune plan ({}): cutoff_us {} ({} s before start {}), {} candidates of {} rows, digest {}\n",
        plan.classifier,
        plan.cutoff_us,
        (plan.run_started_us - plan.cutoff_us) / 1_000_000,
        plan.run_started_us,
        plan.candidates,
        plan.total_rows,
        plan.digest
    ));
    for (verb, buckets) in &plan.candidates_by_verb_and_age {
        let cells: Vec<String> = ["7-14d", "14-30d", "30-90d", ">90d"]
            .iter()
            .map(|bucket| format!("{bucket} {}", buckets.get(*bucket).copied().unwrap_or(0)))
            .collect();
        out.push_str(&format!("  {verb:<34} {}\n", cells.join("  ")));
    }
    let retained: Vec<String> = plan
        .retained_by_reason
        .iter()
        .map(|(reason, count)| format!("{reason} {count}"))
        .collect();
    out.push_str(&format!(
        "  retained: {} (protected backlog {})\n",
        if retained.is_empty() {
            "none".to_string()
        } else {
            retained.join(", ")
        },
        plan.protected_backlog
    ));
    out.push_str(&format!(
        "  store: {} bytes, wal {} bytes, page_size {}, page_count {}, freelist {}, auto_vacuum {}\n",
        plan.size.db_bytes,
        plan.size.wal_bytes,
        plan.size.pages.page_size,
        plan.size.pages.page_count,
        plan.size.pages.freelist_count,
        plan.size.pages.auto_vacuum
    ));
    out.push_str(&format!(
        "  non-candidate keys: {} (full list in the JSON)\n",
        plan.non_candidate_keys.len()
    ));
    out
}

fn run_phases(
    args: &EventsPruneArgs,
    output: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<(), PruneError> {
    let target = PruneTarget {
        events_db: args.events_db.clone(),
        edge_dbs: args.edges_db.clone(),
    };
    if args.apply {
        let missing: Vec<&str> = [
            ("--digest", args.digest.is_none()),
            ("--cutoff-us", args.cutoff_us.is_none()),
            ("--state-dir", args.state_dir.is_none()),
        ]
        .iter()
        .filter(|(_, absent)| *absent)
        .map(|(flag, _)| *flag)
        .collect();
        if !missing.is_empty() {
            return Err(PruneError::Invalid(format!(
                "--apply requires {}",
                missing.join(", ")
            )));
        }
        if args.backup_to.is_none() && args.backup_receipt.is_none() {
            return Err(PruneError::NoBackup(
                "--apply requires --backup-receipt or --backup-to".into(),
            ));
        }
    }
    let now = chrono::Utc::now().timestamp_micros();
    let cutoff = args
        .cutoff_us
        .unwrap_or(now - events_prune::HORIZON_SECONDS * 1_000_000);
    let plan = events_prune::plan_at(&target, now, cutoff)?;
    eprint!("{}", table(&plan));
    output.insert("plan".into(), serde_json::to_value(&plan)?);
    if let Some(expected) = args.digest.as_deref() {
        if expected != plan.digest && (args.backup_to.is_some() || !args.apply) {
            return Err(PruneError::DigestMismatch {
                expected: expected.to_string(),
                actual: plan.digest.clone(),
                detail: format!("{} selected now", plan.candidates),
            });
        }
    }
    let mut receipt_path = args.backup_receipt.clone();
    if let Some(dest) = args.backup_to.as_deref() {
        let receipt = events_prune::backup(&target, &plan, dest)?;
        eprintln!(
            "events-prune backup: {} ({} bytes, sha256 {}), integrity {:?}",
            receipt.backup_path.display(),
            receipt.bytes,
            receipt.sha256,
            receipt.integrity
        );
        output.insert("backup".into(), serde_json::to_value(&receipt)?);
        receipt_path = Some(events_prune::receipt_path(dest));
    }
    if args.apply {
        let options = ApplyOptions {
            digest: args.digest.clone().unwrap_or_default(),
            cutoff_us: cutoff,
            backup_receipt: receipt_path.unwrap_or_default(),
            state_dir: args.state_dir.clone().unwrap_or_default(),
            batch_rows: args.batch_rows,
            pause: Duration::from_millis(args.pause_ms),
            busy_timeout: Duration::from_millis(args.busy_timeout_ms),
            max_batches: args.max_batches,
        };
        let report = events_prune::apply(&target, &options)?;
        eprintln!(
            "events-prune apply: removed {} this run ({} total), {} batches, skipped {:?}; bytes {} -> {}, freelist {} -> {}",
            report.removed,
            report.removed_total,
            report.batches,
            report.skipped,
            report.size_before.db_bytes,
            report.size_after.db_bytes,
            report.size_before.pages.freelist_count,
            report.size_after.pages.freelist_count
        );
        output.insert("apply".into(), serde_json::to_value(&report)?);
    }
    Ok(())
}

/// Run `kkernel db events-prune`. A failure with its own exit code ends the process with that code after the JSON
/// for the phases that completed is printed.
pub async fn run(args: EventsPruneArgs) -> Result<()> {
    let (output, outcome) = tokio::task::spawn_blocking(move || {
        let mut output = serde_json::Map::new();
        let outcome = run_phases(&args, &mut output);
        (output, outcome)
    })
    .await?;
    let mut document = output;
    if let Err(error) = &outcome {
        document.insert(
            "error".into(),
            serde_json::json!({"exit_code": error.exit_code(), "message": error.to_string()}),
        );
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::Value::Object(document))?
    );
    match outcome {
        Ok(()) => Ok(()),
        Err(error) => {
            eprintln!("events-prune: {error}");
            let code = error.exit_code();
            if code == 1 {
                anyhow::bail!("events-prune failed");
            }
            use std::io::Write as _;
            std::io::stdout().flush()?;
            std::process::exit(code);
        }
    }
}

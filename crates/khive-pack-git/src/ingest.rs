//! Batch, cursor-based git-history ingester (ADR-088 §5). One-shot: walks
//! local git history plus (optionally) `gh`-fetched issues and pull
//! requests, and writes `commit` / `issue` / `pull_request` notes through
//! the standard `create` verb. See crates/khive-pack-git/docs/api/ingest.md
//! and crates/khive-pack-git/docs/ingest.md for the full design notes.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use khive_runtime::{secret_gate, KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_storage::types::{SqlStatement, SqlValue};

use crate::hook;
use crate::refs;
use crate::source::remote_url_to_slug;

/// Which record kinds a `run_ingest` pass processes. `Default` selects all
/// three — the CLI's historical behavior and the `git.digest` verb's default
/// (ADR-088 Amendment 1).
#[derive(Debug, Clone, Copy)]
pub struct IngestInclude {
    pub commits: bool,
    pub issues: bool,
    pub pull_requests: bool,
}

impl Default for IngestInclude {
    fn default() -> Self {
        Self {
            commits: true,
            issues: true,
            pull_requests: true,
        }
    }
}

/// Options for one ingest pass.
#[derive(Debug, Clone)]
pub struct IngestOptions {
    /// Local path to the git repository to walk.
    pub repo: PathBuf,
    /// Expected GitHub `owner/repo` derived from the caller's canonical
    /// remote source. Local-path and administrative callers leave this
    /// unset, and the ingest core derives the same identity from the
    /// checkout's configured `origin`. The value is an identity constraint,
    /// never a capability hint: `gh` is always invoked with it explicitly.
    pub expected_github_repo: Option<String>,
    /// The repo-anchor `project` entity — full UUID or an 8+ hex prefix.
    pub project: String,
    /// Bounded work per call, counted across commits + issues + PRs
    /// (ADR-088 Amendment 1). `None` means unbounded — the CLI's historical
    /// one-shot behavior.
    pub max_items: Option<u64>,
    /// Which record kinds to ingest this pass.
    pub include: IngestInclude,
}

impl IngestOptions {
    /// Convenience constructor for callers that want the CLI's historical
    /// unbounded, all-kinds behavior.
    pub fn unbounded(repo: PathBuf, project: String) -> Self {
        Self {
            repo,
            expected_github_repo: None,
            project,
            max_items: None,
            include: IngestInclude::default(),
        }
    }
}

/// Bounds new-record creation attempts across a `run_ingest` pass. See
/// crates/khive-pack-git/docs/api/ingest.md#budget.
struct Budget {
    remaining: Option<u64>,
}

impl Budget {
    fn try_consume(&mut self) -> bool {
        match &mut self.remaining {
            None => true,
            Some(0) => false,
            Some(n) => {
                *n -= 1;
                true
            }
        }
    }

    fn exhausted(&self) -> bool {
        matches!(self.remaining, Some(0))
    }
}

/// A newly created note this pass, for `link_references`'s
/// same-pass cross-reference resolution. See
/// crates/khive-pack-git/docs/api/ingest.md#newrecordforref.
struct NewRecordForRef {
    id: Uuid,
    text: String,
}

/// Caller-visible detail for one content write the secret gate refused.
///
/// The record key is a trusted natural key (commit SHA or GitHub number),
/// and the secret itself is represented only by [`secret_gate::SecretMatch`]'s
/// detector name and masked excerpt. The rejected content is never copied
/// into the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IngestWriteRefusal {
    pub verb: String,
    pub record_kind: String,
    pub record_key: String,
    pub detector: String,
    pub masked: String,
}

/// Machine-readable state of one ingest source (`commits`, `issues`,
/// `pull_requests`) after a pass — issue #1617. A reader no longer has to
/// infer coverage from `done` (a budget-cursor statement) or parse prose
/// out of `warnings[]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub enum IngestSourceState {
    /// The source was walked to the end of its history this pass.
    Completed,
    /// The source was visited but not exhausted: the budget ran out, a
    /// paging window was left incomplete, or a per-record write failure
    /// froze the cursor (`cursor_stalled`). The reason names the cause.
    StoppedEarly(String),
    /// The source was never walked this pass. The reason names the cause:
    /// the budget was already exhausted before the source was reached, or
    /// the source-bound `gh repo view` probe could not resolve an authenticated
    /// GitHub repository for this checkout, or `gh` itself failed before the
    /// walk began. A local cursor/database read failure before remote listing
    /// is also `Skipped`, with a distinct local-failure reason. A failure
    /// after the walk began is `StoppedEarly`, never `Skipped`.
    Skipped(String),
}

/// Per-source ingest coverage for one pass (issue #1617): which sources
/// were walked to completion, which stopped early and why, and which were
/// never reached. Written by the walk paths themselves, not reconstructed
/// at report time, so the states stay truthful when a new walk path is
/// added later.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct IngestSourceStatus {
    pub commits: Option<IngestSourceState>,
    pub issues: Option<IngestSourceState>,
    pub pull_requests: Option<IngestSourceState>,
}

/// Outcome of one ingest pass. Serializable so CLI callers can emit it as JSON.
#[derive(Debug, Default, Serialize)]
pub struct IngestReport {
    /// Durable audit-event receipt for this exact successful `git.digest`
    /// response. The runtime dispatch seam fills this after the ingest report
    /// has been serialized and before returning it to the caller; direct
    /// `run_ingest` users do not receive a receipt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt_id: Option<String>,
    pub commits_ingested: u64,
    pub commits_skipped_existing: u64,
    pub issues_ingested: u64,
    pub issues_skipped_existing: u64,
    pub prs_ingested: u64,
    pub prs_skipped_existing: u64,
    /// `Some(false)` when the source-bound `gh repo view` probe ran but could
    /// not resolve an authenticated GitHub repository — issues/PRs were
    /// skipped but commits still ingested (ADR-088 §5 graceful-absence rule).
    /// `Some(true)` only when that probe returned a usable `owner/repo`, which
    /// is then passed explicitly to every `gh ... list --repo` call. `None`
    /// when this pass never probed: the probe runs only when `include`
    /// requests issues or pull requests, and a commits-only pass says nothing
    /// about `gh` either way (issue #1645).
    pub gh_available: Option<bool>,
    pub warnings: Vec<String>,
    /// Number of per-record content writes refused by the runtime secret gate
    /// during this pass. Callers can assert this is zero independently of
    /// unrelated warnings.
    pub writes_refused: u64,
    /// Safe structured details for every entry counted by `writes_refused`.
    /// Each item names the refused verb and record without echoing the
    /// rejected content.
    pub write_refusals: Vec<IngestWriteRefusal>,
    /// `false` when `max_items` was exhausted before this pass reached the
    /// end of every included kind's history — callers loop until `true`
    /// (ADR-088 Amendment 1). Always `true` for an unbounded
    /// (`max_items: None`) pass.
    pub done: bool,
    /// `true` when a per-record write failure pinned the commits cursor at
    /// the last contiguous success this pass. The failed record is retried
    /// (and its warning re-surfaced) on every subsequent pass until it is
    /// fixed upstream. `done` is forced `false` whenever this is set: commits
    /// beyond the stall were still attempted this pass, but history cannot be
    /// called exhausted while the cursor cannot advance (issue #1645).
    pub cursor_stalled: bool,
    /// The repo-anchor `project` entity id this pass resolved (or the
    /// verb-level caller created).
    pub project_id: Option<String>,
    /// `true` when the `git.digest` verb auto-created the `project` anchor
    /// because none was found (ADR-088 Amendment 1) — never set by
    /// `run_ingest` itself, only by the verb handler after it returns.
    pub project_created: bool,
    /// `true` when `project_created` fired over a soft-deleted anchor that
    /// still had a live corpus annotating it (issue #1173) — the resolved
    /// repo identity matched no LIVE `project` entity, but did match a
    /// soft-deleted one with `annotates` edges still pointing at it. This
    /// distinguishes "first ingest of a new repo" from "anchor lost,
    /// corpus about to duplicate under a fresh anchor" — never set by
    /// `run_ingest` itself, only by the verb handler after it returns.
    pub orphaned_corpus_detected: bool,
    /// The dangling anchor id `orphaned_corpus_detected` was found under.
    /// `None` unless `orphaned_corpus_detected` is `true`.
    pub orphaned_project_id: Option<String>,
    /// Count of `commit`/`issue`/`pull_request` notes still `annotates`-
    /// linked to `orphaned_project_id`. `0` unless `orphaned_corpus_detected`
    /// is `true`.
    pub orphaned_note_count: u64,
    /// `annotates` edges created from a `Closes/Fixes/Resolves #N` or bare
    /// `#N` reference in a commit message or issue/PR body to the referenced
    /// issue/PR note (ADR-088 Amendment 1 ingest enrichment).
    pub reference_edges_created: u64,
    /// References that named a number this pass could not resolve to an
    /// ingested issue/PR note within the same project — skipped, not an
    /// error (fail-open).
    pub reference_edges_unresolved: u64,
    /// `precedes` edges created from a commit's `parents[]` to the commit
    /// itself (ADR-088 Amendment 1 ingest enrichment).
    pub parent_edges_created: u64,
    /// Commits whose masked content exceeded `MAX_COMMIT_EMBED_BYTES`: the
    /// full commit note was stored and FTS-indexed unchanged, but the vector
    /// embedding input was truncated to a UTF-8-safe head prefix at the cap
    /// (issue #764). Only incremented for successfully created commits.
    pub commit_embeddings_truncated: u64,
    /// Total `commit` notes annotating this pass's project in the database
    /// AFTER this pass completes — a row count, not an in-memory delta
    /// (issue #1045). `commits_ingested` only counts creations THIS process
    /// observed; if the daemon respawns mid-digest and drops a response
    /// after the write already landed, that response's `commits_ingested`
    /// is lost from any cumulative sum a caller keeps across calls, even
    /// though the row was durably written. This field is derived by
    /// querying the DB after the walk, so it survives that loss and is safe
    /// to compare directly against an independent source of truth (e.g.
    /// `git rev-list --count <ref>`).
    pub commits_total_in_db: u64,
    /// Touched paths the `--name-only` pass recorded but `changed_paths`
    /// storage cannot carry: valid Unix filenames that violate the hook's
    /// canonical shape (a `\` byte, an `X:` drive prefix, a leading `/`,
    /// or an empty/`.`/`..` component). Only actual predicate rejections
    /// count — dedup/post-masking collisions in the stored array are not
    /// drops. Dropped before create rather than failing the whole commit —
    /// see
    /// crates/khive-pack-git/docs/api/ingest.md#changed-paths-and-code-module-annotations.
    pub changed_paths_filtered_noncanonical: u64,
    /// Changed paths whose `(source_revision, source_path)` module binding
    /// was unusable and therefore received no code-module annotation. Two
    /// shapes fold into this counter: an ambiguous key (two or more live
    /// rows, whether two or more have parseable ids or at most one has a
    /// parseable id) and the single-row sub-case whose one row's id does not
    /// parse (not ambiguous — just no bindable candidate). Counted only when
    /// an ingested commit's path actually hits the key, so unusable keys
    /// untouched by this pass never inflate the count.
    pub code_module_ambiguous_path_skips: u64,
    /// Per-source ingest coverage for this pass (issue #1617): each
    /// included source reports `completed`, `stopped_early { reason }`, or
    /// `skipped { reason }`, so a gate refusal, budget exhaustion, and a
    /// `gh`/remote skip are distinguishable without parsing `warnings[]`.
    /// Additive companion to `done`/`cursor_stalled`, which keep their
    /// existing budget-cursor meaning.
    pub sources: IngestSourceStatus,
    /// `true` only when every included source was walked to the end of its
    /// history this pass — "silence means nothing left", as opposed to
    /// "stopped before the end" (budget exhausted, incomplete paging
    /// window, or a frozen cursor). Unlike `done`, this is a coverage
    /// statement, not a resume-loop signal (issue #1617). Vacuously `true`
    /// when `include` is empty: no source was requested, so nothing can
    /// count against it — it is a statement about the REQUESTED sources,
    /// not about the repository.
    pub history_exhausted: bool,
}

fn record_write_failure(
    report: &mut IngestReport,
    verb: &str,
    record_kind: &str,
    record_key: String,
    error: RuntimeError,
) {
    if let RuntimeError::SecretDetected(secret) = &error {
        report.writes_refused += 1;
        report.write_refusals.push(IngestWriteRefusal {
            verb: verb.to_string(),
            record_kind: record_kind.to_string(),
            record_key: record_key.clone(),
            detector: secret.detector.to_string(),
            masked: secret.masked.clone(),
        });
    }
    report
        .warnings
        .push(format!("{verb} {record_kind} {record_key}: {error}"));
}

/// Overwrite a walker's source slot with `StoppedEarly(reason)` at an
/// end-of-walk arm. See
/// crates/khive-pack-git/docs/ingest.md#seed-invariant-for-pin_stopped_early.
fn pin_stopped_early(slot: &mut Option<IngestSourceState>, reason: String) {
    match slot {
        Some(state) => *state = IngestSourceState::StoppedEarly(reason),
        None => {
            debug_assert!(
                false,
                "walker reached an end-of-walk arm without seeding its source state"
            );
            *slot = Some(IngestSourceState::StoppedEarly(reason));
        }
    }
}

/// Run one ingest pass over `opts.repo`: issues + PRs first (via `gh`, when
/// available), then commits (via local `git log`), each bounded by
/// `opts.max_items` and cursor-resumable (call again while the returned
/// `IngestReport.done` is `false`). Returns an error only for a failure that
/// aborts the whole pass (e.g. an unresolvable `opts.project`); per-record
/// failures are collected in `IngestReport.warnings` instead. See
/// crates/khive-pack-git/docs/api/ingest.md#run_ingest_with_commit_recovery for
/// why this has no self-healing recovery (unlike the verb-handler path).
pub async fn run_ingest(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &VerbRegistry,
    opts: IngestOptions,
) -> Result<IngestReport> {
    run_ingest_with_commit_recovery(runtime, token, registry, opts, |_repo, _err| Ok(None)).await
}

/// Same one-shot ingest pass as `run_ingest`, but a classified
/// missing-promisor-object failure is retried through `recover` (issue
/// #765) instead of aborting the whole pass. See
/// crates/khive-pack-git/docs/api/ingest.md#run_ingest_with_commit_recovery.
pub(crate) async fn run_ingest_with_commit_recovery(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &VerbRegistry,
    opts: IngestOptions,
    mut recover: impl FnMut(&Path, &GitLogError) -> Result<Option<RecoveredRepo>> + Send,
) -> Result<IngestReport> {
    let mut report = IngestReport {
        done: true,
        ..IngestReport::default()
    };

    let project_id = resolve_id(runtime, token, &opts.project)
        .await?
        .ok_or_else(|| anyhow!("--project {:?} did not resolve to an entity", opts.project))?;
    report.project_id = Some(project_id.to_string());

    let mut merge_sha_to_pr: HashMap<String, Uuid> = HashMap::new();
    let mut number_to_pr: HashMap<u64, Uuid> = HashMap::new();
    let mut budget = Budget {
        remaining: opts.max_items,
    };
    let mut new_records: Vec<NewRecordForRef> = Vec::new();
    // Per-source completion flags folded into `report.sources` at the end of
    // the pass (issue #1617). The flags are seeded by the failure/walk paths
    // themselves (`ingest_commits`/`ingest_prs`/`ingest_issues` set them
    // false on a budget break, incomplete paging window, or frozen cursor),
    // never reconstructed from counts at report time.
    let mut commits_complete = false;
    let mut prs_complete = false;
    let mut issues_complete = false;
    // Set when a gh walker returned Err AFTER recording a walk state
    // (walked-then-failed): the source states stay as the walker left them
    // (possibly `completed`), but the pass itself failed, so
    // `history_exhausted` must not claim total coverage on their strength
    // alone.
    let mut gh_walk_failed_after_walk = false;

    // Graceful degradation covers both "gh is not on PATH" and "gh is present
    // but cannot resolve an authenticated GitHub repository for this checkout"
    // (e.g. a non-GitHub or local-only repo). Either way, requested issues/PRs
    // are skipped with a structured reason and commits still ingest (ADR-088
    // §5). The successful probe returns the exact owner/repo later passed via
    // `--repo`, so PATH presence is never mistaken for remote usability.
    if opts.include.issues || opts.include.pull_requests {
        let gh_probe = probe_gh_repository(&opts.repo, opts.expected_github_repo.as_deref());
        if let Ok(gh_repo) = &gh_probe {
            report.gh_available = Some(true);
            if opts.include.pull_requests && !budget.exhausted() {
                match ingest_prs(
                    runtime,
                    token,
                    registry,
                    &opts.repo,
                    gh_repo,
                    project_id,
                    &mut report,
                    &mut merge_sha_to_pr,
                    &mut number_to_pr,
                    &mut budget,
                    &mut new_records,
                    &mut prs_complete,
                )
                .await
                {
                    Ok(()) => {}
                    Err(e) => {
                        // Distinguishes a never-walked source (first-page
                        // failure) from walked-then-failed. See
                        // crates/khive-pack-git/docs/ingest.md#walker-exit-states.
                        let walked = matches!(
                            report.sources.pull_requests.as_ref(),
                            Some(IngestSourceState::Completed | IngestSourceState::StoppedEarly(_))
                        );
                        if walked {
                            // Nothing was "skipped" once the walk began — name
                            // how far it got before the pass failed.
                            let visited = report.prs_ingested + report.prs_skipped_existing;
                            let noun = if visited == 1 { "record" } else { "records" };
                            report.warnings.push(format!(
                                "gh pr list failed after walking {visited} {noun} — \
                                 stopped early, not skipped: {e}"
                            ));
                        } else if report.sources.pull_requests.is_none() {
                            report
                                .warnings
                                .push(format!("gh pr list failed, skipping pull requests: {e}"));
                        } else {
                            report.warnings.push(format!(
                                "pull request ingest failed before remote listing; local source failure retained: {e}"
                            ));
                        }
                        if !walked && report.sources.pull_requests.is_none() {
                            report.sources.pull_requests = Some(IngestSourceState::Skipped(
                                format!("gh pr list failed: {e}"),
                            ));
                        } else if walked {
                            match report.sources.pull_requests.as_mut() {
                                Some(IngestSourceState::StoppedEarly(reason)) => {
                                    reason.push_str(&format!(
                                        "; pass then failed after the walk: {e}"
                                    ));
                                }
                                // A completed walk whose pass then fails is
                                // downgraded to stopped-early so `completed`
                                // never outlives an unproven pass.
                                Some(state @ IngestSourceState::Completed) => {
                                    *state = IngestSourceState::StoppedEarly(format!(
                                        "walk completed but the pass then failed: {e}"
                                    ));
                                }
                                Some(IngestSourceState::Skipped(_)) | None => {}
                            }
                            // Walked-then-failed leaves durability unproven,
                            // so the resume loop must not treat the source as
                            // finished; a first-fetch failure pins nothing.
                            report.done = false;
                            gh_walk_failed_after_walk = true;
                        }
                        prs_complete = false;
                    }
                }
            } else if opts.include.pull_requests {
                report.sources.pull_requests = Some(IngestSourceState::Skipped(
                    "budget exhausted before pull requests were reached".to_string(),
                ));
            }
            if opts.include.issues && !budget.exhausted() {
                if let Err(e) = ingest_issues(
                    runtime,
                    token,
                    registry,
                    &opts.repo,
                    gh_repo,
                    project_id,
                    &mut report,
                    &mut budget,
                    &mut new_records,
                    &mut issues_complete,
                )
                .await
                {
                    // See the PR arm above: "skipping" is only accurate for
                    // a first-fetch failure; a walked-then-failed source
                    // names how far the walk got.
                    let walked = matches!(
                        report.sources.issues.as_ref(),
                        Some(IngestSourceState::Completed | IngestSourceState::StoppedEarly(_))
                    );
                    if walked {
                        let visited = report.issues_ingested + report.issues_skipped_existing;
                        let noun = if visited == 1 { "record" } else { "records" };
                        report.warnings.push(format!(
                            "gh issue list failed after walking {visited} {noun} — \
                             stopped early, not skipped: {e}"
                        ));
                    } else if report.sources.issues.is_none() {
                        report
                            .warnings
                            .push(format!("gh issue list failed, skipping issues: {e}"));
                    } else {
                        report.warnings.push(format!(
                            "issue ingest failed before remote listing; local source failure retained: {e}"
                        ));
                    }
                    // See the PR arm above: `Skipped` only when the walk
                    // never began (the walker signals that by leaving its
                    // state unset); a walked-then-failed source keeps the
                    // state the walker already wrote.
                    if !walked && report.sources.issues.is_none() {
                        report.sources.issues = Some(IngestSourceState::Skipped(format!(
                            "gh issue list failed: {e}"
                        )));
                    } else if walked {
                        match report.sources.issues.as_mut() {
                            Some(IngestSourceState::StoppedEarly(reason)) => {
                                reason.push_str(&format!("; pass then failed after the walk: {e}"));
                            }
                            // See the PR arm above: a completed walk whose
                            // pass then fails is downgraded to
                            // stopped-early so the failure is never
                            // invisible next to a `completed` claim.
                            Some(state @ IngestSourceState::Completed) => {
                                *state = IngestSourceState::StoppedEarly(format!(
                                    "walk completed but the pass then failed: {e}"
                                ));
                            }
                            Some(IngestSourceState::Skipped(_)) | None => {}
                        }
                        // See the PR arm above: walked-then-failed is not a
                        // finished source; a first-fetch failure pins
                        // nothing.
                        report.done = false;
                        gh_walk_failed_after_walk = true;
                    }
                    issues_complete = false;
                }
            } else if opts.include.issues {
                report.sources.issues = Some(IngestSourceState::Skipped(
                    "budget exhausted before issues were reached".to_string(),
                ));
            }
        } else {
            let reason = gh_probe
                .as_ref()
                .expect_err("successful probe handled in the preceding branch");
            report.gh_available = Some(false);
            report.warnings.push(format!(
                "{reason}; skipped requested GitHub sources — commits still ingest"
            ));
            if opts.include.pull_requests {
                report.sources.pull_requests = Some(IngestSourceState::Skipped(reason.to_string()));
            }
            if opts.include.issues {
                report.sources.issues = Some(IngestSourceState::Skipped(reason.to_string()));
            }
        }
    }

    if opts.include.commits && !budget.exhausted() {
        match ingest_commits(
            runtime,
            token,
            registry,
            &opts.repo,
            project_id,
            &merge_sha_to_pr,
            &number_to_pr,
            &mut report,
            &mut budget,
            &mut new_records,
            &mut recover,
            &mut commits_complete,
        )
        .await
        {
            Ok(()) => {}
            Err(e) => {
                // A `Some` slot beside an Err means the walk ran and the pass
                // failed after it; a `None` slot is a pre-walk hard error. See
                // crates/khive-pack-git/docs/ingest.md#walker-exit-states.
                if report.sources.commits.is_some() {
                    match report.sources.commits.as_mut() {
                        Some(IngestSourceState::StoppedEarly(reason)) => {
                            reason.push_str(&format!("; pass then failed after the walk: {e}"));
                        }
                        // See the gh arms above: a completed walk whose pass
                        // then fails is downgraded so the failure is never
                        // invisible next to a `completed` claim.
                        Some(state @ IngestSourceState::Completed) => {
                            *state = IngestSourceState::StoppedEarly(format!(
                                "walk completed but the pass then failed: {e}"
                            ));
                        }
                        Some(IngestSourceState::Skipped(_)) | None => {}
                    }
                    report
                        .warnings
                        .push(format!("commit ingest failed after the walk: {e}"));
                    report.done = false;
                    commits_complete = false;
                } else {
                    return Err(e);
                }
            }
        }
    } else if opts.include.commits {
        report.sources.commits = Some(IngestSourceState::Skipped(
            "budget exhausted before commits were reached".to_string(),
        ));
    }

    if budget.exhausted() {
        report.done = false;
    }

    // Release-mode belt-and-braces fallback for a walker that returned
    // without recording its source state — an instrumentation gap, not a
    // normal stop. See crates/khive-pack-git/docs/ingest.md#walker-exit-states
    // for the fabrication risk this guards against.
    if report.sources.pull_requests.is_none() && opts.include.pull_requests {
        debug_assert!(
            false,
            "pull_requests walker returned without recording its source state"
        );
        report.sources.pull_requests = Some(if prs_complete {
            IngestSourceState::Completed
        } else {
            IngestSourceState::StoppedEarly(
                "state not recorded by walker; stopped before the PR history was exhausted".into(),
            )
        });
    }
    if report.sources.issues.is_none() && opts.include.issues {
        debug_assert!(
            false,
            "issues walker returned without recording its source state"
        );
        report.sources.issues = Some(if issues_complete {
            IngestSourceState::Completed
        } else {
            IngestSourceState::StoppedEarly(
                "state not recorded by walker; stopped before the issue history was exhausted"
                    .into(),
            )
        });
    }
    if report.sources.commits.is_none() && opts.include.commits {
        debug_assert!(
            false,
            "commits walker returned without recording its source state"
        );
        report.sources.commits = Some(if commits_complete {
            IngestSourceState::Completed
        } else {
            IngestSourceState::StoppedEarly(
                "state not recorded by walker; stopped before the commit history was exhausted"
                    .into(),
            )
        });
    }
    // A source left `None` was not requested by `include`, so it cannot
    // count against exhaustion.
    report.history_exhausted = !gh_walk_failed_after_walk
        && [
            &report.sources.commits,
            &report.sources.issues,
            &report.sources.pull_requests,
        ]
        .into_iter()
        .all(|s| {
            s.as_ref()
                .is_none_or(|state| matches!(state, IngestSourceState::Completed))
        });

    link_references(
        runtime,
        token,
        registry,
        project_id,
        &new_records,
        &mut report,
    )
    .await;

    // Derived fresh from the database on every pass (regardless of which
    // branch above ran, or whether this pass ingested anything new), so it
    // stays a truthful completeness signal across daemon restarts — see
    // `IngestReport::commits_total_in_db` (issue #1045).
    report.commits_total_in_db = count_commit_notes_for_project(runtime, token, project_id).await?;

    Ok(report)
}

/// Resolve a full UUID or an 8+ hex prefix to a full UUID, unfiltered by
/// namespace.
async fn resolve_id(
    runtime: &KhiveRuntime,
    _token: &NamespaceToken,
    raw: &str,
) -> Result<Option<Uuid>> {
    if let Ok(u) = Uuid::parse_str(raw) {
        return Ok(Some(u));
    }
    runtime
        .resolve_prefix_unfiltered(raw)
        .await
        .map_err(|e| anyhow!("{e}"))
}

/// Resolve `raw` (a full UUID or an 8+ hex prefix) to an existing `project`
/// entity id, unfiltered by namespace. Returns `Ok(None)` when no entity
/// matches; never creates one. Used by the `git.digest` verb handler to
/// resolve an explicitly supplied `project` argument.
pub async fn resolve_project_id(runtime: &KhiveRuntime, raw: &str) -> Result<Option<Uuid>> {
    if let Ok(u) = Uuid::parse_str(raw) {
        return Ok(Some(u));
    }
    runtime
        .resolve_prefix_unfiltered(raw)
        .await
        .map_err(|e| anyhow!("{e}"))
}

/// Find an existing `issue` or `pull_request` note by `properties.number`
/// within `project_id`.
async fn find_issue_or_pr_by_number(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    project_id: Uuid,
    number: u64,
) -> Result<Option<Uuid>> {
    if let Some(id) = find_by_number(runtime, token, "issue", project_id, number).await? {
        return Ok(Some(id));
    }
    find_by_number(runtime, token, "pull_request", project_id, number).await
}

/// Post-ingestion sweep: extract GitHub reference-grammar mentions from
/// every note created this pass and materialize `annotates` edges to the
/// referenced issue/PR note. Fail-open. See
/// crates/khive-pack-git/docs/api/ingest.md#link_references.
async fn link_references(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &VerbRegistry,
    project_id: Uuid,
    new_records: &[NewRecordForRef],
    report: &mut IngestReport,
) {
    for record in new_records {
        let mentions = refs::dedupe_prefer_closes(refs::extract_references(&record.text));
        for mention in mentions {
            let target = match find_issue_or_pr_by_number(
                runtime,
                token,
                project_id,
                mention.number,
            )
            .await
            {
                Ok(Some(id)) => id,
                Ok(None) => {
                    report.reference_edges_unresolved += 1;
                    continue;
                }
                Err(e) => {
                    report
                        .warnings
                        .push(format!("resolving reference #{}: {e}", mention.number));
                    continue;
                }
            };
            if target == record.id {
                // A note referencing its own number (rare, e.g. a PR body
                // that quotes its own number) — not a real cross-reference.
                continue;
            }
            match crate::dispatch_from_token(
                registry,
                token,
                "link",
                json!({
                        "source_id": record.id.to_string(),
                        "target_id": target.to_string(),
                        "relation": "annotates",
                        "metadata": { "ref_kind": mention.kind.as_str() },
                }),
            )
            .await
            {
                Ok(_) => report.reference_edges_created += 1,
                Err(e) => report.warnings.push(format!(
                    "linking reference #{} from {}: {e}",
                    mention.number, record.id
                )),
            }
        }
    }
}

/// Resolve the exact GitHub `owner/repo` that `gh` can access for this
/// checkout, passed explicitly to `gh repo view` (never argument-less
/// selection). Failure strings are stable and credential-safe — see the
/// module overview in crates/khive-pack-git/docs/api/ingest.md.
fn probe_gh_repository(
    repo: &Path,
    expected: Option<&str>,
) -> std::result::Result<String, &'static str> {
    let expected = match expected {
        Some(expected) => validate_owner_repo(expected)?,
        None => github_repository_from_origin(repo)?,
    };
    let output = Command::new("gh")
        .args([
            "repo",
            "view",
            expected.as_str(),
            "--json",
            "nameWithOwner,url",
        ])
        .current_dir(repo)
        // Process-global GH_REPO/GH_HOST overrides could otherwise make this
        // checkout appear usable by probing a different repo or host.
        .env_remove("GH_REPO")
        .env_remove("GH_HOST")
        .env("GH_PROMPT_DISABLED", "1")
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "gh CLI not found on PATH"
            } else {
                "gh CLI could not be started"
            }
        })?;
    if !output.status.success() {
        return Err(
            "gh CLI could not resolve an authenticated GitHub repository for this checkout",
        );
    }
    parse_gh_repository_identity(&output.stdout, &expected)
}

/// Derive a GitHub `owner/repo` from the checkout's fetch identity. Only the
/// configured `origin` is authoritative: another remote, or `gh`'s own local
/// default, must never select the issue/PR source for this ingest.
fn github_repository_from_origin(repo: &Path) -> std::result::Result<String, &'static str> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["remote", "get-url", "origin"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|_| "git CLI could not resolve this checkout's origin repository")?;
    if !output.status.success() {
        return Err("checkout has no usable github.com origin repository");
    }
    let origin = std::str::from_utf8(&output.stdout)
        .map(str::trim)
        .map_err(|_| "checkout has no usable github.com origin repository")?;
    let slug =
        remote_url_to_slug(origin).ok_or("checkout has no usable github.com origin repository")?;
    let mut segments = slug.split('/');
    let (Some(host), Some(owner), Some(name), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return Err("checkout has no usable github.com origin repository");
    };
    if !host.eq_ignore_ascii_case("github.com") {
        return Err("checkout has no usable github.com origin repository");
    }
    validate_owner_repo(&format!("{owner}/{name}"))
}

fn validate_owner_repo(slug: &str) -> std::result::Result<String, &'static str> {
    let Some((owner, name)) = slug.split_once('/') else {
        return Err("invalid expected GitHub repository identity");
    };
    if owner.is_empty()
        || name.is_empty()
        || name.contains('/')
        || slug.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err("invalid expected GitHub repository identity");
    }
    Ok(slug.to_string())
}

/// Validate the repository identity returned by `gh repo view` without
/// trusting either field in isolation. Keeping this pure makes the
/// non-GitHub and mismatched-identity boundaries deterministic to test.
fn parse_gh_repository_identity(
    stdout: &[u8],
    expected: &str,
) -> std::result::Result<String, &'static str> {
    let payload: serde_json::Value = serde_json::from_slice(stdout)
        .map_err(|_| "gh CLI returned an invalid repository identity")?;
    let slug = payload
        .get("nameWithOwner")
        .and_then(serde_json::Value::as_str)
        .ok_or("gh CLI returned an invalid repository identity")?;
    if validate_owner_repo(slug).is_err() {
        return Err("gh CLI returned an invalid repository identity");
    }
    let url = payload
        .get("url")
        .and_then(serde_json::Value::as_str)
        .ok_or("gh CLI returned an invalid repository identity")?;
    let url_slug =
        remote_url_to_slug(url).ok_or("gh CLI returned an invalid repository identity")?;
    if !url_slug.eq_ignore_ascii_case(&format!("github.com/{slug}")) {
        return Err("gh CLI did not resolve a github.com repository for this checkout");
    }
    if !slug.eq_ignore_ascii_case(expected) {
        return Err("gh CLI resolved a different repository than the digest source");
    }
    Ok(slug.to_string())
}

#[cfg(test)]
mod gh_repository_identity_tests {
    use super::parse_gh_repository_identity;

    #[test]
    fn accepts_matching_github_identity() {
        assert_eq!(
            parse_gh_repository_identity(
                br#"{"nameWithOwner":"Fixture/Repository","url":"https://github.com/Fixture/Repository"}"#,
                "fixture/repository",
            ),
            Ok("Fixture/Repository".to_string())
        );
    }

    #[test]
    fn rejects_successful_non_github_identity() {
        assert_eq!(
            parse_gh_repository_identity(
                br#"{"nameWithOwner":"fixture/repository","url":"https://gitlab.com/fixture/repository"}"#,
                "fixture/repository",
            ),
            Err("gh CLI did not resolve a github.com repository for this checkout")
        );
    }

    #[test]
    fn rejects_mismatched_slug_and_url() {
        assert_eq!(
            parse_gh_repository_identity(
                br#"{"nameWithOwner":"fixture/repository","url":"https://github.com/other/repository"}"#,
                "fixture/repository",
            ),
            Err("gh CLI did not resolve a github.com repository for this checkout")
        );
    }

    #[test]
    fn rejects_malformed_identity() {
        assert_eq!(
            parse_gh_repository_identity(
                br#"{"nameWithOwner":"fixture/repository"}"#,
                "fixture/repository",
            ),
            Err("gh CLI returned an invalid repository identity")
        );
    }

    #[test]
    fn rejects_self_consistent_but_unexpected_repository() {
        assert_eq!(
            parse_gh_repository_identity(
                br#"{"nameWithOwner":"other/repository","url":"https://github.com/other/repository"}"#,
                "fixture/repository",
            ),
            Err("gh CLI resolved a different repository than the digest source")
        );
    }
}

/// Look up an existing `commit` note by its `properties.sha` (natural-key
/// idempotence — dedupe before create).
async fn find_commit_by_sha(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    sha: &str,
) -> Result<Option<Uuid>> {
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(|e| anyhow!("{e}"))?;
    let row = r
        .query_row(SqlStatement {
            sql: "SELECT id FROM notes WHERE kind='commit' AND namespace=?1 \
                  AND deleted_at IS NULL AND json_extract(properties,'$.sha')=?2 LIMIT 1"
                .into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(sha.to_string()),
            ],
            label: Some("git_ingest_find_commit_by_sha".into()),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(row.and_then(|r| row_uuid(&r)))
}

/// Look up an existing `issue`/`pull_request` note by its `properties.number`,
/// scoped by kind + namespace + `project_id` (GitHub numbers are
/// repository-scoped — see crates/khive-pack-git/docs/api/ingest.md).
async fn find_by_number(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    kind: &str,
    project_id: Uuid,
    number: u64,
) -> Result<Option<Uuid>> {
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(|e| anyhow!("{e}"))?;
    let row = r
        .query_row(SqlStatement {
            sql: "SELECT id FROM notes WHERE kind=?1 AND namespace=?2 \
                  AND deleted_at IS NULL AND json_extract(properties,'$.number')=?3 \
                  AND json_extract(properties,'$.project_id')=?4 LIMIT 1"
                .into(),
            params: vec![
                SqlValue::Text(kind.to_string()),
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Integer(number as i64),
                SqlValue::Text(project_id.to_string()),
            ],
            label: Some("git_ingest_find_by_number".into()),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(row.and_then(|r| row_uuid(&r)))
}

fn row_uuid(row: &khive_storage::types::SqlRow) -> Option<Uuid> {
    match row.get("id") {
        Some(SqlValue::Uuid(u)) => Some(*u),
        Some(SqlValue::Text(s)) => Uuid::parse_str(s).ok(),
        _ => None,
    }
}

/// Escape SQLite `LIKE` wildcards (`%`, `_`, `\`) so a caller-supplied path
/// matches literally under `LIKE ... ESCAPE '\'`.
fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Find an existing `document` entity whose `properties.source_uri` or
/// `name` matches `path` (ADR-086 keying convention); `None` when no match
/// (v0 never creates documents on the ingester's behalf). See
/// crates/khive-pack-git/docs/api/ingest.md#find_document_for_path for the
/// single-query exact-vs-suffix-match ordering rationale.
async fn find_document_for_path(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    path: &str,
) -> Result<Option<Uuid>> {
    let file_name = Path::new(path)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or(path);
    let sql = runtime.sql();
    let namespace = token.namespace().as_str().to_string();
    let like_pattern = format!("%{}", escape_like(path));

    let mut r = sql.reader().await.map_err(|e| anyhow!("{e}"))?;
    let row = r
        .query_row(SqlStatement {
            sql: "SELECT id FROM entities WHERE kind='document' AND namespace=?1 \
                  AND deleted_at IS NULL \
                  AND (json_extract(properties,'$.source_uri')=?2 OR name=?3 \
                       OR json_extract(properties,'$.source_uri') LIKE ?4 ESCAPE '\\') \
                  ORDER BY CASE WHEN json_extract(properties,'$.source_uri')=?2 OR name=?3 \
                                THEN 0 ELSE 1 END, id \
                  LIMIT 1"
                .into(),
            params: vec![
                SqlValue::Text(namespace),
                SqlValue::Text(path.to_string()),
                SqlValue::Text(file_name.to_string()),
                SqlValue::Text(like_pattern),
            ],
            label: Some("git_ingest_find_document_for_path".into()),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(row.and_then(|r| row_uuid(&r)))
}

/// Load the live code-map module index for the exact repository snapshot
/// being digested, keyed by `(source_revision, source_path)`. See
/// crates/khive-pack-git/docs/api/ingest.md#changed-paths-and-code-module-annotations
/// for the ambiguity contract and the best-effort degradation rule.
async fn load_code_modules_by_snapshot_path(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    source_revision: &str,
) -> Result<HashMap<String, Option<Uuid>>> {
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(|e| anyhow!("{e}"))?;
    let rows = r
        .query_all(SqlStatement {
            sql: "SELECT id, json_extract(properties,'$.source_path') AS source_path \
                  FROM entities WHERE kind='concept' AND entity_type='module' \
                  AND namespace=?1 AND deleted_at IS NULL \
                  AND json_type(properties,'$.source_path')='text' \
                  AND json_extract(properties,'$.source_revision')=?2 \
                  ORDER BY source_path, id"
                .into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(source_revision.to_string()),
            ],
            label: Some("git_ingest_load_code_modules_by_snapshot_path".into()),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;

    let mut modules: HashMap<String, Option<Uuid>> = HashMap::new();
    for row in rows {
        let Some(SqlValue::Text(path)) = row.get("source_path") else {
            continue;
        };
        // A row whose id does not parse is still a live row for its
        // `(source_revision, source_path)` key: it occupies the slot (so any
        // second row for the same key marks the pair ambiguous) but can
        // never bind as an annotation target itself.
        let id = row_uuid(&row);
        modules
            .entry(path.clone())
            .and_modify(|candidate| *candidate = None)
            .or_insert(id);
    }
    Ok(modules)
}

/// Read the last-ingested cursor value for `(project_id, kind)`, if any.
async fn read_cursor(
    runtime: &KhiveRuntime,
    project_id: Uuid,
    kind: &str,
) -> Result<Option<String>> {
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(|e| anyhow!("{e}"))?;
    let row = r
        .query_row(SqlStatement {
            sql: "SELECT cursor_value FROM git_mirror_cursor WHERE project_id=?1 AND kind=?2"
                .into(),
            params: vec![
                SqlValue::Text(project_id.to_string()),
                SqlValue::Text(kind.to_string()),
            ],
            label: Some("git_ingest_read_cursor".into()),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(row.and_then(|r| match r.get("cursor_value") {
        Some(SqlValue::Text(s)) => Some(s.clone()),
        _ => None,
    }))
}

/// Advance the `(project_id, kind)` cursor. See
/// crates/khive-pack-git/docs/api/ingest.md#write_cursor for the
/// stall-then-retry cursor semantics.
async fn write_cursor(
    runtime: &KhiveRuntime,
    project_id: Uuid,
    kind: &str,
    value: &str,
) -> Result<()> {
    let sql = runtime.sql();
    let mut w = sql.writer().await.map_err(|e| anyhow!("{e}"))?;
    w.execute(SqlStatement {
        sql: "INSERT INTO git_mirror_cursor(project_id, kind, cursor_value, updated_at) \
              VALUES(?1, ?2, ?3, ?4) \
              ON CONFLICT(project_id, kind) DO UPDATE SET \
                cursor_value=excluded.cursor_value, \
                updated_at=excluded.updated_at"
            .into(),
        params: vec![
            SqlValue::Text(project_id.to_string()),
            SqlValue::Text(kind.to_string()),
            SqlValue::Text(value.to_string()),
            SqlValue::Integer(Utc::now().timestamp_micros()),
        ],
        label: Some("git_ingest_write_cursor".into()),
    })
    .await
    .map_err(|e| anyhow!("{e}"))?;
    Ok(())
}

// ── commits ─────────────────────────────────────────────────────────────────

const RECORD_SEP: char = '\u{1e}';
const FIELD_SEP: char = '\u{1f}';
const TOUCHED_HEADER_PREFIX: &[u8] = b"/\x1e";

struct RawCommit {
    sha: String,
    short_sha: String,
    author: String,
    author_email: String,
    committed_at: String,
    parents: Vec<String>,
    subject: String,
    body: String,
}

/// Which `git log` pass a classified failure came from (issue #765). See
/// crates/khive-pack-git/docs/api/ingest.md#issue-765-commit-snapshot-recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitLogPhase {
    Metadata,
    TouchedFiles,
}

/// A non-zero-exit `git log` failure, carrying its phase and raw stderr for
/// classification by `is_missing_promisor_object`.
#[derive(Debug)]
pub(crate) struct GitLogError {
    phase: GitLogPhase,
    stderr: String,
}

impl std::fmt::Display for GitLogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cmd = match self.phase {
            GitLogPhase::Metadata => "git log",
            GitLogPhase::TouchedFiles => "git log --name-only",
        };
        write!(f, "{cmd} failed: {}", self.stderr)
    }
}

impl std::error::Error for GitLogError {}

impl GitLogError {
    /// `true` for exactly the class of failure issue #765 authorizes
    /// self-healing for: a missing-object diagnostic that names a promisor
    /// remote. Deliberately narrow — see
    /// crates/khive-pack-git/docs/api/ingest.md#issue-765-commit-snapshot-recovery.
    pub(crate) fn is_missing_promisor_object(&self) -> bool {
        let lower = self.stderr.to_ascii_lowercase();
        lower.contains("promisor")
            && (lower.contains("not in the object database") || lower.contains("missing object"))
    }
}

/// Walk local git history via `git log` with a stable, machine-parseable
/// format. See crates/khive-pack-git/docs/api/ingest.md#issue-765-commit-snapshot-recovery.
fn walk_commits(repo: &Path, since_sha: Option<&str>) -> Result<Vec<RawCommit>> {
    // Raw control-byte separators embedded directly in the format string
    // (not git's `%xHH` escape syntax) — passed as a single argv element
    // (never through a shell), so the literal bytes survive intact and git's
    // pretty-format engine emits any non-`%` character verbatim.
    let format = format!("%H{FIELD_SEP}%h{FIELD_SEP}%an{FIELD_SEP}%ae{FIELD_SEP}%cI{FIELD_SEP}%P{FIELD_SEP}%s{FIELD_SEP}%b{RECORD_SEP}");
    let mut args = vec![
        "log".to_string(),
        "--reverse".to_string(),
        format!("--pretty=format:{format}"),
    ];
    if let Some(sha) = since_sha {
        args.push(format!("{sha}..HEAD"));
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(&args)
        .output()
        .context("spawning git log")?;
    if !output.status.success() {
        return Err(anyhow::Error::new(GitLogError {
            phase: GitLogPhase::Metadata,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut commits = Vec::new();
    for record in text.split(RECORD_SEP) {
        let record = record.trim_matches('\n');
        if record.is_empty() {
            continue;
        }
        let fields: Vec<&str> = record.splitn(8, FIELD_SEP).collect();
        if fields.len() < 8 {
            continue;
        }
        let sha = fields[0].to_string();
        let short_sha = fields[1].to_string();
        let author = fields[2].to_string();
        let author_email = fields[3].to_string();
        let committed_at = fields[4].to_string();
        let parents = fields[5]
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let subject = fields[6].to_string();
        let body = fields[7].trim_end_matches('\n').to_string();
        commits.push(RawCommit {
            sha,
            short_sha,
            author,
            author_email,
            committed_at,
            parents,
            subject,
            body,
        });
    }
    Ok(commits)
}

/// `sha -> [touched paths]` for every commit in `repo`'s history, via a
/// separate NUL-delimited `--name-only` pass. See
/// crates/khive-pack-git/docs/api/ingest.md#changed-paths-and-code-module-annotations.
fn touched_files(repo: &Path) -> Result<HashMap<String, Vec<String>>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("log")
        .arg("-z")
        .arg("--name-only")
        .arg("--no-renames")
        .arg("--diff-merges=first-parent")
        // Git paths are always repository-relative, so no tracked path token
        // can start with `/`. This absolute-looking prefix is therefore an
        // unambiguous header sentinel in the NUL-delimited token stream.
        .arg(format!("--pretty=format:/{RECORD_SEP}%H"))
        .output()
        .context("spawning git log --name-only")?;
    if !output.status.success() {
        return Err(anyhow::Error::new(GitLogError {
            phase: GitLogPhase::TouchedFiles,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }));
    }
    parse_touched_files(&output.stdout)
}

/// Decode the `-z --name-only` stream [`touched_files`] produces. See
/// crates/khive-pack-git/docs/api/ingest.md#changed-paths-and-code-module-annotations
/// for the header-loss ambiguity this parser deliberately does not resolve.
fn parse_touched_files(bytes: &[u8]) -> Result<HashMap<String, Vec<String>>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    // With `-z`, git NUL-terminates each token; the header and its first
    // path share one token, separated by exactly one newline.
    let mut current_sha: Option<String> = None;
    for token in bytes.split(|byte| *byte == 0) {
        if token.is_empty() {
            continue;
        }
        if let Some(header) = token.strip_prefix(TOUCHED_HEADER_PREFIX) {
            if header.len() < 40 || !header[..40].iter().all(u8::is_ascii_hexdigit) {
                let lossy = String::from_utf8_lossy(header);
                let masked = secret_gate::mask_secrets(lossy.as_ref());
                let display = refs::truncate_chars(masked.as_ref(), 80);
                return Err(anyhow!(
                    "git log --name-only output contains a malformed commit header {:?}",
                    display
                ));
            }
            let sha = String::from_utf8_lossy(&header[..40]).into_owned();
            let files = map.entry(sha.clone()).or_default();
            if let Some(first_path) = header[40..].strip_prefix(b"\n") {
                if !first_path.is_empty() {
                    files.push(String::from_utf8_lossy(first_path).into_owned());
                }
            } else if !header[40..].is_empty() {
                // Anything after the SHA that does not start with the one
                // newline separator is a shape git never emits; guessing at
                // it could drop or fabricate a first path.
                return Err(anyhow!(
                    "git log --name-only header for commit {sha} carries a \
                     malformed first-path separator"
                ));
            }
            current_sha = Some(sha);
            continue;
        }
        let Some(sha) = &current_sha else {
            // Path bytes are attacker/repo-controlled, so the error snippet
            // is secret-masked the same way stored changed_paths are —
            // never raw token bytes in a log/error path.
            let lossy = String::from_utf8_lossy(token);
            let masked = secret_gate::mask_secrets(lossy.as_ref());
            let display = refs::truncate_chars(masked.as_ref(), 80);
            return Err(anyhow!(
                "git log --name-only output contains a path token before any \
                 commit header: {display:?}"
            ));
        };
        map.get_mut(sha)
            .expect("current SHA was inserted with its header")
            .push(String::from_utf8_lossy(token).into_owned());
    }
    Ok(map)
}

#[cfg(test)]
mod touched_file_parser_tests {
    use super::parse_touched_files;

    #[test]
    fn accepts_single_or_double_nul_commit_boundaries() {
        let sha_a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let sha_b = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let sha_c = "cccccccccccccccccccccccccccccccccccccccc";
        let raw =
            format!("/\x1e{sha_a}\nfirst.rs\0second.rs\0/\x1e{sha_b}\nthird.rs\0\0/\x1e{sha_c}\0");

        let parsed = parse_touched_files(raw.as_bytes()).expect("well-formed stream");
        assert_eq!(
            parsed[sha_a],
            vec!["first.rs".to_string(), "second.rs".to_string()]
        );
        assert_eq!(parsed[sha_b], vec!["third.rs".to_string()]);
        assert!(parsed[sha_c].is_empty());
    }

    #[test]
    fn preserves_delimiters_and_uses_lossy_utf8_path_normalization() {
        let sha = "dddddddddddddddddddddddddddddddddddddddd";
        let mut raw = format!("/\x1e{sha}\n").into_bytes();
        raw.extend_from_slice(b"src/caf\xc3\xa9\t\"quoted\"\\leaf\nline.rs\0bad-\xff.rs\0");

        let parsed = parse_touched_files(&raw).expect("well-formed stream");
        assert_eq!(
            parsed[sha],
            vec![
                "src/café\t\"quoted\"\\leaf\nline.rs".to_string(),
                "bad-�.rs".to_string(),
            ]
        );
    }

    #[test]
    fn rejects_a_path_token_before_any_header() {
        // Silently dropping this token could store `[]` for a commit that
        // touched files; failing the phase preserves the retry contract.
        let sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let raw = format!("orphan.rs\0/\x1e{sha}\nfirst.rs\0");

        let err = parse_touched_files(raw.as_bytes()).expect_err("orphan path must fail");
        assert!(
            format!("{err}").contains("before any commit header"),
            "{err}"
        );
    }

    #[test]
    fn masks_a_secret_shaped_orphan_token_in_the_error() {
        // The error snippet is a log path: it must carry the same masking as
        // stored changed_paths, never raw token bytes.
        let fake_token = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let raw = format!("src/{fake_token}.rs\0");

        let err = parse_touched_files(raw.as_bytes()).expect_err("orphan path must fail");
        let text = format!("{err}");
        assert!(!text.contains(fake_token), "{text}");
        assert!(text.contains("MASKED"), "{text}");
    }

    #[test]
    fn masks_a_secret_shaped_malformed_header_in_the_error() {
        let fake_token = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let raw = format!("/\x1e{fake_token}\0");

        let err = parse_touched_files(raw.as_bytes()).expect_err("bad header must fail");
        let text = format!("{err}");
        assert!(!text.contains(fake_token), "{text}");
        assert!(text.contains("MASKED"), "{text}");
    }

    #[test]
    fn masks_an_orphan_secret_before_truncating_the_error() {
        let fake_token = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        // The credential crosses the old byte-80 cut, so masking only the
        // prefix would leave the raw secret in the error path.
        let token = format!("{} {fake_token}", "x".repeat(59));
        assert!(token.len() > 80);
        let raw = format!("{token}\0");

        let err = parse_touched_files(raw.as_bytes()).expect_err("orphan path must fail");
        let text = format!("{err}");
        assert!(!text.contains(fake_token), "{text}");
        assert!(text.contains("MASKED"), "{text}");
    }

    #[test]
    fn rejects_a_malformed_header_sha() {
        let raw = "/\x1enot-hex-at-all\nfirst.rs\0second.rs\0";

        let err = parse_touched_files(raw.as_bytes()).expect_err("bad header must fail");
        assert!(
            format!("{err}").contains("malformed commit header"),
            "{err}"
        );
    }

    #[test]
    fn rejects_a_header_remainder_without_the_newline_separator() {
        // A header remainder that does not start with exactly one newline is
        // a shape git never emits; silently discarding it could lose the
        // first path or misread it as a leading-newline path.
        let sha = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let raw = format!("/\x1e{sha}XXfirst.rs\0");

        let err = parse_touched_files(raw.as_bytes()).expect_err("bad separator must fail");
        assert!(format!("{err}").contains("first-path separator"), "{err}");
    }
}

/// The two `git log` passes a commit-ingest phase needs, loaded together so
/// a classified failure in either one can be retried as a single unit.
struct CommitSnapshot {
    commits: Vec<RawCommit>,
    files_by_sha: HashMap<String, Vec<String>>,
}

/// Load one commit-history snapshot; skips `touched_files` entirely when
/// `walk_commits` found no new commits.
fn load_commit_snapshot(repo: &Path, since_sha: Option<&str>) -> Result<CommitSnapshot> {
    let commits = walk_commits(repo, since_sha)?;
    if commits.is_empty() {
        return Ok(CommitSnapshot {
            commits,
            files_by_sha: HashMap::new(),
        });
    }
    let files_by_sha = touched_files(repo)?;
    Ok(CommitSnapshot {
        commits,
        files_by_sha,
    })
}

/// Which repair `RemoteCommitRecovery` (`handlers.rs`) performed. See
/// crates/khive-pack-git/docs/api/ingest.md#issue-765-commit-snapshot-recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheRepairStrategy {
    Refetch,
    Reclone,
}

/// The repo path and strategy a `recover` callback used to repair a
/// classified `GitLogError`.
pub(crate) struct RecoveredRepo {
    pub(crate) repo: PathBuf,
    pub(crate) strategy: CacheRepairStrategy,
}

fn cache_repair_warning(strategy: CacheRepairStrategy) -> String {
    match strategy {
        CacheRepairStrategy::Refetch => {
            "repaired corrupt remote git cache by refetching missing promisor objects".to_string()
        }
        CacheRepairStrategy::Reclone => {
            "repaired corrupt remote git cache by replacing the owned clone".to_string()
        }
    }
}

/// Load a commit-history snapshot, retrying through `recover` when the
/// failure is a classified missing-promisor-object error (issue #765). See
/// crates/khive-pack-git/docs/api/ingest.md#issue-765-commit-snapshot-recovery
/// for the retry-bound semantics.
fn recover_commit_snapshot(
    repo: &Path,
    since_sha: Option<&str>,
    mut recover: impl FnMut(&Path, &GitLogError) -> Result<Option<RecoveredRepo>>,
) -> Result<(CommitSnapshot, Option<String>)> {
    let mut repo_path = repo.to_path_buf();
    let mut recovery_warning: Option<String> = None;
    loop {
        match load_commit_snapshot(&repo_path, since_sha) {
            Ok(snapshot) => return Ok((snapshot, recovery_warning)),
            Err(e) => {
                let classified = e
                    .downcast_ref::<GitLogError>()
                    .filter(|g| g.is_missing_promisor_object());
                let Some(git_log_err) = classified else {
                    return Err(e);
                };
                match recover(&repo_path, git_log_err)? {
                    Some(recovered) => {
                        repo_path = recovered.repo;
                        recovery_warning = Some(cache_repair_warning(recovered.strategy));
                    }
                    None => return Err(e),
                }
            }
        }
    }
}

/// Squash-merge subject suffix `"... (#123)"` -> `123`.
fn squash_merge_pr_number(subject: &str) -> Option<u64> {
    let trimmed = subject.trim_end();
    let close = trimmed.strip_suffix(')')?;
    let open = close.rfind("(#")?;
    close[open + 2..].parse::<u64>().ok()
}

/// Max characters for the `name` field the amendment's readable-names rider
/// sets on newly ingested notes (issues/PRs: `"#<number> <title>"`; commits:
/// `"<short_sha> <subject>"`).
const NAME_MAX_CHARS: usize = 120;

/// Cap for the text a commit note sends to the vector embedder (issue #764).
/// Matches the repository's existing `MAX_EMBED_BYTES` precedent
/// (`khive-pack-knowledge`, `kkernel::reindex`, ADR-048) — bytes, not chars,
/// UTF-8-boundary-safe. The full, untruncated commit content is always
/// stored and FTS-indexed; only the candidate vector input is capped.
const MAX_COMMIT_EMBED_BYTES: usize = 32_768;

/// Sentinel reason seeded into the commit source slot when a walk begins.
/// A slot still holding this exact reason at the end of the pass means the
/// walk ran to the end of its snapshot without a budget break or a stall,
/// and is rewritten to `Completed`; any other reason was written by a real
/// early-stop arm and is preserved.
const COMMIT_WALK_SEED_REASON: &str = "walk began but did not report completion";

/// Returns a UTF-8-valid, proper head prefix of `content` when it exceeds
/// `MAX_COMMIT_EMBED_BYTES`, or `None` when `content` is at or under the cap
/// (nothing to truncate — the full text is a valid embedding input as-is).
fn truncated_embedding_head(content: &str) -> Option<&str> {
    if content.len() <= MAX_COMMIT_EMBED_BYTES {
        return None;
    }
    let mut end = MAX_COMMIT_EMBED_BYTES;
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    Some(&content[..end])
}

/// Every `RawCommit` string field funnels through this constructor before it
/// can reach `properties` or the note `name`, masking secrets in
/// caller-controlled prose fields. See
/// crates/khive-pack-git/docs/api/ingest.md#masking-boundaries-maskedcommitfields-maskedissuefields-maskedprfields.
struct MaskedCommitFields {
    sha: String,
    short_sha: String,
    author: String,
    author_email: String,
    committed_at: String,
    parents: Vec<String>,
    subject: String,
    body: String,
}

impl MaskedCommitFields {
    fn new(commit: &RawCommit) -> Self {
        let RawCommit {
            sha,
            short_sha,
            author,
            author_email,
            committed_at,
            parents,
            subject,
            body,
        } = commit;
        Self {
            sha: sha.clone(),
            short_sha: short_sha.clone(),
            author: secret_gate::mask_secrets(author).into_owned(),
            author_email: secret_gate::mask_secrets(author_email).into_owned(),
            committed_at: committed_at.clone(),
            parents: parents.clone(),
            subject: secret_gate::mask_secrets(subject).into_owned(),
            body: secret_gate::mask_secrets(body).into_owned(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn ingest_commits(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &VerbRegistry,
    repo: &Path,
    project_id: Uuid,
    merge_sha_to_pr: &HashMap<String, Uuid>,
    number_to_pr: &HashMap<u64, Uuid>,
    report: &mut IngestReport,
    budget: &mut Budget,
    new_records: &mut Vec<NewRecordForRef>,
    recover: &mut (dyn FnMut(&Path, &GitLogError) -> Result<Option<RecoveredRepo>> + Send),
    walk_complete: &mut bool,
) -> Result<()> {
    let since = read_cursor(runtime, project_id, "commits").await?;
    let (snapshot, recovery_warning) = recover_commit_snapshot(repo, since.as_deref(), recover)?;
    let CommitSnapshot {
        commits,
        files_by_sha,
    } = snapshot;
    if commits.is_empty() {
        // An empty range is a genuine completion only when the cursor is an
        // ancestor of HEAD. See crates/khive-pack-git/docs/ingest.md
        // #commit-walk-ancestor-divergence-and-cursor-stall.
        let cursor_not_ancestor = last_sha_of(&since).is_some_and(|since_sha| {
            let not_ancestor = !is_ancestor_of_head(repo, since_sha);
            if not_ancestor {
                report.warnings.push(format!(
                    "commits cursor {since_sha} is not an ancestor of this \
                     source's HEAD: the walked history lags or diverged from \
                     the history that advanced the cursor, so nothing was \
                     walked (issue #1644)"
                ));
                report.done = false;
            }
            not_ancestor
        });
        if let Some(warning) = recovery_warning {
            report.warnings.push(warning);
        }
        // Recorded here, not the end-of-pass fill: a diverged cursor is a
        // stop-early, while an ancestor cursor is a genuine completion.
        if cursor_not_ancestor {
            report.sources.commits = Some(IngestSourceState::StoppedEarly(
                "commits cursor is not an ancestor of this source's HEAD: the walked history \
                 lags or diverged from the history that advanced the cursor (issue #1644)"
                    .into(),
            ));
        } else {
            report.sources.commits = Some(IngestSourceState::Completed);
            *walk_complete = true;
        }
        return Ok(());
    }

    // Seed the source before the first natural-key lookup so a mid-walk
    // database error reports as walked-then-failed, not a pre-walk error.
    report.sources.commits = Some(IngestSourceState::StoppedEarly(
        COMMIT_WALK_SEED_REASON.into(),
    ));
    // The last record (walk is oldest-first) is the exact snapshot HEAD the
    // module index binds against; the walk itself is never truncated by
    // `max_items` — only the create loop below is.
    let snapshot_head = commits
        .last()
        .expect("non-empty commit snapshot checked above")
        .sha
        .clone();
    // Module annotation is best-effort enrichment: a failed index load
    // degrades to no module annotation with a warning carrying the load
    // error's text rather than aborting the pass that records the durable
    // `changed_paths` facts.
    let code_modules_by_source_path =
        match load_code_modules_by_snapshot_path(runtime, token, &snapshot_head).await {
            Ok(modules) => modules,
            Err(e) => {
                report.warnings.push(format!(
                    "code module index load failed for snapshot {snapshot_head}: {e}; \
                     commits ingest without code-module annotation"
                ));
                HashMap::new()
            }
        };

    // `cursor_stalled` freezes `last_sha` at the last contiguous success so a
    // failed record is retried next pass instead of skipped forever; later
    // records this pass are still attempted. See crates/khive-pack-git/docs/
    // ingest.md#commit-walk-ancestor-divergence-and-cursor-stall.
    let mut last_sha: Option<String> = since;
    let mut cursor_stalled = false;
    // Bounded detail for the per-run ambiguous-module-skip warning: the
    // masked skipped paths in encounter order, capped so one pathological
    // run cannot bloat the report (the full count is always exact).
    const AMBIGUOUS_SKIP_DETAIL_CAP: usize = 5;
    const AMBIGUOUS_SKIP_PATH_DISPLAY_CHARS: usize = 80;
    let mut ambiguous_module_skip_paths: Vec<String> = Vec::new();
    // Parent SHA -> note id for commits created earlier this pass; combined
    // with `find_commit_by_sha`'s DB lookup, resolves parent edges regardless
    // of which pass the parent landed in. The stall guard on the `last_sha`
    // advances below prevents stranding a failed commit behind an advanced
    // floor. See crates/khive-pack-git/docs/ingest.md
    // #commit-walk-ancestor-divergence-and-cursor-stall.
    let mut local_sha_to_id: HashMap<String, Uuid> = HashMap::new();
    for c in &commits {
        if let Some(existing) = find_commit_by_sha(runtime, token, &c.sha).await? {
            local_sha_to_id.insert(c.sha.clone(), existing);
            report.commits_skipped_existing += 1;
            if !cursor_stalled {
                last_sha = Some(c.sha.clone());
            }
            continue;
        }

        if budget.exhausted() {
            report.sources.commits = Some(IngestSourceState::StoppedEarly(
                "budget exhausted before the commit history was exhausted".into(),
            ));
            break;
        }

        let masked = MaskedCommitFields::new(c);
        let content = if masked.body.trim().is_empty() {
            masked.subject.clone()
        } else {
            format!("{}\n\n{}", masked.subject, masked.body)
        };

        // Both `git log` passes walk the same history, so every walked
        // commit should have a path-set entry. A missing entry means the two
        // passes disagree; surface it instead of silently storing the `[]`
        // the contract reserves for a genuinely empty commit.
        let Some(touched_paths) = files_by_sha.get(&c.sha) else {
            cursor_stalled = true;
            let recipient_detail = commits
                .iter()
                .position(|candidate| candidate.sha == c.sha)
                .and_then(|index| {
                    commits
                        .iter()
                        .skip(index + 1)
                        .find(|candidate| files_by_sha.contains_key(&candidate.sha))
                })
                .map(|recipient| {
                    format!(
                        "; orphaned paths may have been absorbed by newer commit {}",
                        recipient.sha
                    )
                })
                .unwrap_or_else(|| {
                    "; no newer path-set recipient was identifiable from this snapshot".to_string()
                });
            report.warnings.push(format!(
                "create commit {}: no touched-path set recorded by the \
                 --name-only pass; not ingested{}",
                c.sha, recipient_detail
            ));
            continue;
        };
        // Canonical-path filtering, drop accounting, and raw-vs-masked
        // filter ordering are documented at crates/khive-pack-git/docs/api/
        // ingest.md#changed-paths-and-code-module-annotations.
        let mut noncanonical = 0_u64;
        let changed_paths: Vec<String> = touched_paths
            .iter()
            .filter(|path| {
                let canonical = hook::is_repo_relative_path(path);
                if !canonical {
                    noncanonical += 1;
                }
                canonical
            })
            .map(|path| secret_gate::mask_secrets(path).into_owned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        report.changed_paths_filtered_noncanonical += noncanonical;
        // The three stored `changed_paths` states (empty/remainder/omitted)
        // are documented at crates/khive-pack-git/docs/api/ingest.md
        // #changed-paths-and-code-module-annotations.
        let changed_paths_property = if touched_paths.is_empty() || !changed_paths.is_empty() {
            Some(changed_paths.clone())
        } else {
            None
        };
        let mut annotates = BTreeSet::from([project_id.to_string()]);

        for path in &changed_paths {
            match code_modules_by_source_path.get(path) {
                Some(Some(module_id)) => {
                    annotates.insert(module_id.to_string());
                }
                // Ambiguous-key skip accounting is documented at
                // crates/khive-pack-git/docs/api/ingest.md#changed-paths-and-code-module-annotations.
                Some(None) => {
                    report.code_module_ambiguous_path_skips += 1;
                    if ambiguous_module_skip_paths.len() < AMBIGUOUS_SKIP_DETAIL_CAP {
                        ambiguous_module_skip_paths.push(path.clone());
                    }
                }
                None => {}
            }
            if path.starts_with("docs/adr/") {
                if let Some(doc_id) = find_document_for_path(runtime, token, path).await? {
                    annotates.insert(doc_id.to_string());
                }
            }
        }

        // Merge-commit sha mapping and squash-merge suffix parsing are both
        // scoped to PRs discovered THIS pass; also fall back to a direct
        // by-number lookup so a commit can still resolve its merging PR when
        // that PR was ingested in an earlier pass (its note already exists,
        // but this run's `number_to_pr` in-memory map starts empty).
        let pr_id = match merge_sha_to_pr.get(&c.sha).copied() {
            Some(id) => Some(id),
            None => match squash_merge_pr_number(&c.subject) {
                Some(n) => match number_to_pr.get(&n).copied() {
                    Some(id) => Some(id),
                    None => find_by_number(runtime, token, "pull_request", project_id, n).await?,
                },
                None => None,
            },
        };
        if let Some(pr_id) = pr_id {
            annotates.insert(pr_id.to_string());
        }

        let mut properties = json!({
            "sha": masked.sha,
            "short_sha": masked.short_sha,
            "author": masked.author,
            "author_email": masked.author_email,
            "committed_at": masked.committed_at,
            "parents": masked.parents,
        });
        if let Some(paths) = changed_paths_property {
            properties["changed_paths"] = json!(paths);
        }

        let name = refs::truncate_chars(
            &format!("{} {}", masked.short_sha, masked.subject),
            NAME_MAX_CHARS,
        );
        let embedding_head = truncated_embedding_head(&content);

        let mut create_request = json!({
            "kind": "commit",
            "name": name,
            "content": content,
            "properties": properties,
            "annotates": annotates.into_iter().collect::<Vec<_>>(),
        });
        if let Some(head) = embedding_head {
            create_request["embedding_content"] = json!(head);
        }

        budget.try_consume();
        match crate::dispatch_from_token(registry, token, "create", create_request).await {
            Ok(v) => {
                report.commits_ingested += 1;
                if embedding_head.is_some() {
                    report.commit_embeddings_truncated += 1;
                }
                if !cursor_stalled {
                    last_sha = Some(c.sha.clone());
                }
                if let Some(id) = v
                    .get("id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| Uuid::parse_str(s).ok())
                {
                    local_sha_to_id.insert(c.sha.clone(), id);
                    new_records.push(NewRecordForRef {
                        id,
                        text: content.clone(),
                    });
                    // Parent -> child `precedes` edges (ADR-088 Amendment 1
                    // ingest enrichment). Fail-open: an unresolved or
                    // failing parent link is skipped/warned, never aborts
                    // the pass.
                    for parent_sha in &c.parents {
                        let parent_id = match local_sha_to_id.get(parent_sha).copied() {
                            Some(pid) => Some(pid),
                            None => find_commit_by_sha(runtime, token, parent_sha).await?,
                        };
                        let Some(parent_id) = parent_id else {
                            continue;
                        };
                        if parent_id == id {
                            continue;
                        }
                        match crate::dispatch_from_token(
                            registry,
                            token,
                            "link",
                            json!({
                                    "source_id": parent_id.to_string(),
                                    "target_id": id.to_string(),
                                    "relation": "precedes",
                            }),
                        )
                        .await
                        {
                            Ok(_) => report.parent_edges_created += 1,
                            Err(e) => report.warnings.push(format!(
                                "linking parent {parent_sha} -> {} precedes: {e}",
                                c.sha
                            )),
                        }
                    }
                }
            }
            Err(e) => {
                record_write_failure(report, "create", "commit", c.sha.clone(), e);
                cursor_stalled = true;
            }
        }
    }

    if cursor_stalled {
        report.cursor_stalled = true;
        report.done = false;
        report.sources.commits = Some(IngestSourceState::StoppedEarly(
            "a per-record write failure froze the commits cursor (cursor_stalled)".into(),
        ));
    } else {
        let walk_ended_clean = match &report.sources.commits {
            None => true,
            Some(IngestSourceState::StoppedEarly(reason)) => reason == COMMIT_WALK_SEED_REASON,
            Some(_) => false,
        };
        if walk_ended_clean {
            // Neither a budget break nor a stall fired: the loop visited
            // every commit in the snapshot, so the walk-start seed (or a
            // `None` from an instrumentation gap) is rewritten to
            // `Completed`. Any other reason was written by a real
            // early-stop arm and is preserved.
            report.sources.commits = Some(IngestSourceState::Completed);
            *walk_complete = true;
        }
    }
    if let Some(sha) = last_sha {
        // A stalled cursor has already frozen `last_sha` at the last
        // contiguous success above; the write persists exactly that floor.
        // A failure here surfaces at the call site, which distinguishes it
        // from a pre-walk failure by the already-recorded source state and
        // downgrades the pass to stopped-early in-band (never a hard
        // abort of the whole ingest).
        write_cursor(runtime, project_id, "commits", &sha).await?;
    }
    // One bounded line per run (never one per path): filenames are
    // attacker/repo-controlled and may be long, so the count is the
    // load-bearing fact; a bounded masked path sample rides along so the
    // count is actionable, and the full set remains recoverable from the
    // raw `git log -z --name-only` stream if an operator needs it.
    if report.changed_paths_filtered_noncanonical > 0 {
        report.warnings.push(format!(
            "{} touched path(s) dropped from changed_paths: outside the \
             canonical repo-relative shape (NUL byte, backslash, `X:` \
             drive prefix, leading `/`, or empty/`.`/`..` component)",
            report.changed_paths_filtered_noncanonical
        ));
    }
    if report.code_module_ambiguous_path_skips > 0 {
        // Every skip increments the exact counter above; only the retained
        // masked path sample is capped at AMBIGUOUS_SKIP_DETAIL_CAP.
        let shown = ambiguous_module_skip_paths
            .len()
            .min(AMBIGUOUS_SKIP_DETAIL_CAP);
        let detail = ambiguous_module_skip_paths[..shown]
            .iter()
            .map(|path| {
                let display = refs::truncate_chars(path, AMBIGUOUS_SKIP_PATH_DISPLAY_CHARS);
                format!("{display:?}")
            })
            .collect::<Vec<_>>()
            .join(", ");
        let remaining = report
            .code_module_ambiguous_path_skips
            .saturating_sub(shown as u64);
        let suffix = if remaining > 0 {
            format!(", +{remaining} more")
        } else {
            String::new()
        };
        report.warnings.push(format!(
            "{} changed path(s) skipped code-module annotation: no usable \
             (source_revision, source_path) binding (first {shown}: \
             [{detail}]{suffix})",
            report.code_module_ambiguous_path_skips
        ));
    }
    if let Some(warning) = recovery_warning {
        report.warnings.push(warning);
    }
    Ok(())
}

/// Borrow the cursor SHA out of the `Option<String>` read from the store.
fn last_sha_of(since: &Option<String>) -> Option<&str> {
    since.as_deref().filter(|s| !s.is_empty())
}

/// `true` when `sha` is an ancestor of (or equal to) `repo`'s HEAD. A SHA
/// that is unknown to this repo returns `false` — for the empty-walk guard
/// that is the correct reading: the walked source does not contain the
/// history that advanced the cursor.
fn is_ancestor_of_head(repo: &Path, sha: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge-base", "--is-ancestor", sha, "HEAD"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Count `commit` notes annotating `project_id`, derived fresh from the
/// database rather than tracked in-memory across process invocations — see
/// `IngestReport::commits_total_in_db` (issue #1045).
async fn count_commit_notes_for_project(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    project_id: Uuid,
) -> Result<u64> {
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(|e| anyhow!("{e}"))?;
    let row = r
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM notes n \
                  JOIN graph_edges e ON e.source_id = n.id AND e.namespace = n.namespace \
                  WHERE n.kind = 'commit' AND n.namespace = ?1 AND n.deleted_at IS NULL \
                  AND e.relation = 'annotates' AND e.target_id = ?2 AND e.deleted_at IS NULL"
                .into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(project_id.to_string()),
            ],
            label: Some("git_ingest_count_commit_notes".into()),
        })
        .await
        .map_err(|e| anyhow!("{e}"))?;
    match row {
        Some(SqlValue::Integer(n)) => Ok(n as u64),
        _ => Ok(0),
    }
}

// ── issues + PRs (gh CLI) ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GhAuthor {
    login: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhLabel {
    name: String,
}

#[derive(Debug, Deserialize)]
struct GhIssue {
    number: u64,
    title: String,
    author: Option<GhAuthor>,
    #[serde(rename = "createdAt")]
    created_at: Option<String>,
    #[serde(rename = "closedAt")]
    closed_at: Option<String>,
    #[serde(rename = "updatedAt")]
    updated_at: Option<String>,
    labels: Option<Vec<GhLabel>>,
    #[serde(rename = "stateReason")]
    state_reason: Option<String>,
    body: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhMergeCommit {
    oid: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhPr {
    number: u64,
    title: String,
    author: Option<GhAuthor>,
    #[serde(rename = "createdAt")]
    created_at: Option<String>,
    #[serde(rename = "mergedAt")]
    merged_at: Option<String>,
    #[serde(rename = "closedAt")]
    closed_at: Option<String>,
    #[serde(rename = "updatedAt")]
    updated_at: Option<String>,
    #[serde(rename = "baseRefName")]
    base_ref_name: Option<String>,
    #[serde(rename = "headRefName")]
    head_ref_name: Option<String>,
    #[serde(rename = "mergeCommit")]
    merge_commit: Option<GhMergeCommit>,
    body: Option<String>,
}

/// Every `GhIssue` field funnels through this constructor before it can
/// reach `properties`/`content`/the note name/the paging cursor. See
/// crates/khive-pack-git/docs/api/ingest.md#masking-boundaries-maskedcommitfields-maskedissuefields-maskedprfields.
struct MaskedIssueFields {
    number: u64,
    title: String,
    body: String,
    author_login: Option<String>,
    labels: Vec<String>,
    created_at: Option<String>,
    closed_at: Option<String>,
    updated_at: Option<String>,
    state_reason: StateReasonField,
}

/// Classified outcome of parsing a raw `stateReason` against the governed
/// enum. `Rejected` never carries the raw string forward. See
/// crates/khive-pack-git/docs/api/ingest.md#masking-boundaries-maskedcommitfields-maskedissuefields-maskedprfields.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StateReasonField {
    Absent,
    Valid(String),
    Rejected,
}

impl MaskedIssueFields {
    fn new(issue: GhIssue, warnings: &mut Vec<String>) -> Self {
        let GhIssue {
            number,
            title,
            author,
            created_at,
            closed_at,
            updated_at,
            labels,
            state_reason,
            body,
        } = issue;

        Self {
            number,
            title: secret_gate::mask_secrets(&title).into_owned(),
            body: secret_gate::mask_secrets(&body.unwrap_or_default()).into_owned(),
            author_login: author
                .and_then(|a| a.login)
                .map(|login| secret_gate::mask_secrets(&login).into_owned()),
            labels: labels
                .unwrap_or_default()
                .into_iter()
                .map(|l| secret_gate::mask_secrets(&l.name).into_owned())
                .collect(),
            created_at: canonical_issue_timestamp("createdAt", number, created_at, warnings),
            closed_at: canonical_issue_timestamp("closedAt", number, closed_at, warnings),
            updated_at: canonical_issue_timestamp("updatedAt", number, updated_at, warnings),
            state_reason: canonical_issue_state_reason(state_reason),
        }
    }
}

/// Classifies a raw `stateReason` string against the governed enum
/// (`hook::ISSUE_STATE_REASONS`, ADR-088 §3), case-normalized first. See
/// crates/khive-pack-git/docs/api/ingest.md#masking-boundaries-maskedcommitfields-maskedissuefields-maskedprfields.
fn canonical_issue_state_reason(raw: Option<String>) -> StateReasonField {
    let Some(raw) = raw.filter(|r| !r.is_empty()) else {
        return StateReasonField::Absent;
    };
    let lowered = raw.to_ascii_lowercase();
    if hook::ISSUE_STATE_REASONS.contains(&lowered.as_str()) {
        StateReasonField::Valid(lowered)
    } else {
        StateReasonField::Rejected
    }
}

/// Parses a GitHub issue timestamp into canonical RFC3339 form; on parse
/// failure the field is dropped (with a warning, never the raw value) and
/// the issue is still ingested. See
/// crates/khive-pack-git/docs/api/ingest.md#masking-boundaries-maskedcommitfields-maskedissuefields-maskedprfields.
fn canonical_issue_timestamp(
    field: &'static str,
    number: u64,
    raw: Option<String>,
    warnings: &mut Vec<String>,
) -> Option<String> {
    let raw = raw?;
    match chrono::DateTime::parse_from_rfc3339(&raw) {
        Ok(dt) => Some(
            dt.with_timezone(&Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        ),
        Err(_) => {
            warnings.push(format!(
                "issue #{number}: {field} is not a valid RFC3339 timestamp, field dropped"
            ));
            None
        }
    }
}

fn gh_json(repo: &Path, gh_repo: &str, args: &[&str]) -> Result<String> {
    // gh has no `-C` flag (unlike git). Keep cwd for local git configuration,
    // but target the repository explicitly so later remote/cwd drift cannot
    // redirect a resumed digest to a different repository.
    let output = Command::new("gh")
        .current_dir(repo)
        .args(args)
        .args(["--repo", gh_repo])
        .env_remove("GH_REPO")
        .env_remove("GH_HOST")
        .env("GH_PROMPT_DISABLED", "1")
        .output()
        .context("spawning gh")?;
    if !output.status.success() {
        let operation = args.get(0..2).unwrap_or(args).join(" ");
        return Err(anyhow!("gh {operation} failed"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Per-page fetch cap for both PR and issue paging — `gh {pr,issue} list
/// --search` never returns more than this many results for a single query
/// regardless of `--limit`. See
/// crates/khive-pack-git/docs/api/ingest.md#paging-pageoutcome-decide_page_outcome-page_limit.
const PAGE_LIMIT: usize = 1000;

/// What a paging loop should do after processing one fetched page — the
/// entire "was the remote window proven exhausted" decision lives here. See
/// crates/khive-pack-git/docs/api/ingest.md#paging-pageoutcome-decide_page_outcome-page_limit.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PageOutcome {
    /// Page held fewer than `PAGE_LIMIT` items: remote window proven exhausted.
    WindowComplete,
    /// Page was full and the local budget is exhausted: stop, not proven exhausted.
    StopBudgetExhausted,
    /// Page was full but the floor didn't advance: stop, not proven exhausted.
    StopFloorStalled,
    /// Page was full, budget remains, floor advanced: fetch the next page.
    Continue(String),
}

fn decide_page_outcome(
    page_len: usize,
    current_floor: Option<&str>,
    last_updated_at: Option<&str>,
    budget_exhausted: bool,
) -> PageOutcome {
    if page_len < PAGE_LIMIT {
        return PageOutcome::WindowComplete;
    }
    if budget_exhausted {
        return PageOutcome::StopBudgetExhausted;
    }
    match last_updated_at {
        Some(next) if Some(next) != current_floor => PageOutcome::Continue(next.to_string()),
        _ => PageOutcome::StopFloorStalled,
    }
}

/// Test-only helper: production code matches on `PageOutcome` directly.
#[cfg(test)]
fn page_outcome_proves_window_complete(outcome: PageOutcome) -> bool {
    matches!(outcome, PageOutcome::WindowComplete)
}

fn search_query(floor: Option<&str>) -> String {
    match floor {
        Some(f) => format!("sort:updated-asc updated:>={f}"),
        None => "sort:updated-asc".to_string(),
    }
}

const PR_FIELDS: &str = "number,title,author,createdAt,mergedAt,closedAt,updatedAt,baseRefName,headRefName,mergeCommit,body";
const ISSUE_FIELDS: &str =
    "number,title,author,createdAt,closedAt,updatedAt,labels,stateReason,body";

fn fetch_pr_page(repo: &Path, gh_repo: &str, floor: Option<&str>) -> Result<Vec<GhPr>> {
    let search = search_query(floor);
    let raw = gh_json(
        repo,
        gh_repo,
        &[
            "pr",
            "list",
            "--state",
            "all",
            "--search",
            search.as_str(),
            "--limit",
            "1000",
            "--json",
            PR_FIELDS,
        ],
    )?;
    serde_json::from_str(&raw).context("parsing gh pr list --json")
}

fn fetch_issue_page(repo: &Path, gh_repo: &str, floor: Option<&str>) -> Result<Vec<GhIssue>> {
    let search = search_query(floor);
    let raw = gh_json(
        repo,
        gh_repo,
        &[
            "issue",
            "list",
            "--state",
            "all",
            "--search",
            search.as_str(),
            "--limit",
            "1000",
            "--json",
            ISSUE_FIELDS,
        ],
    )?;
    serde_json::from_str(&raw).context("parsing gh issue list --json")
}

/// Every `GhPr` field funnels through this constructor before it can reach
/// `properties`/`content`/the note `name`/the in-memory PR-linking maps or
/// the paging cursor. Contributor-controlled prose fields are masked, and
/// `updatedAt` is canonicalized before it can affect sorting or a later
/// `gh --search` argument. See
/// crates/khive-pack-git/docs/api/ingest.md#masking-boundaries-maskedcommitfields-maskedissuefields-maskedprfields.
struct MaskedPrFields {
    number: u64,
    title: String,
    body: String,
    author_login: Option<String>,
    created_at: Option<String>,
    merged_at: Option<String>,
    closed_at: Option<String>,
    updated_at: Option<String>,
    base_ref_name: Option<String>,
    head_ref_name: Option<String>,
    merge_commit_oid: Option<String>,
}

impl MaskedPrFields {
    fn new(pr: GhPr, warnings: &mut Vec<String>) -> Self {
        let GhPr {
            number,
            title,
            author,
            created_at,
            merged_at,
            closed_at,
            updated_at,
            base_ref_name,
            head_ref_name,
            merge_commit,
            body,
        } = pr;
        Self {
            number,
            title: secret_gate::mask_secrets(&title).into_owned(),
            body: secret_gate::mask_secrets(&body.unwrap_or_default()).into_owned(),
            author_login: author
                .and_then(|a| a.login)
                .map(|login| secret_gate::mask_secrets(&login).into_owned()),
            created_at,
            merged_at,
            closed_at,
            updated_at: canonical_pr_updated_at(number, updated_at, warnings),
            base_ref_name: base_ref_name.map(|r| secret_gate::mask_secrets(&r).into_owned()),
            head_ref_name: head_ref_name.map(|r| secret_gate::mask_secrets(&r).into_owned()),
            merge_commit_oid: merge_commit.and_then(|m| m.oid),
        }
    }
}

/// Parses a GitHub pull-request `updatedAt` into canonical RFC3339 form.
/// Invalid values are dropped before page sorting so raw remote data can
/// never become a persisted cursor or a later `gh --search` argument.
fn canonical_pr_updated_at(
    number: u64,
    raw: Option<String>,
    warnings: &mut Vec<String>,
) -> Option<String> {
    let raw = raw?;
    match chrono::DateTime::parse_from_rfc3339(&raw) {
        Ok(dt) => Some(
            dt.with_timezone(&Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        ),
        Err(_) => {
            warnings.push(format!(
                "pull request #{number}: updatedAt is not a valid RFC3339 timestamp, field dropped"
            ));
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn ingest_prs(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &VerbRegistry,
    repo: &Path,
    gh_repo: &str,
    project_id: Uuid,
    report: &mut IngestReport,
    merge_sha_to_pr: &mut HashMap<String, Uuid>,
    number_to_pr: &mut HashMap<u64, Uuid>,
    budget: &mut Budget,
    new_records: &mut Vec<NewRecordForRef>,
    walk_complete: &mut bool,
) -> Result<()> {
    let since = match read_cursor(runtime, project_id, "prs").await {
        Ok(since) => since,
        Err(e) => {
            report.sources.pull_requests = Some(IngestSourceState::Skipped(format!(
                "local cursor/database read failed before pull request listing: {e}"
            )));
            return Err(e);
        }
    };

    // `cursor_stalled` mirrors `ingest_commits`: once one PR fails to create,
    // later PRs in this pass are still attempted (so every failure surfaces
    // in this pass's warnings), but `max_updated` no longer advances past the
    // stall point — the next pass re-fetches from before the failure and
    // retries it, while already-landed PRs are no-ops via the natural key.
    let mut max_updated: Option<String> = since.clone();
    let mut cursor_stalled = false;
    let mut floor = since.clone();
    let mut window_complete = true;
    let mut stop_reason: Option<&'static str> = None;

    'paging: loop {
        // The FIRST fetch failing must report `skipped` (never walked);
        // every later failure happens mid/post-walk. The call site reads
        // that distinction off this state slot, so the marker goes up as
        // soon as the walk begins and pre-seeds the stopped-early state:
        // leaving the loop before the window completes IS stopping early,
        // and the arms below specialize the reason when they fire.
        let first_page = report.sources.pull_requests.is_none();
        let page = match fetch_pr_page(repo, gh_repo, floor.as_deref()) {
            Ok(page) => {
                if first_page {
                    report.sources.pull_requests = Some(IngestSourceState::StoppedEarly(
                        "walk began but did not report completion".into(),
                    ));
                }
                page
            }
            Err(e) => return Err(e),
        };
        let page_len = page.len();
        // Mask/canonicalize the complete remote page before either sorting
        // or selecting its continuation floor. In particular, raw
        // `updatedAt` must never enter the cursor/argv boundary.
        let mut page: Vec<MaskedPrFields> = page
            .into_iter()
            .map(|pr| MaskedPrFields::new(pr, &mut report.warnings))
            .collect();
        // Re-sorted defensively for the frozen-cursor invariant; `is_new` is
        // inclusive for tie handling. See crates/khive-pack-git/docs/api/
        // ingest.md#ingest_prs--ingest_issues-cursor-semantics.
        page.sort_by(|a, b| a.updated_at.cmp(&b.updated_at));
        let last_updated_at = page.last().and_then(|pr| pr.updated_at.clone());

        for masked in page {
            if let Some(existing) =
                find_by_number(runtime, token, "pull_request", project_id, masked.number).await?
            {
                number_to_pr.insert(masked.number, existing);
                if let Some(oid) = masked.merge_commit_oid.as_ref().cloned() {
                    merge_sha_to_pr.insert(oid, existing);
                }
                report.prs_skipped_existing += 1;
                // Advancing the floor past a stalled pass's later records
                // would skip the refused record forever; see
                // crates/khive-pack-git/docs/api/ingest.md#ingest_prs--ingest_issues-cursor-semantics.
                if !cursor_stalled {
                    if let Some(u) = &masked.updated_at {
                        if max_updated
                            .as_deref()
                            .map(|m| u.as_str() > m)
                            .unwrap_or(true)
                        {
                            max_updated = Some(u.clone());
                        }
                    }
                }
                continue;
            }
            // Computed AFTER the existence lookup: a record fetched by gh
            // because of the inclusive cursor tie (`updated >= since`) that
            // then lands is covered by its own create — walking past it
            // must not freeze the cursor at the pass floor.
            let is_new = since
                .as_deref()
                .zip(masked.updated_at.as_deref())
                .map(|(cursor, updated)| updated >= cursor)
                .unwrap_or(true);
            if !is_new {
                continue;
            }
            if budget.exhausted() {
                // Records after this point were never visited even on a short
                // page — a short page proves only the remote window ended, not
                // that the local walk covered it.
                window_complete = false;
                stop_reason = Some("budget exhausted before the pull request window completed");
                break;
            }
            let content = masked.body;
            let properties = json!({
                "number": masked.number,
                "title": masked.title,
                "author": masked.author_login,
                "created_at": masked.created_at,
                "merged_at": masked.merged_at,
                "closed_at": masked.closed_at,
                "base_ref": masked.base_ref_name,
                "head_ref": masked.head_ref_name,
                "project_id": project_id.to_string(),
            });
            let name = refs::truncate_chars(
                &format!("#{} {}", masked.number, masked.title),
                NAME_MAX_CHARS,
            );

            budget.try_consume();
            let result = match crate::dispatch_from_token(
                registry,
                token,
                "create",
                json!({
                        "kind": "pull_request",
                        "name": name,
                        "content": content,
                        "properties": properties,
                        "annotates": [project_id.to_string()],
                }),
            )
            .await
            {
                Ok(v) => v,
                Err(e) => {
                    record_write_failure(
                        report,
                        "create",
                        "pull_request",
                        format!("#{}", masked.number),
                        e,
                    );
                    cursor_stalled = true;
                    continue;
                }
            };

            if let Some(id) = result
                .get("id")
                .and_then(|v| v.as_str())
                .and_then(|s| Uuid::parse_str(s).ok())
            {
                number_to_pr.insert(masked.number, id);
                if let Some(oid) = masked.merge_commit_oid {
                    merge_sha_to_pr.insert(oid, id);
                }
                new_records.push(NewRecordForRef {
                    id,
                    text: content.clone(),
                });
            }
            report.prs_ingested += 1;
            if !cursor_stalled {
                if let Some(u) = &masked.updated_at {
                    if max_updated
                        .as_deref()
                        .map(|m| u.as_str() > m)
                        .unwrap_or(true)
                    {
                        max_updated = Some(u.clone());
                    }
                }
            }
        }

        match decide_page_outcome(
            page_len,
            floor.as_deref(),
            last_updated_at.as_deref(),
            budget.exhausted(),
        ) {
            PageOutcome::WindowComplete => break 'paging,
            PageOutcome::StopBudgetExhausted => {
                window_complete = false;
                stop_reason = Some("budget exhausted before the pull request window completed");
                break 'paging;
            }
            PageOutcome::StopFloorStalled => {
                window_complete = false;
                stop_reason = Some(
                    "full page returned but the pull request paging floor stalled before the window completed",
                );
                break 'paging;
            }
            PageOutcome::Continue(next_floor) => floor = Some(next_floor),
        }
    }

    if !window_complete {
        // The remote window may hold more PRs than this pass ever fetched
        // (ADR-088 Amendment 1); the local budget alone is
        // not a complete signal; report `done = false` regardless of budget
        // state so the caller's resume loop keeps going.
        report.done = false;
        // Seed invariant (see `pin_stopped_early`): this arm is reachable
        // only after at least one successful page fetch, and the first
        // successful fetch seeds the slot above — the helper's `None` arm
        // is a release-mode soft fallback, never a panic.
        pin_stopped_early(
            &mut report.sources.pull_requests,
            stop_reason
                .unwrap_or("the pull request paging window was not proven complete")
                .into(),
        );
    }
    if cursor_stalled {
        // A stalled PR cursor means records past the frozen floor were never
        // retried; `done: true` here would tell the caller the slot is
        // complete when it is permanently behind (issue #1645).
        report.cursor_stalled = true;
        report.done = false;
        // See the `!window_complete` arm above for the seed invariant.
        pin_stopped_early(
            &mut report.sources.pull_requests,
            "a per-record write failure froze the pull_requests cursor (cursor_stalled)".into(),
        );
    }
    if window_complete && !cursor_stalled {
        // Completion is recorded HERE too — every walker exit leaves the
        // state slot final, so the end-of-pass fill is a pure
        // instrumentation-gap fallback, not part of the normal path.
        report.sources.pull_requests = Some(IngestSourceState::Completed);
        *walk_complete = true;
    }

    if let Some(cursor) = max_updated {
        write_cursor(runtime, project_id, "prs", &cursor).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn ingest_issues(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &VerbRegistry,
    repo: &Path,
    gh_repo: &str,
    project_id: Uuid,
    report: &mut IngestReport,
    budget: &mut Budget,
    new_records: &mut Vec<NewRecordForRef>,
    walk_complete: &mut bool,
) -> Result<()> {
    let since = match read_cursor(runtime, project_id, "issues").await {
        Ok(since) => since,
        Err(e) => {
            report.sources.issues = Some(IngestSourceState::Skipped(format!(
                "local cursor/database read failed before issue listing: {e}"
            )));
            return Err(e);
        }
    };

    // `cursor_stalled` mirrors `ingest_commits`/`ingest_prs`: a per-record
    // create failure is aggregated as a warning and later records in this
    // pass are still attempted, but `max_updated` freezes at the stall point
    // so the next pass retries the failed record instead of skipping it
    // forever; already-landed records are no-ops via the natural key.
    let mut max_updated: Option<String> = since.clone();
    let mut cursor_stalled = false;
    let mut floor = since.clone();
    let mut window_complete = true;
    let mut stop_reason: Option<&'static str> = None;

    'paging: loop {
        // See `ingest_prs`: the first fetch failing must report `skipped`
        // (never walked); the call site reads that off this state slot, so
        // the walk-start marker also pre-seeds the stopped-early state that
        // leaving the loop early implies.
        let first_page = report.sources.issues.is_none();
        let page = match fetch_issue_page(repo, gh_repo, floor.as_deref()) {
            Ok(page) => {
                if first_page {
                    report.sources.issues = Some(IngestSourceState::StoppedEarly(
                        "walk began but did not report completion".into(),
                    ));
                }
                page
            }
            Err(e) => return Err(e),
        };
        let page_len = page.len();
        // The entire page is classified before sort/paging-cursor derivation
        // touches it, so a raw `updated_at` never reaches an argv boundary.
        // See crates/khive-pack-git/docs/api/ingest.md
        // #ingest_prs--ingest_issues-cursor-semantics.
        let mut masked_page: Vec<MaskedIssueFields> = page
            .into_iter()
            .map(|issue| MaskedIssueFields::new(issue, &mut report.warnings))
            .collect();
        // See `ingest_prs`: the frozen-cursor retry guarantee requires
        // walking records in nondecreasing updated_at order, which `--search
        // sort:updated-asc` does not itself guarantee across ties — sort
        // defensively, using the canonicalized (not raw) timestamp.
        masked_page.sort_by(|a, b| a.updated_at.cmp(&b.updated_at));
        let last_updated_at = masked_page.last().and_then(|i| i.updated_at.clone());

        for masked in masked_page {
            if find_by_number(runtime, token, "issue", project_id, masked.number)
                .await?
                .is_some()
            {
                report.issues_skipped_existing += 1;
                // See `ingest_prs`: while this pass is stalled, advancing
                // the floor past records walked after the stall point would
                // persist a cursor strictly newer than the refused record's
                // timestamp and skip it forever instead of retrying it. A
                // clean (non-stalled) all-existing pass still advances
                // normally.
                if !cursor_stalled {
                    if let Some(u) = &masked.updated_at {
                        if max_updated
                            .as_deref()
                            .map(|m| u.as_str() > m)
                            .unwrap_or(true)
                        {
                            max_updated = Some(u.clone());
                        }
                    }
                }
                continue;
            }
            // Computed AFTER the existence lookup (see `ingest_prs`): a
            // cursor-tie record that lands is covered by its own create.
            let is_new = since
                .as_deref()
                .zip(masked.updated_at.as_deref())
                .map(|(cursor, updated)| updated >= cursor)
                .unwrap_or(true);
            if !is_new {
                continue;
            }
            if budget.exhausted() {
                // See `ingest_prs`: unvisited records remain, so the walk
                // stops early even when the page is short; the
                // exact-budget boundary resolves conservatively and a
                // resumed pass completes idempotently.
                window_complete = false;
                stop_reason = Some("budget exhausted before the issue window completed");
                break;
            }

            let number = masked.number;
            let updated_at = masked.updated_at.clone();

            // `stateReason` was already classified at the masking boundary;
            // an ungoverned value is rejected here before the record is
            // built, matching ADR-088's fail-closed contract. See
            // crates/khive-pack-git/docs/api/ingest.md#ingest_prs--ingest_issues-cursor-semantics.
            if masked.state_reason == StateReasonField::Rejected {
                report.warnings.push(format!(
                    "issue #{number}: stateReason is not one of the governed values, record skipped"
                ));
                cursor_stalled = true;
                continue;
            }

            let content = masked.body;
            let safe_title = masked.title;
            let mut properties = json!({
                "number": number,
                "title": safe_title,
                "author": masked.author_login,
                "created_at": masked.created_at,
                "closed_at": masked.closed_at,
                "labels": masked.labels,
                "project_id": project_id.to_string(),
            });
            if let StateReasonField::Valid(reason) = masked.state_reason {
                properties["state_reason"] = json!(reason);
            }
            let name = refs::truncate_chars(&format!("#{number} {safe_title}"), NAME_MAX_CHARS);

            budget.try_consume();
            let result = match crate::dispatch_from_token(
                registry,
                token,
                "create",
                json!({
                        "kind": "issue",
                        "name": name,
                        "content": content,
                        "properties": properties,
                        "annotates": [project_id.to_string()],
                }),
            )
            .await
            {
                Ok(v) => v,
                Err(e) => {
                    record_write_failure(report, "create", "issue", format!("#{number}"), e);
                    cursor_stalled = true;
                    continue;
                }
            };
            if let Some(id) = result
                .get("id")
                .and_then(|v| v.as_str())
                .and_then(|s| Uuid::parse_str(s).ok())
            {
                new_records.push(NewRecordForRef {
                    id,
                    text: content.clone(),
                });
            }

            report.issues_ingested += 1;
            if !cursor_stalled {
                if let Some(u) = &updated_at {
                    if max_updated
                        .as_deref()
                        .map(|m| u.as_str() > m)
                        .unwrap_or(true)
                    {
                        max_updated = Some(u.clone());
                    }
                }
            }
        }

        match decide_page_outcome(
            page_len,
            floor.as_deref(),
            last_updated_at.as_deref(),
            budget.exhausted(),
        ) {
            PageOutcome::WindowComplete => break 'paging,
            PageOutcome::StopBudgetExhausted => {
                window_complete = false;
                stop_reason = Some("budget exhausted before the issue window completed");
                break 'paging;
            }
            PageOutcome::StopFloorStalled => {
                window_complete = false;
                stop_reason = Some(
                    "full page returned but the issue paging floor stalled before the window completed",
                );
                break 'paging;
            }
            PageOutcome::Continue(next_floor) => floor = Some(next_floor),
        }
    }

    if !window_complete {
        report.done = false;
        // Seed invariant (see `pin_stopped_early`): this arm is reachable
        // only after at least one successful page fetch, and the first
        // successful fetch seeds the slot above.
        pin_stopped_early(
            &mut report.sources.issues,
            stop_reason
                .unwrap_or("the issue paging window was not proven complete")
                .into(),
        );
    }
    if cursor_stalled {
        // Same contract as the commits and PR paths: a frozen issue cursor
        // means unretried records exist past the floor, so the slot is not
        // complete (issue #1645).
        report.cursor_stalled = true;
        report.done = false;
        // See the `!window_complete` arm above for the seed invariant.
        pin_stopped_early(
            &mut report.sources.issues,
            "a per-record write failure froze the issues cursor (cursor_stalled)".into(),
        );
    }
    if window_complete && !cursor_stalled {
        // See `ingest_prs`: completion is recorded at the exit, not left
        // for the end-of-pass fill.
        report.sources.issues = Some(IngestSourceState::Completed);
        *walk_complete = true;
    }

    if let Some(cursor) = max_updated {
        write_cursor(runtime, project_id, "issues", &cursor).await?;
    }
    Ok(())
}

#[cfg(test)]
mod paging_tests {
    use super::*;

    #[test]
    fn search_query_omits_updated_qualifier_with_no_floor() {
        assert_eq!(search_query(None), "sort:updated-asc");
    }

    #[test]
    fn search_query_includes_inclusive_updated_floor() {
        assert_eq!(
            search_query(Some("2024-01-01T00:00:00Z")),
            "sort:updated-asc updated:>=2024-01-01T00:00:00Z"
        );
    }

    #[test]
    fn short_page_proves_window_complete_regardless_of_budget() {
        let outcome = decide_page_outcome(42, None, Some("2024-01-01T00:00:00Z"), false);
        assert_eq!(outcome, PageOutcome::WindowComplete);
        assert!(page_outcome_proves_window_complete(outcome));

        // Even a page that runs out of budget mid-way is still a proof of
        // completeness if the page itself was short — the loop always
        // finishes sorting/processing the whole (short) page first.
        let outcome = decide_page_outcome(0, None, None, true);
        assert_eq!(outcome, PageOutcome::WindowComplete);
    }

    /// This is the exact ADR-088 Amendment 1 scenario: a
    /// full (`PAGE_LIMIT`-sized) page came back, but the local budget was
    /// NOT exhausted (e.g. every record in the page already existed and
    /// consumed no budget) and paging is still forced to stop because the
    /// floor didn't move. `done` must be false here — the remote window is
    /// not proven exhausted just because the local budget wasn't hit.
    #[test]
    fn full_page_with_stalled_floor_is_not_window_complete_even_with_budget_left() {
        let outcome = decide_page_outcome(PAGE_LIMIT, Some("X"), Some("X"), false);
        assert_eq!(outcome, PageOutcome::StopFloorStalled);
        assert!(!page_outcome_proves_window_complete(outcome));
    }

    #[test]
    fn full_page_with_advancing_floor_and_budget_left_continues() {
        let outcome = decide_page_outcome(PAGE_LIMIT, Some("A"), Some("B"), false);
        assert_eq!(outcome, PageOutcome::Continue("B".to_string()));
        assert!(!page_outcome_proves_window_complete(outcome));
    }

    #[test]
    fn full_page_with_exhausted_budget_stops_without_proving_completeness() {
        let outcome = decide_page_outcome(PAGE_LIMIT, Some("A"), Some("B"), true);
        assert_eq!(outcome, PageOutcome::StopBudgetExhausted);
        assert!(!page_outcome_proves_window_complete(outcome));
    }

    #[test]
    fn full_page_with_no_updated_at_stalls_rather_than_looping_forever() {
        let outcome = decide_page_outcome(PAGE_LIMIT, Some("A"), None, false);
        assert_eq!(outcome, PageOutcome::StopFloorStalled);
    }
}

/// Issue #765: `GitLogError` classification + `recover_commit_snapshot`
/// retry loop (pure/synchronous). See
/// crates/khive-pack-git/docs/api/ingest.md#test-module-notes.
#[cfg(test)]
mod recovery_classifier_tests {
    use super::*;

    fn err(phase: GitLogPhase, stderr: &str) -> GitLogError {
        GitLogError {
            phase,
            stderr: stderr.to_string(),
        }
    }

    const REAL_WORLD_MESSAGE: &str = "fatal: deadbeefdeadbeefdeadbeefdeadbeefdeadbeef is in \
         the commit graph file, but not in the object database\nfatal: unable to parse commit: \
         deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\nfatal: could not fetch from promisor remote";

    #[test]
    fn classifies_real_world_missing_promisor_object_message_on_either_phase() {
        assert!(err(GitLogPhase::TouchedFiles, REAL_WORLD_MESSAGE).is_missing_promisor_object());
        assert!(err(GitLogPhase::Metadata, REAL_WORLD_MESSAGE).is_missing_promisor_object());
    }

    #[test]
    fn classifies_missing_object_wording_case_insensitively() {
        assert!(err(
            GitLogPhase::TouchedFiles,
            "FATAL: MISSING OBJECT abc123; PROMISOR remote unavailable"
        )
        .is_missing_promisor_object());
    }

    #[test]
    fn does_not_classify_bad_object_without_promisor() {
        assert!(!err(GitLogPhase::Metadata, "fatal: bad object HEAD").is_missing_promisor_object());
    }

    #[test]
    fn does_not_classify_auth_or_network_failures() {
        assert!(!err(
            GitLogPhase::Metadata,
            "fatal: Authentication failed for 'https://example.com/org/repo.git/'"
        )
        .is_missing_promisor_object());
        assert!(!err(
            GitLogPhase::TouchedFiles,
            "fatal: unable to access 'https://example.com/org/repo.git/': Could not resolve host"
        )
        .is_missing_promisor_object());
    }

    #[test]
    fn does_not_classify_promisor_mention_without_missing_object_wording() {
        // "promisor" alone (e.g. a config-dump or unrelated log line) must
        // not be treated as proof of corruption -- both keyword classes are
        // required.
        assert!(!err(
            GitLogPhase::Metadata,
            "fatal: promisor remote configured but unreachable"
        )
        .is_missing_promisor_object());
    }

    /// Healthy repo: loads on first try, no recover call, no warning. Holds
    /// `cache::ENV_MUTEX` — see crates/khive-pack-git/docs/api/ingest.md#test-module-notes.
    #[test]
    fn recover_commit_snapshot_returns_no_warning_when_healthy() {
        let _env = crate::cache::ENV_MUTEX.blocking_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo_with_commit(dir.path());
        let mut recover_calls = 0;
        let (snapshot, warning) = recover_commit_snapshot(dir.path(), None, |_repo, _err| {
            recover_calls += 1;
            Ok(None)
        })
        .expect("healthy repo loads");
        assert_eq!(snapshot.commits.len(), 1);
        assert_eq!(warning, None);
        assert_eq!(recover_calls, 0);
    }

    /// An unclassified `git log` failure must never reach `recover` and
    /// must propagate as-is. Same `ENV_MUTEX` requirement as above.
    #[test]
    fn recover_commit_snapshot_never_calls_recover_for_unclassified_failures() {
        let _env = crate::cache::ENV_MUTEX.blocking_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        // Not a git repo at all -- `git log` fails with a plain spawn/repo
        // error, not a classified promisor one.
        let mut recover_calls = 0;
        let result = recover_commit_snapshot(dir.path(), None, |_repo, _err| {
            recover_calls += 1;
            Ok(Some(RecoveredRepo {
                repo: dir.path().to_path_buf(),
                strategy: CacheRepairStrategy::Refetch,
            }))
        });
        assert!(result.is_err(), "a non-repo path must fail to load");
        assert_eq!(
            recover_calls, 0,
            "an unclassified failure must never invoke recover"
        );
    }

    fn init_repo_with_commit(repo: &Path) {
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .expect("spawn git");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test User"]);
        std::fs::write(repo.join("a.txt"), b"hello").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-q", "-m", "initial"]);
    }
}

#[cfg(test)]
mod truncation_tests {
    use super::*;

    #[test]
    fn under_cap_content_is_not_truncated() {
        let content = "a".repeat(MAX_COMMIT_EMBED_BYTES - 1);
        assert_eq!(truncated_embedding_head(&content), None);
    }

    #[test]
    fn exactly_at_cap_content_is_not_truncated() {
        let content = "a".repeat(MAX_COMMIT_EMBED_BYTES);
        assert_eq!(truncated_embedding_head(&content), None);
    }

    #[test]
    fn over_cap_content_is_truncated_to_exactly_the_cap() {
        let content = "a".repeat(MAX_COMMIT_EMBED_BYTES + 1);
        let head = truncated_embedding_head(&content).expect("over cap must truncate");
        assert_eq!(head.len(), MAX_COMMIT_EMBED_BYTES);
        assert!(content.starts_with(head));
    }

    /// Multibyte scalar straddling the byte cap must roll back to a char boundary.
    #[test]
    fn multibyte_scalar_straddling_cap_rolls_back_to_char_boundary() {
        // Fill up to one byte short of the cap with ASCII, then place a
        // 3-byte character exactly across the boundary.
        let mut content = "a".repeat(MAX_COMMIT_EMBED_BYTES - 1);
        content.push('€'); // 3 bytes: straddles byte 32_768..32_771
        content.push_str("tail-sentinel");

        let head = truncated_embedding_head(&content).expect("over cap must truncate");
        assert!(head.len() <= MAX_COMMIT_EMBED_BYTES);
        assert!(content.is_char_boundary(head.len()));
        assert!(std::str::from_utf8(head.as_bytes()).is_ok());
        assert!(content.starts_with(head));
        assert!(
            !head.contains("tail-sentinel"),
            "head must not include text past the cap"
        );
    }
}

/// PR #816: `resolve_id`/`resolve_project_id` LIKE-wildcard-injection
/// regression tests. See crates/khive-pack-git/docs/api/ingest.md#test-module-notes.
#[cfg(test)]
mod compact_prefix_resolver_tests {
    use super::*;
    use khive_runtime::Namespace;

    #[tokio::test]
    async fn resolve_project_id_rejects_like_wildcard_input() {
        let rt = KhiveRuntime::memory().unwrap();
        let token = rt.authorize(Namespace::local()).unwrap();
        let project = rt
            .create_entity(
                &token,
                "project",
                None,
                "WildcardIngestTest",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let compact = project.id.simple().to_string();
        let wildcard_input = format!("{}%", &compact[..8]);

        let resolved = resolve_project_id(&rt, &wildcard_input).await.unwrap();
        assert_eq!(
            resolved, None,
            "a %-bearing project argument must not resolve via a wildcard LIKE scan"
        );
    }

    #[tokio::test]
    async fn resolve_id_resolves_compact_prefix_over_8_chars() {
        let rt = KhiveRuntime::memory().unwrap();
        let token = rt.authorize(Namespace::local()).unwrap();
        let project = rt
            .create_entity(
                &token,
                "project",
                None,
                "CompactIngestTest",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        let compact = project.id.simple().to_string();

        let resolved = resolve_id(&rt, &token, &compact[..16]).await.unwrap();
        assert_eq!(resolved, Some(project.id));
    }
}

/// PR #816: `find_document_for_path` LIKE-escaping + exact-match-ordering
/// regression tests. See crates/khive-pack-git/docs/api/ingest.md#test-module-notes.
#[cfg(test)]
mod find_document_for_path_tests {
    use super::*;
    use khive_runtime::Namespace;

    async fn create_document(rt: &KhiveRuntime, token: &NamespaceToken, source_uri: &str) -> Uuid {
        rt.create_entity(
            token,
            "document",
            None,
            source_uri,
            None,
            Some(json!({ "source_uri": source_uri })),
            vec![],
        )
        .await
        .unwrap()
        .id
    }

    #[tokio::test]
    async fn path_with_like_wildcards_resolves_only_itself() {
        let rt = KhiveRuntime::memory().unwrap();
        let token = rt.authorize(Namespace::local()).unwrap();

        // Neither document's `source_uri` matches `path` exactly, so
        // resolution must fall through to the suffix-`LIKE` scan. Under the
        // pre-fix unescaped pattern, `%` matches zero-or-more chars and `_`
        // matches exactly one char, so this decoy (`100` + "" + "Q" +
        // `done.rs`) would incorrectly satisfy `LIKE '%src/100%_done.rs'`.
        // With `%`/`_` escaped, the pattern requires the literal substring
        // `100%_done.rs` and the decoy no longer matches.
        let path = "src/100%_done.rs";
        let decoy_source_uri = "prefix/src/100Qdone.rs";
        create_document(&rt, &token, decoy_source_uri).await;

        let resolved = find_document_for_path(&rt, &token, path).await.unwrap();
        assert_eq!(
            resolved, None,
            "a % or _ in the path must be matched literally, not as a LIKE wildcard"
        );
    }

    #[tokio::test]
    async fn exact_match_wins_over_wildcard_broadened_candidate() {
        let rt = KhiveRuntime::memory().unwrap();
        let token = rt.authorize(Namespace::local()).unwrap();

        let path = "crates/khive-pack-git/src/ingest.rs";
        let broadened_suffix_path = "other/crates/khive-pack-git/src/ingest.rs";
        // Created first so an unordered `LIMIT 1` scan without exact-match
        // priority would be free to return it instead of the exact match.
        create_document(&rt, &token, broadened_suffix_path).await;
        let exact_id = create_document(&rt, &token, path).await;

        let resolved = find_document_for_path(&rt, &token, path).await.unwrap();
        assert_eq!(
            resolved,
            Some(exact_id),
            "an exact source_uri match must always win over a suffix-LIKE candidate"
        );
    }

    /// PR #816: TOCTOU regression — single-query snapshot must still rank
    /// the exact match first regardless of insertion order.
    #[tokio::test]
    async fn single_query_snapshot_prefers_exact_over_broadened() {
        let rt = KhiveRuntime::memory().unwrap();
        let token = rt.authorize(Namespace::local()).unwrap();

        let path = "crates/khive-pack-git/src/toctou.rs";
        let broadened_suffix_path = "other/crates/khive-pack-git/src/toctou.rs";
        let exact_id = create_document(&rt, &token, path).await;
        create_document(&rt, &token, broadened_suffix_path).await;

        let resolved = find_document_for_path(&rt, &token, path).await.unwrap();
        assert_eq!(
            resolved,
            Some(exact_id),
            "a single query covering both exact and broadened candidates \
             must still rank the exact match first, regardless of insertion order"
        );
    }
}

/// `load_code_modules_by_snapshot_path` ambiguity and error-surface
/// regression tests. See
/// crates/khive-pack-git/docs/api/ingest.md#test-module-notes.
#[cfg(test)]
mod module_index_loader_tests {
    use super::*;
    use khive_runtime::Namespace;

    const REVISION: &str = "1111111111111111111111111111111111111111";
    const AMBIGUOUS_PATH: &str = "crates/ambig/src/lib.rs";

    fn rt_and_token() -> (KhiveRuntime, NamespaceToken) {
        let rt = KhiveRuntime::memory().unwrap();
        let token = rt.authorize(Namespace::local()).unwrap();
        (rt, token)
    }

    async fn create_module(
        rt: &KhiveRuntime,
        token: &NamespaceToken,
        name: &str,
        path: &str,
    ) -> Uuid {
        rt.create_entity(
            token,
            "concept",
            Some("module"),
            name,
            None,
            Some(json!({
                "source_path": path,
                "source_revision": REVISION
            })),
            vec![],
        )
        .await
        .unwrap()
        .id
    }

    /// A live module row whose `id` does not parse as a UUID — a shape the
    /// normal `create` path never writes, inserted raw to prove the loader
    /// still counts it toward ambiguity.
    async fn insert_unparsable_module_row(
        rt: &KhiveRuntime,
        token: &NamespaceToken,
        name: &str,
        path: &str,
    ) {
        let mut writer = rt.sql().writer().await.unwrap();
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO entities \
                      (id, namespace, kind, entity_type, name, properties, created_at, updated_at) \
                      VALUES (?1, ?2, 'concept', 'module', ?3, ?4, 0, 0)"
                    .into(),
                params: vec![
                    SqlValue::Text("not-a-parseable-uuid".to_string()),
                    SqlValue::Text(token.namespace().as_str().to_string()),
                    SqlValue::Text(name.to_string()),
                    SqlValue::Text(
                        json!({
                            "source_path": path,
                            "source_revision": REVISION
                        })
                        .to_string(),
                    ),
                ],
                label: Some("test_insert_unparsable_module_row".into()),
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn single_valid_module_row_binds() {
        let (rt, token) = rt_and_token();
        let id = create_module(&rt, &token, "solo_module", AMBIGUOUS_PATH).await;

        let index = load_code_modules_by_snapshot_path(&rt, &token, REVISION)
            .await
            .unwrap();
        assert_eq!(index.get(AMBIGUOUS_PATH), Some(&Some(id)));
    }

    /// Contract: more than one live module with the same
    /// `(source_revision, source_path)` is ambiguous and annotates none. An
    /// unparsable-id row is still a live row for that key, so it must mark
    /// the pair ambiguous even though it can never bind itself.
    #[tokio::test]
    async fn unparsable_module_row_counts_toward_ambiguity() {
        let (rt, token) = rt_and_token();
        let valid_id = create_module(&rt, &token, "valid_module", AMBIGUOUS_PATH).await;
        insert_unparsable_module_row(&rt, &token, "malformed_module", AMBIGUOUS_PATH).await;

        let index = load_code_modules_by_snapshot_path(&rt, &token, REVISION)
            .await
            .unwrap();
        assert_eq!(
            index.get(AMBIGUOUS_PATH),
            Some(&None),
            "a malformed row is evidence of a second live module for \
             {AMBIGUOUS_PATH}; the valid row {valid_id} must not be selected"
        );
    }

    #[tokio::test]
    async fn two_parseable_module_rows_fold_to_ambiguity() {
        let (rt, token) = rt_and_token();
        let first_id = create_module(&rt, &token, "first_module", AMBIGUOUS_PATH).await;
        let second_id = create_module(&rt, &token, "second_module", AMBIGUOUS_PATH).await;

        let index = load_code_modules_by_snapshot_path(&rt, &token, REVISION)
            .await
            .unwrap();
        assert_eq!(
            index.get(AMBIGUOUS_PATH),
            Some(&None),
            "two live parseable rows ({first_id}, {second_id}) must not select a winner"
        );
    }

    #[tokio::test]
    async fn unparsable_module_row_alone_never_binds() {
        let (rt, token) = rt_and_token();
        insert_unparsable_module_row(&rt, &token, "lonely_malformed", AMBIGUOUS_PATH).await;

        let index = load_code_modules_by_snapshot_path(&rt, &token, REVISION)
            .await
            .unwrap();
        assert_eq!(
            index.get(AMBIGUOUS_PATH),
            Some(&None),
            "an unparsable id can never serve as an annotation target"
        );
    }

    /// A failed index load must surface its cause: the caller includes this
    /// error text in the degradation warning, so persistent SQL/schema
    /// problems stay diagnosable.
    #[tokio::test]
    async fn load_failure_surfaces_error_text() {
        let (rt, token) = rt_and_token();
        // Break the substrate directly: the index query then fails with a
        // real SQL error that must reach the caller.
        let mut writer = rt.sql().writer().await.unwrap();
        writer
            .execute(SqlStatement {
                sql: "DROP TABLE entities".into(),
                params: vec![],
                label: Some("test_drop_entities".into()),
            })
            .await
            .unwrap();

        let err = load_code_modules_by_snapshot_path(&rt, &token, REVISION)
            .await
            .expect_err("a failed index load must return Err");
        assert!(
            format!("{err}").contains("no such table"),
            "the error must carry the underlying SQL cause: {err}"
        );
    }
}

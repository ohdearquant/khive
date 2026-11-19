use super::*;

// ── issues + PRs (gh CLI) ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(super) struct GhAuthor {
    pub(super) login: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct GhLabel {
    pub(super) name: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct GhIssue {
    pub(super) number: u64,
    pub(super) title: String,
    pub(super) author: Option<GhAuthor>,
    #[serde(rename = "createdAt")]
    pub(super) created_at: Option<String>,
    #[serde(rename = "closedAt")]
    pub(super) closed_at: Option<String>,
    #[serde(rename = "updatedAt")]
    pub(super) updated_at: Option<String>,
    pub(super) labels: Option<Vec<GhLabel>>,
    #[serde(rename = "stateReason")]
    pub(super) state_reason: Option<String>,
    pub(super) body: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct GhMergeCommit {
    pub(super) oid: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct GhPr {
    pub(super) number: u64,
    pub(super) title: String,
    pub(super) author: Option<GhAuthor>,
    #[serde(rename = "createdAt")]
    pub(super) created_at: Option<String>,
    #[serde(rename = "mergedAt")]
    pub(super) merged_at: Option<String>,
    #[serde(rename = "closedAt")]
    pub(super) closed_at: Option<String>,
    #[serde(rename = "updatedAt")]
    pub(super) updated_at: Option<String>,
    #[serde(rename = "baseRefName")]
    pub(super) base_ref_name: Option<String>,
    #[serde(rename = "headRefName")]
    pub(super) head_ref_name: Option<String>,
    #[serde(rename = "mergeCommit")]
    pub(super) merge_commit: Option<GhMergeCommit>,
    pub(super) body: Option<String>,
}

/// Every `GhIssue` field funnels through this constructor before it can
/// reach `properties`/`content`/the note name/the paging cursor. See
/// crates/khive-pack-git/docs/api/ingest.md#masking-boundaries-maskedcommitfields-maskedissuefields-maskedprfields.
pub(super) struct MaskedIssueFields {
    pub(super) number: u64,
    pub(super) title: String,
    pub(super) body: String,
    pub(super) author_login: Option<String>,
    pub(super) labels: Vec<String>,
    pub(super) created_at: Option<String>,
    pub(super) closed_at: Option<String>,
    pub(super) updated_at: Option<String>,
    pub(super) state_reason: StateReasonField,
}

/// Classified outcome of parsing a raw `stateReason` against the governed
/// enum. `Rejected` never carries the raw string forward. See
/// crates/khive-pack-git/docs/api/ingest.md#masking-boundaries-maskedcommitfields-maskedissuefields-maskedprfields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum StateReasonField {
    Absent,
    Valid(String),
    Rejected,
}

impl MaskedIssueFields {
    pub(super) fn new(issue: GhIssue, warnings: &mut Vec<String>) -> Self {
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
            title: mask_git_ingest(&title).into_owned(),
            body: mask_git_ingest(&body.unwrap_or_default()).into_owned(),
            author_login: author
                .and_then(|a| a.login)
                .map(|login| mask_git_ingest(&login).into_owned()),
            labels: labels
                .unwrap_or_default()
                .into_iter()
                .map(|l| mask_git_ingest(&l.name).into_owned())
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
pub(super) fn canonical_issue_state_reason(raw: Option<String>) -> StateReasonField {
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
pub(super) fn canonical_issue_timestamp(
    field: &'static str,
    number: u64,
    raw: Option<String>,
    warnings: &mut Vec<String>,
) -> Option<String> {
    let raw = raw?;
    match chrono::DateTime::parse_from_rfc3339(&raw) {
        Ok(dt) => Some(khive_runtime::presentation::format_utc_rfc3339_seconds(&dt)),
        Err(_) => {
            warnings.push(format!(
                "issue #{number}: {field} is not a valid RFC3339 timestamp, field dropped"
            ));
            None
        }
    }
}

pub(super) async fn gh_json(repo: &Path, gh_repo: &str, args: &[&str]) -> Result<String> {
    gh_json_with_command(
        repo,
        gh_repo,
        args,
        Path::new("gh"),
        GH_COMMAND_TIMEOUT,
        GH_STDOUT_LIMIT,
    )
    .await
}

pub(super) async fn gh_json_with_command(
    repo: &Path,
    gh_repo: &str,
    args: &[&str],
    gh_program: &Path,
    timeout: Duration,
    stdout_limit: usize,
) -> Result<String> {
    // gh has no `-C` flag (unlike git). Keep cwd for local git configuration,
    // but target the repository explicitly so later remote/cwd drift cannot
    // redirect a resumed digest to a different repository.
    let mut command = AsyncCommand::new(gh_program);
    command
        .current_dir(repo)
        .args(args)
        .args(["--repo", gh_repo])
        .env_remove("GH_REPO")
        .env_remove("GH_HOST")
        .env("GH_PROMPT_DISABLED", "1");
    let output = run_ingest_command(command, timeout, stdout_limit)
        .await
        .map_err(|error| {
            // IngestCommandError displays only fixed, credential-safe reasons.
            // Public ingest reports use ordinary Display, not the cause chain.
            let context = format!("running gh: {error}");
            anyhow::Error::new(error).context(context)
        })?;
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
pub(super) const PAGE_LIMIT: usize = 1000;

pub(super) fn page_fetch_limit(
    budget: &Budget,
    checkpoint: &PageCheckpoint,
    floor: Option<&str>,
) -> usize {
    let Some(remaining) = budget.remaining else {
        return PAGE_LIMIT;
    };
    // Inclusive queries replay acknowledged rows for free. Reserve room for
    // those rows so even a one-visit budget can reach an unseen timestamp tie.
    let boundary_replays = if checkpoint.floor.as_deref() == floor {
        checkpoint.at_floor.len()
    } else {
        0
    };
    (remaining.min(PAGE_LIMIT as u64) as usize)
        .saturating_add(boundary_replays)
        .saturating_add(checkpoint.undated.len())
        .min(PAGE_LIMIT)
}

/// What a paging loop should do after processing one fetched page — the
/// entire "was the remote window proven exhausted" decision lives here. See
/// crates/khive-pack-git/docs/api/ingest.md#paging-pageoutcome-decide_page_outcome-page_limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PageOutcome {
    /// Page held fewer than the requested limit: remote window proven exhausted.
    WindowComplete,
    /// Page was full and the local budget is exhausted: stop, not proven exhausted.
    StopBudgetExhausted,
    /// Page was full but the floor didn't advance: stop, not proven exhausted.
    StopFloorStalled,
    /// Page was full, budget remains, floor advanced: fetch the next page.
    Continue(String),
}

pub(super) fn decide_page_outcome(
    page_len: usize,
    requested_limit: usize,
    current_floor: Option<&str>,
    last_updated_at: Option<&str>,
    budget_exhausted: bool,
) -> PageOutcome {
    if page_len < requested_limit {
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
pub(super) fn page_outcome_proves_window_complete(outcome: PageOutcome) -> bool {
    matches!(outcome, PageOutcome::WindowComplete)
}

pub(super) fn search_query(floor: Option<&str>) -> String {
    match floor {
        Some(f) => format!("sort:updated-asc updated:>={f}"),
        None => "sort:updated-asc".to_string(),
    }
}

pub(super) const PR_FIELDS: &str = "number,title,author,createdAt,mergedAt,closedAt,updatedAt,baseRefName,headRefName,mergeCommit,body";
pub(super) const ISSUE_FIELDS: &str =
    "number,title,author,createdAt,closedAt,updatedAt,labels,stateReason,body";

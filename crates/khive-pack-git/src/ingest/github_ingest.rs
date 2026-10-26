use super::*;

pub(super) async fn fetch_pr_page(
    repo: &Path,
    gh_repo: &str,
    floor: Option<&str>,
    limit: usize,
) -> Result<Vec<GhPr>> {
    fetch_page::<GhPr>(
        repo,
        gh_repo,
        floor,
        limit,
        "pr",
        PR_FIELDS,
        "parsing gh pr list --json",
    )
    .await
}

pub(super) async fn fetch_issue_page(
    repo: &Path,
    gh_repo: &str,
    floor: Option<&str>,
    limit: usize,
) -> Result<Vec<GhIssue>> {
    fetch_page::<GhIssue>(
        repo,
        gh_repo,
        floor,
        limit,
        "issue",
        ISSUE_FIELDS,
        "parsing gh issue list --json",
    )
    .await
}

/// Every `GhPr` field funnels through this constructor before it can reach
/// `properties`/`content`/the note `name`/the in-memory PR-linking maps or
/// the paging cursor. Contributor-controlled prose fields are masked, and
/// `updatedAt` is canonicalized before it can affect sorting or a later
/// `gh --search` argument. See
/// crates/khive-pack-git/docs/api/ingest.md#masking-boundaries-maskedcommitfields-maskedissuefields-maskedprfields.
pub(super) struct MaskedPrFields {
    pub(super) number: u64,
    pub(super) title: String,
    pub(super) body: String,
    pub(super) author_login: Option<String>,
    pub(super) created_at: Option<String>,
    pub(super) merged_at: Option<String>,
    pub(super) closed_at: Option<String>,
    pub(super) updated_at: Option<String>,
    pub(super) base_ref_name: Option<String>,
    pub(super) head_ref_name: Option<String>,
    pub(super) merge_commit_oid: Option<String>,
}

impl MaskedPrFields {
    pub(super) fn new(pr: GhPr, warnings: &mut Vec<String>) -> Self {
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
            title: mask_git_ingest(&title).into_owned(),
            body: mask_git_ingest(&body.unwrap_or_default()).into_owned(),
            author_login: author
                .and_then(|a| a.login)
                .map(|login| mask_git_ingest(&login).into_owned()),
            created_at,
            merged_at,
            closed_at,
            updated_at: canonical_pr_updated_at(number, updated_at, warnings),
            base_ref_name: base_ref_name.map(|r| mask_git_ingest(&r).into_owned()),
            head_ref_name: head_ref_name.map(|r| mask_git_ingest(&r).into_owned()),
            merge_commit_oid: merge_commit.and_then(|m| m.oid),
        }
    }
}

/// Parses a GitHub pull-request `updatedAt` into canonical RFC3339 form.
/// Invalid values are dropped before page sorting so raw remote data can
/// never become a persisted cursor or a later `gh --search` argument.
pub(super) fn canonical_pr_updated_at(
    number: u64,
    raw: Option<String>,
    warnings: &mut Vec<String>,
) -> Option<String> {
    let raw = raw?;
    match chrono::DateTime::parse_from_rfc3339(&raw) {
        Ok(dt) => Some(khive_runtime::presentation::format_utc_rfc3339_seconds(&dt)),
        Err(_) => {
            warnings.push(format!(
                "pull request #{number}: updatedAt is not a valid RFC3339 timestamp, field dropped"
            ));
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn ingest_prs(
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
    let mut checkpoint =
        match read_page_checkpoint(runtime, token, project_id, "prs", &mut report.warnings).await {
            Ok(checkpoint) => checkpoint,
            Err(e) => {
                report.sources.pull_requests = Some(IngestSourceState::Skipped(format!(
                    "local cursor/database read failed before pull request listing: {e}"
                )));
                return Err(e);
            }
        };

    // `cursor_stalled` mirrors `ingest_commits`: once one PR fails to create,
    // later PRs in this pass are still attempted (so every failure surfaces
    // in this pass's warnings), but `checkpoint.floor` no longer advances past the
    // stall point — the next pass re-fetches from before the failure and
    // retries it, while already-landed PRs are no-ops via the natural key.
    let since = checkpoint.floor.clone();
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
        let requested_limit = page_fetch_limit(budget, &checkpoint, floor.as_deref());
        let page = match fetch_pr_page(repo, gh_repo, floor.as_deref(), requested_limit).await {
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
        page.sort_by(|a, b| {
            a.updated_at
                .cmp(&b.updated_at)
                .then(a.number.cmp(&b.number))
        });
        let last_updated_at = page.last().and_then(|pr| pr.updated_at.clone());
        let undated: BTreeSet<u64> = page
            .iter()
            .filter(|p| p.updated_at.is_none())
            .map(|p| p.number)
            .collect();
        checkpoint
            .undated
            .retain(|number, _| undated.contains(number));

        for masked in page {
            if let Some(existing) =
                checkpoint.acknowledged(masked.number, masked.updated_at.as_deref())
            {
                number_to_pr.insert(masked.number, existing);
                if let Some(oid) = masked.merge_commit_oid.as_ref() {
                    merge_sha_to_pr.insert(oid.clone(), existing);
                }
                continue;
            }
            if !budget.try_consume() {
                window_complete = false;
                stop_reason = Some("budget exhausted before the pull_request window completed");
                break;
            }
            if let Some(existing) =
                find_by_number(runtime, token, "pull_request", project_id, masked.number).await?
            {
                number_to_pr.insert(masked.number, existing);
                if let Some(oid) = masked.merge_commit_oid.as_ref().cloned() {
                    merge_sha_to_pr.insert(oid, existing);
                }
                report.prs_skipped_existing += 1;
                if !cursor_stalled
                    && !checkpoint.acknowledge(
                        masked.number,
                        masked.updated_at.as_deref(),
                        existing,
                    )
                {
                    report
                        .warnings
                        .push("prs: checkpoint boundary exceeds the remote page limit".into());
                    stall_cursor(&mut cursor_stalled, report);
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

            let created = match create_github_record(
                registry,
                token,
                report,
                "pull_request",
                masked.number,
                project_id,
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
                Some(created) => created,
                None => {
                    stall_cursor(&mut cursor_stalled, report);
                    continue;
                }
            };
            let result = created.response;
            let stored_content = created.content;

            let id = result
                .get("id")
                .and_then(|v| v.as_str())
                .and_then(|s| Uuid::parse_str(s).ok());
            if let Some(id) = id {
                number_to_pr.insert(masked.number, id);
                if let Some(oid) = masked.merge_commit_oid {
                    merge_sha_to_pr.insert(oid, id);
                }
                new_records.push(NewRecordForRef {
                    id,
                    text: stored_content.clone(),
                });
            }
            report.prs_ingested += 1;
            if !cursor_stalled {
                match id {
                    Some(id)
                        if checkpoint.acknowledge(
                            masked.number,
                            masked.updated_at.as_deref(),
                            id,
                        ) => {}
                    _ => {
                        report.warnings.push(
                            "prs: record could not be acknowledged in the bounded checkpoint"
                                .into(),
                        );
                        stall_cursor(&mut cursor_stalled, report);
                    }
                }
            }
        }

        // Save completed and budget-truncated pages before another fetch can
        // fail. A stalled pass persists only its contiguous-success prefix.
        write_page_checkpoint(runtime, project_id, "prs", &checkpoint).await?;
        match decide_page_outcome(
            page_len,
            requested_limit,
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

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn ingest_issues(
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
    let mut checkpoint = match read_page_checkpoint(
        runtime,
        token,
        project_id,
        "issues",
        &mut report.warnings,
    )
    .await
    {
        Ok(checkpoint) => checkpoint,
        Err(e) => {
            report.sources.issues = Some(IngestSourceState::Skipped(format!(
                "local cursor/database read failed before issue listing: {e}"
            )));
            return Err(e);
        }
    };

    // `cursor_stalled` mirrors `ingest_commits`/`ingest_prs`: a per-record
    // create failure is aggregated as a warning and later records in this
    // pass are still attempted, but `checkpoint.floor` freezes at the stall point
    // so the next pass retries the failed record instead of skipping it
    // forever; already-landed records are no-ops via the natural key.
    let since = checkpoint.floor.clone();
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
        let requested_limit = page_fetch_limit(budget, &checkpoint, floor.as_deref());
        let page = match fetch_issue_page(repo, gh_repo, floor.as_deref(), requested_limit).await {
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
        masked_page.sort_by(|a, b| {
            a.updated_at
                .cmp(&b.updated_at)
                .then(a.number.cmp(&b.number))
        });
        let last_updated_at = masked_page.last().and_then(|i| i.updated_at.clone());
        // Undated acknowledgments cover this fetched window, not every undated
        // record ever encountered. Retain present rows across timestamp advances
        // (tiny budgets must not replay them forever), and prune absent rows.
        let undated: BTreeSet<u64> = masked_page
            .iter()
            .filter(|i| i.updated_at.is_none())
            .map(|i| i.number)
            .collect();
        checkpoint
            .undated
            .retain(|number, _| undated.contains(number));

        for masked in masked_page {
            if checkpoint
                .acknowledged(masked.number, masked.updated_at.as_deref())
                .is_some()
            {
                continue;
            }
            if !budget.try_consume() {
                window_complete = false;
                stop_reason = Some("budget exhausted before the issue window completed");
                break;
            }
            if let Some(existing) =
                find_by_number(runtime, token, "issue", project_id, masked.number).await?
            {
                report.issues_skipped_existing += 1;
                if !cursor_stalled
                    && !checkpoint.acknowledge(
                        masked.number,
                        masked.updated_at.as_deref(),
                        existing,
                    )
                {
                    report
                        .warnings
                        .push("issues: checkpoint boundary exceeds the remote page limit".into());
                    stall_cursor(&mut cursor_stalled, report);
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
                stall_cursor(&mut cursor_stalled, report);
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

            let created = match create_github_record(
                registry,
                token,
                report,
                "issue",
                number,
                project_id,
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
                Some(created) => created,
                None => {
                    stall_cursor(&mut cursor_stalled, report);
                    continue;
                }
            };
            let result = created.response;
            let stored_content = created.content;
            let id = result
                .get("id")
                .and_then(|v| v.as_str())
                .and_then(|s| Uuid::parse_str(s).ok());
            if let Some(id) = id {
                new_records.push(NewRecordForRef {
                    id,
                    text: stored_content.clone(),
                });
            }

            report.issues_ingested += 1;
            if !cursor_stalled {
                match id {
                    Some(id)
                        if checkpoint.acknowledge(masked.number, updated_at.as_deref(), id) => {}
                    _ => {
                        report.warnings.push(
                            "issues: record could not be acknowledged in the bounded checkpoint"
                                .into(),
                        );
                        stall_cursor(&mut cursor_stalled, report);
                    }
                }
            }
        }

        // Save completed and budget-truncated pages before another fetch can
        // fail. A stalled pass persists only its contiguous-success prefix.
        write_page_checkpoint(runtime, project_id, "issues", &checkpoint).await?;
        match decide_page_outcome(
            page_len,
            requested_limit,
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

    Ok(())
}

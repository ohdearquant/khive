use super::*;

/// The metadata and page-path `git log` passes a commit-ingest phase needs, loaded together so
/// a classified failure in either one can be retried as a single unit.
pub(super) struct CommitSnapshot {
    pub(super) commits: Vec<RawCommit>,
    pub(super) files_by_sha: HashMap<String, Vec<String>>,
    pub(super) head: String,
    pub(super) repo: PathBuf,
}

/// Resolve the ref without peeling objects so missing-promisor recovery still
/// runs through the classified git-log boundary. Never retarget a retry to HEAD.
pub(super) fn resolve_commit_head(repo: &Path) -> Result<String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "HEAD"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = spawn_retrying_executable_busy(&EXECUTABLE_BUSY_BACKOFF_MS, || command.spawn())
        .and_then(|child| child.wait_with_output())
        .context("resolving commit snapshot HEAD")?;
    let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || !is_commit_oid(&head) {
        return Err(anyhow!("could not resolve commit snapshot HEAD"));
    }
    Ok(head)
}

/// Load one commit-history snapshot. Validate and remove the acknowledged
/// prefix before selecting paths, but retain the entire remaining walk for
/// the existing budget and completion decisions.
pub(super) fn load_commit_snapshot(
    repo: &Path,
    since_sha: Option<&str>,
    snapshot_head: &str,
    last_completed_sha: Option<&str>,
    page_limit: Option<usize>,
) -> Result<CommitSnapshot> {
    let mut commits = walk_commits(repo, since_sha, snapshot_head)?;
    if let Some(last_completed_sha) = last_completed_sha {
        let position = commits
            .iter()
            .position(|record| record.sha == last_completed_sha)
            .ok_or_else(|| anyhow!("commits checkpoint position is absent from its frozen snapshot; reset both commit cursor rows to replay history"))?;
        commits.drain(..=position);
    }
    let page_shas: Vec<String> = commits
        .iter()
        .take(page_limit.unwrap_or(usize::MAX))
        .map(|commit| commit.sha.clone())
        .rev()
        .collect();
    let files_by_sha = touched_files(repo, &page_shas)?;
    Ok(CommitSnapshot {
        commits,
        files_by_sha,
        head: snapshot_head.to_string(),
        repo: repo.to_path_buf(),
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

pub(super) fn cache_repair_warning(strategy: CacheRepairStrategy) -> String {
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
pub(super) fn recover_commit_snapshot(
    repo: &Path,
    since_sha: Option<&str>,
    snapshot_head: Option<&str>,
    last_completed_sha: Option<&str>,
    page_limit: Option<usize>,
    mut recover: impl FnMut(&Path, &GitLogError) -> Result<Option<RecoveredRepo>>,
) -> Result<(CommitSnapshot, Option<String>)> {
    let snapshot_head = match snapshot_head {
        Some(head) => head.to_string(),
        None => resolve_commit_head(repo)?,
    };
    let mut repo_path = repo.to_path_buf();
    let mut recovery_warning: Option<String> = None;
    loop {
        match load_commit_snapshot(
            &repo_path,
            since_sha,
            &snapshot_head,
            last_completed_sha,
            page_limit,
        ) {
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
pub(super) fn squash_merge_pr_number(subject: &str) -> Option<u64> {
    let trimmed = subject.trim_end();
    let close = trimmed.strip_suffix(')')?;
    let open = close.rfind("(#")?;
    close[open + 2..].parse::<u64>().ok()
}

/// Max characters for the `name` field the amendment's readable-names rider
/// sets on newly ingested notes (issues/PRs: `"#<number> <title>"`; commits:
/// `"<short_sha> <subject>"`).
pub(super) const NAME_MAX_CHARS: usize = 120;

/// Byte cap for a commit note's candidate `embedding_content` (issue #764).
/// This UTF-8-safe pre-cut drives the truncation summary; the runtime bounds
/// each model's provider input to its own budget. Full commit content stays
/// untruncated in storage and FTS.
pub(super) const MAX_COMMIT_EMBED_BYTES: usize = 32_768;

/// Sentinel reason seeded into the commit source slot when a walk begins.
/// A slot still holding this exact reason at the end of the pass means the
/// walk ran to the end of its snapshot without a budget break or a stall,
/// and is rewritten to `Completed`; any other reason was written by a real
/// early-stop arm and is preserved.
pub(super) const COMMIT_WALK_SEED_REASON: &str = "walk began but did not report completion";

/// Returns a UTF-8-valid, proper head prefix of `content` when it exceeds
/// `MAX_COMMIT_EMBED_BYTES`, or `None` when `content` is at or under the cap
/// (nothing to truncate — the full text is a valid embedding input as-is).
pub(super) fn truncated_embedding_head(content: &str) -> Option<&str> {
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
pub(super) struct MaskedCommitFields {
    pub(super) sha: String,
    pub(super) short_sha: String,
    pub(super) author: String,
    pub(super) author_email: String,
    pub(super) committed_at: String,
    pub(super) parents: Vec<String>,
    pub(super) subject: String,
    pub(super) body: String,
}

impl MaskedCommitFields {
    pub(super) fn new(commit: &RawCommit) -> Self {
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
            author: mask_git_ingest(author).into_owned(),
            author_email: mask_git_ingest(author_email).into_owned(),
            committed_at: committed_at.clone(),
            parents: parents.clone(),
            subject: mask_git_ingest(subject).into_owned(),
            body: mask_git_ingest(body).into_owned(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn ingest_commits(
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
    recover: impl FnMut(&Path, &GitLogError) -> Result<Option<RecoveredRepo>> + Send + 'static,
    walk_complete: &mut bool,
) -> Result<()> {
    let (since, checkpoint) = read_commit_checkpoint(runtime, token, project_id).await?;
    let pending = checkpoint
        .as_ref()
        .filter(|c| c.last_completed_sha != c.snapshot_head);
    let base_cursor = match pending {
        Some(c) => c.base_cursor.clone(),
        None => since.clone(),
    };
    if base_cursor
        .as_deref()
        .is_some_and(|sha| !is_commit_oid(sha))
    {
        return Err(anyhow!(
            "invalid commits cursor; reset both commit cursor rows to replay history"
        ));
    }
    let repo_path = repo.to_path_buf();
    let since_sha = base_cursor.clone();
    let frozen_head = pending.map(|c| c.snapshot_head.clone());
    let last_completed_sha = pending.map(|c| c.last_completed_sha.clone());
    let page_limit = budget
        .remaining
        .map(|remaining| usize::try_from(remaining).unwrap_or(usize::MAX));
    // The metadata walk, page-path pass, and any classified cache repair all
    // use blocking git/cache operations. Keep them in one worker so retries
    // still bind to the original frozen tip without blocking a Tokio worker.
    let (snapshot, recovery_warning) = tokio::task::spawn_blocking(move || {
        recover_commit_snapshot(
            &repo_path,
            since_sha.as_deref(),
            frozen_head.as_deref(),
            last_completed_sha.as_deref(),
            page_limit,
            recover,
        )
    })
    .await
    .context("commit snapshot task failed")??;
    let CommitSnapshot {
        commits,
        files_by_sha,
        head: snapshot_head,
        repo: snapshot_repo,
    } = snapshot;
    if commits.is_empty() {
        // An empty range is a genuine completion only when the cursor is an
        // ancestor of HEAD. See crates/khive-pack-git/docs/ingest.md
        // #commit-walk-ancestor-divergence-and-cursor-stall.
        let cursor_not_ancestor = last_sha_of(&since).is_some_and(|since_sha| {
            let not_ancestor = !is_ancestor_of_head(&snapshot_repo, since_sha);
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
        } else if resolve_commit_head(&snapshot_repo)? != snapshot_head {
            report.done = false;
            report.sources.commits = Some(IngestSourceState::StoppedEarly(
                "source HEAD changed after the frozen commit snapshot; call again for the new history".into(),
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
    let mut checkpoint = CommitCheckpoint {
        version: 1,
        namespace: token.namespace().as_str().to_string(),
        base_cursor,
        snapshot_head: snapshot_head.clone(),
        last_completed_sha: String::new(),
    };
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

    // `cursor_stalled` freezes the persisted SHA at the last contiguous success so a
    // failed record is retried next pass instead of skipped forever; later
    // records this pass are still attempted. See crates/khive-pack-git/docs/
    // ingest.md#commit-walk-ancestor-divergence-and-cursor-stall.
    let mut cursor_stalled = false;
    // Bounded detail for the per-run ambiguous-module-skip warning: the
    // masked skipped paths in encounter order, capped so one pathological
    // run cannot bloat the report (the full count is always exact).
    const AMBIGUOUS_SKIP_DETAIL_CAP: usize = 5;
    const AMBIGUOUS_SKIP_PATH_DISPLAY_CHARS: usize = 80;
    let mut ambiguous_module_skip_paths: Vec<String> = Vec::new();
    // Parent SHA -> note id for commits created earlier this pass; combined
    // with `find_commit_by_sha`'s DB lookup, resolves parent edges regardless
    // of which pass the parent landed in. The stall guard on the cursor
    // writes below prevents stranding a failed commit behind an advanced
    // floor. See crates/khive-pack-git/docs/ingest.md
    // #commit-walk-ancestor-divergence-and-cursor-stall.
    let mut local_sha_to_id: HashMap<String, Uuid> = HashMap::new();
    for c in &commits {
        if !budget.try_consume() {
            report.sources.commits = Some(IngestSourceState::StoppedEarly(
                "budget exhausted before the commit history was exhausted".into(),
            ));
            break;
        }
        let existing = find_commit_by_sha(runtime, token, &c.sha).await?;

        let masked = MaskedCommitFields::new(c);
        let content = if masked.body.trim().is_empty() {
            masked.subject.clone()
        } else {
            format!("{}\n\n{}", masked.subject, masked.body)
        };

        // The path pass requests this page's commits from the frozen metadata
        // walk, so every visited commit should have a path-set entry. A
        // missing entry means the passes disagree; surface it instead of
        // silently storing the `[]`
        // the contract reserves for a genuinely empty commit.
        let Some(touched_paths) = files_by_sha.get(&c.sha) else {
            stall_cursor(&mut cursor_stalled, report);
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
            .map(|path| mask_git_ingest(path).into_owned())
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

        if let Some(existing) = existing {
            // A SHA is shared across project anchors. The natural-key hit
            // skips note creation, but still adds this project's missing
            // annotations before advancing its checkpoint. Existing live
            // edges may have curated fields; tombstones stay deleted.
            let links =
                missing_commit_annotation_specs(runtime, token, existing, &annotates).await?;
            if let Err(error) = runtime.link_many(token, links).await {
                record_write_failure(report, "link", "commit", c.sha.clone(), error);
                stall_cursor(&mut cursor_stalled, report);
                continue;
            }
            local_sha_to_id.insert(c.sha.clone(), existing);
            report.commits_skipped_existing += 1;
            if !cursor_stalled {
                checkpoint.last_completed_sha.clone_from(&c.sha);
                write_commit_checkpoint(runtime, project_id, &checkpoint).await?;
            }
            continue;
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
            "key": format!("git.commit:{}", c.sha),
            "name": name,
            "content": content,
            "properties": properties,
            "annotates": annotates.iter().cloned().collect::<Vec<_>>(),
        });
        if let Some(head) = embedding_head {
            create_request["embedding_content"] = json!(head);
        }

        // The keyed note path finishes embedding and annotations before its
        // single publication transaction. A competing pass may win the key
        // after our initial SHA lookup; both exact replay and key conflict
        // then reuse only the committed winner, including this project's
        // missing annotations, before acknowledging the checkpoint.
        let (id, created) = match crate::dispatch_from_token(
            registry,
            token,
            "create",
            create_request,
        )
        .await
        {
            Ok(v) => {
                let Some(id) = v
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(|s| Uuid::parse_str(s).ok())
                else {
                    record_write_failure(
                        report,
                        "create",
                        "commit",
                        c.sha.clone(),
                        RuntimeError::Internal("create commit returned no valid id".into()),
                    );
                    stall_cursor(&mut cursor_stalled, report);
                    continue;
                };
                let Some(created) = v.get("created").and_then(Value::as_bool) else {
                    record_write_failure(
                        report,
                        "create",
                        "commit",
                        c.sha.clone(),
                        RuntimeError::Internal(
                            "keyed create commit returned no created flag".into(),
                        ),
                    );
                    stall_cursor(&mut cursor_stalled, report);
                    continue;
                };
                (id, created)
            }
            Err(error) if is_note_key_conflict(&error) => {
                let Some(id) = find_commit_by_sha(runtime, token, &c.sha).await? else {
                    let key = format!("git.commit:{}", c.sha);
                    let holder = runtime
                        .get_note_by_key(token, &key, Some("commit"), false)
                        .await;
                    let failure = match holder {
                            Ok(note) => {
                                let holder_sha = note
                                    .properties
                                    .as_ref()
                                    .and_then(|properties| properties.get("sha"))
                                    .and_then(Value::as_str)
                                    .filter(|sha| crate::object_id::is_40_hex(sha.as_bytes()))
                                    .unwrap_or("<missing or malformed>");
                                RuntimeError::Internal(format!(
                                    "git commit key {key:?} is held by note {} kind {} properties.sha {holder_sha:?}; expected SHA {}",
                                    note.id, note.kind, c.sha
                                ))
                            }
                            Err(lookup_error) => RuntimeError::Internal(format!(
                                "git commit key {key:?} conflicted but holder lookup failed: {lookup_error}; create error: {error}"
                            )),
                        };
                    record_write_failure(report, "create", "commit", c.sha.clone(), failure);
                    stall_cursor(&mut cursor_stalled, report);
                    continue;
                };
                (id, false)
            }
            Err(error) => {
                record_write_failure(report, "create", "commit", c.sha.clone(), error);
                stall_cursor(&mut cursor_stalled, report);
                continue;
            }
        };
        if !created {
            let links = missing_commit_annotation_specs(runtime, token, id, &annotates).await?;
            if let Err(error) = runtime.link_many(token, links).await {
                record_write_failure(report, "link", "commit", c.sha.clone(), error);
                stall_cursor(&mut cursor_stalled, report);
                continue;
            }
            local_sha_to_id.insert(c.sha.clone(), id);
            report.commits_skipped_existing += 1;
        } else {
            report.commits_ingested += 1;
            if embedding_head.is_some() {
                report.commit_embeddings_truncated += 1;
            }
            local_sha_to_id.insert(c.sha.clone(), id);
            new_records.push(NewRecordForRef {
                id,
                text: content.clone(),
            });
            // Parent -> child `precedes` edges are best-effort enrichment.
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
        if !cursor_stalled {
            // Each contiguous success survives later failures without treating
            // the ancestor closure of one SHA as the visited DAG prefix.
            checkpoint.last_completed_sha.clone_from(&c.sha);
            write_commit_checkpoint(runtime, project_id, &checkpoint).await?;
        }
    }

    if cursor_stalled {
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
            if resolve_commit_head(&snapshot_repo)? != snapshot_head {
                report.done = false;
                report.sources.commits = Some(IngestSourceState::StoppedEarly(
                    "source HEAD changed after the frozen commit snapshot; call again for the new history".into(),
                ));
            } else {
                report.sources.commits = Some(IngestSourceState::Completed);
                *walk_complete = true;
            }
        }
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
pub(super) fn last_sha_of(since: &Option<String>) -> Option<&str> {
    since.as_deref().filter(|s| !s.is_empty())
}

/// `true` when `sha` is an ancestor of (or equal to) `repo`'s HEAD. A SHA
/// that is unknown to this repo returns `false` — for the empty-walk guard
/// that is the correct reading: the walked source does not contain the
/// history that advanced the cursor.
pub(super) fn is_ancestor_of_head(repo: &Path, sha: &str) -> bool {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(["merge-base", "--is-ancestor", sha, "HEAD"])
        // This predicate consumes only status; diagnostics have no reader.
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    spawn_retrying_executable_busy(&EXECUTABLE_BUSY_BACKOFF_MS, || command.spawn())
        .and_then(|mut child| child.wait())
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Count `commit` notes annotating `project_id`, derived fresh from the
/// database rather than tracked in-memory across process invocations — see
/// `IngestReport::commits_total_in_db` (issue #1045).
pub(super) async fn count_commit_notes_for_project(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    project_id: Uuid,
) -> Result<u64> {
    let sql = runtime.sql();
    let mut r = sql.reader().await.map_err(anyhow::Error::new)?;
    let row = r
        .query_scalar(SqlStatement {
            sql: sql!("commit_notes_for_project_count").into(),
            params: vec![
                SqlValue::Text(token.namespace().as_str().to_string()),
                SqlValue::Text(project_id.to_string()),
            ],
            label: Some("git_ingest_count_commit_notes".into()),
        })
        .await
        .map_err(anyhow::Error::new)?;
    match row {
        Some(SqlValue::Integer(n)) => Ok(n as u64),
        _ => Ok(0),
    }
}

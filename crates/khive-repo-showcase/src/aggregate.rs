use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};

use crate::export::{bounded_page, CommitWork, ExportError};
use crate::join::natural_id;
use crate::*;

pub(crate) struct AggregateInput<'a> {
    pub(crate) generated_at: &'a str,
    pub(crate) graph: &'a RepoGraph,
    pub(crate) commits: &'a [CommitWork],
    pub(crate) modules: &'a [ModuleNode],
    pub(crate) structure_edges: &'a [GraphEdge],
    pub(crate) commit_module_edges: &'a [GraphEdge],
    pub(crate) issues: &'a [IssueNode],
    pub(crate) pull_requests: &'a [PullRequestNode],
    pub(crate) release_tags: Page<ReleaseTag>,
    pub(crate) bounds: &'a ExportBounds,
    pub(crate) provenance: &'a PipelineProvenance,
}

pub(crate) fn build_aggregates(input: AggregateInput<'_>) -> Result<RepoAggregates, ExportError> {
    let generated_at = DateTime::parse_from_rfc3339(input.generated_at)
        .map_err(|error| {
            ExportError::InvalidData(format!("invalid normalized generated_at: {error}"))
        })?
        .with_timezone(&Utc);
    let commit_coverage = history_source(input.provenance, |sources| &sources.commits);
    let structure_unavailable = [
        &input.graph.modules.disclosure,
        &input.graph.structure_edges.disclosure,
    ]
    .into_iter()
    .find(|disclosure| disclosure.status == DisclosureStatus::Unavailable)
    .and_then(|disclosure| disclosure.reason.clone());
    let structure_status = if structure_unavailable.is_some() {
        ViewStatus::Unavailable
    } else {
        ViewStatus::Available
    };
    let join_unavailable =
        if input.graph.commit_module_edges.disclosure.status == DisclosureStatus::Unavailable {
            input
                .graph
                .commit_module_edges
                .disclosure
                .reason
                .clone()
                .or_else(|| Some("commit-to-module join is unavailable".into()))
        } else {
            commit_coverage.unavailable_reason("commit-to-module join")
        };
    let join_status = if join_unavailable.is_some() {
        ViewStatus::Unavailable
    } else {
        ViewStatus::Available
    };
    let module_ids = input
        .modules
        .iter()
        .map(|module| module.id.clone())
        .collect::<BTreeSet<_>>();
    let dependency_edges = input
        .structure_edges
        .iter()
        .filter(|edge| {
            edge.relation == "depends_on"
                && module_ids.contains(&edge.source)
                && module_ids.contains(&edge.target)
        })
        .collect::<Vec<_>>();
    let (fan_in, fan_out) = degrees(&dependency_edges);
    let cycles = dependency_cycles(&module_ids, &dependency_edges);
    let cycle_membership = cycles
        .iter()
        .flat_map(|cycle| {
            cycle
                .module_ids
                .iter()
                .map(move |module| (module.clone(), cycle.id.clone()))
        })
        .fold(
            BTreeMap::<String, Vec<String>>::new(),
            |mut map, (module, cycle)| {
                map.entry(module).or_default().push(cycle);
                map
            },
        );
    let dependency_rows = input
        .modules
        .iter()
        .map(|module| DependencyModuleRow {
            module_id: module.id.clone(),
            fan_in: *fan_in.get(&module.id).unwrap_or(&0),
            fan_out: *fan_out.get(&module.id).unwrap_or(&0),
            cycle_ids: cycle_membership
                .get(&module.id)
                .cloned()
                .unwrap_or_default(),
        })
        .collect::<Vec<_>>();
    let dependency_topology = DependencyTopologyAnalysis {
        meta: meta(
            "views.dependency_topology",
            Granularity::Module,
            JoinTag::StructureOnly,
            structure_status,
            structure_unavailable.clone(),
            &["graph.modules", "graph.structure_edges.depends_on"],
            all_history(),
            input.bounds.aggregate_rows,
            "module_id",
        ),
        modules: covered_analysis_page(
            dependency_rows,
            input.bounds.aggregate_rows,
            "module_id",
            structure_unavailable.as_deref(),
        ),
        cycles: covered_analysis_page(
            cycles.clone(),
            input.bounds.aggregate_rows,
            "cycle_id",
            structure_unavailable.as_deref(),
        ),
    };

    let commit_by_id = input
        .commits
        .iter()
        .map(|commit| (commit.node.id.as_str(), &commit.node))
        .collect::<BTreeMap<_, _>>();
    let activity_start = generated_at - Duration::days(365);
    let mut commits_by_module = BTreeMap::<String, BTreeSet<String>>::new();
    let mut modules_by_commit = BTreeMap::<String, BTreeSet<String>>::new();
    for edge in input.commit_module_edges {
        commits_by_module
            .entry(edge.target.clone())
            .or_default()
            .insert(edge.source.clone());
        modules_by_commit
            .entry(edge.source.clone())
            .or_default()
            .insert(edge.target.clone());
    }
    let recent_counts = input
        .modules
        .iter()
        .map(|module| {
            let count = commits_by_module
                .get(&module.id)
                .into_iter()
                .flatten()
                .filter(|commit_id| {
                    commit_by_id
                        .get(commit_id.as_str())
                        .and_then(|commit| parse_time(&commit.committed_at))
                        .is_some_and(|timestamp| timestamp >= activity_start)
                })
                .count() as u64;
            (module.id.clone(), count)
        })
        .collect::<BTreeMap<_, _>>();
    let churn_threshold = median_nonzero(recent_counts.values().copied());
    let fan_in_threshold = median_nonzero(fan_in.values().copied());
    let mut hotspots = input
        .modules
        .iter()
        .map(|module| {
            let commit_count = *recent_counts.get(&module.id).unwrap_or(&0);
            let incoming = *fan_in.get(&module.id).unwrap_or(&0);
            let high_churn = commit_count >= churn_threshold && commit_count > 0;
            let high_fan_in = incoming >= fan_in_threshold && incoming > 0;
            HotspotRow {
                module_id: module.id.clone(),
                commit_count,
                fan_in: incoming,
                quadrant: match (high_churn, high_fan_in) {
                    (true, true) => HotspotQuadrant::HighChurnHighFanIn,
                    (true, false) => HotspotQuadrant::HighChurnLowFanIn,
                    (false, true) => HotspotQuadrant::LowChurnHighFanIn,
                    (false, false) => HotspotQuadrant::LowChurnLowFanIn,
                },
            }
        })
        .collect::<Vec<_>>();
    hotspots.sort_by(|left, right| {
        right
            .commit_count
            .cmp(&left.commit_count)
            .then_with(|| right.fan_in.cmp(&left.fan_in))
            .then_with(|| left.module_id.cmp(&right.module_id))
    });
    let hotspot_quadrant = Analysis {
        meta: meta(
            "views.hotspot_quadrant",
            Granularity::Module,
            JoinTag::Join,
            join_status,
            join_unavailable.clone(),
            &[
                "graph.commit_module_edges",
                "graph.structure_edges.depends_on",
            ],
            rolling_days(365, generated_at),
            input.bounds.aggregate_rows,
            "commit_count_desc,fan_in_desc,module_id",
        ),
        data: covered_analysis_page(
            hotspots.clone(),
            input.bounds.aggregate_rows,
            "commit_count_desc,fan_in_desc,module_id",
            join_unavailable.as_deref(),
        ),
    };

    let structural_pairs = dependency_edges
        .iter()
        .map(|edge| ordered_pair(&edge.source, &edge.target))
        .collect::<BTreeSet<_>>();
    let mut pair_counts = BTreeMap::<(String, String), u64>::new();
    for (commit_id, modules) in &modules_by_commit {
        let recent = commit_by_id
            .get(commit_id.as_str())
            .and_then(|commit| parse_time(&commit.committed_at))
            .is_some_and(|timestamp| timestamp >= activity_start);
        if !recent {
            continue;
        }
        let modules = modules.iter().collect::<Vec<_>>();
        for left in 0..modules.len() {
            for right in left + 1..modules.len() {
                let pair = ordered_pair(modules[left], modules[right]);
                if !structural_pairs.contains(&pair) {
                    *pair_counts.entry(pair).or_default() += 1;
                }
            }
        }
    }
    let recent_commit_count = input
        .commits
        .iter()
        .filter(|commit| {
            parse_time(&commit.node.committed_at)
                .is_some_and(|timestamp| timestamp >= activity_start)
        })
        .count()
        .max(1) as f64;
    let mut hidden = pair_counts
        .into_iter()
        .map(
            |((left_module_id, right_module_id), cochange_count)| HiddenCouplingRow {
                left_module_id,
                right_module_id,
                cochange_count,
                support: Ratio::new(cochange_count as f64 / recent_commit_count)
                    .expect("co-change support is bounded by the commit count"),
            },
        )
        .collect::<Vec<_>>();
    hidden.sort_by(|left, right| {
        right
            .cochange_count
            .cmp(&left.cochange_count)
            .then_with(|| left.left_module_id.cmp(&right.left_module_id))
            .then_with(|| left.right_module_id.cmp(&right.right_module_id))
    });
    let hidden_coupling = Analysis {
        meta: meta(
            "views.hidden_coupling",
            Granularity::Module,
            JoinTag::Join,
            join_status,
            join_unavailable.clone(),
            &[
                "graph.commit_module_edges",
                "graph.structure_edges.depends_on",
            ],
            rolling_days(365, generated_at),
            input.bounds.aggregate_rows,
            "cochange_count_desc,module_pair",
        ),
        data: covered_analysis_page(
            hidden,
            input.bounds.aggregate_rows,
            "cochange_count_desc,module_pair",
            join_unavailable.as_deref(),
        ),
    };

    let recent_start = generated_at - Duration::days(90);
    let treemap = input
        .modules
        .iter()
        .map(|module| {
            let recent = commits_by_module
                .get(&module.id)
                .into_iter()
                .flatten()
                .filter(|commit_id| {
                    commit_by_id
                        .get(commit_id.as_str())
                        .and_then(|commit| parse_time(&commit.committed_at))
                        .is_some_and(|timestamp| timestamp >= recent_start)
                })
                .count() as u64;
            TreemapRow {
                package_id: module.package_id.clone(),
                module_id: module.id.clone(),
                source_file_count: 1,
                recent_commit_count: match &join_unavailable {
                    Some(reason) => Availability::unavailable(reason.clone()),
                    None => Availability::available(recent),
                },
            }
        })
        .collect();
    let structure_treemap = Analysis {
        meta: meta(
            "views.structure_treemap",
            Granularity::ModuleSymbolDeferred,
            JoinTag::StructureOnly,
            structure_status,
            structure_unavailable.clone(),
            &[
                "graph.packages",
                "graph.modules",
                "graph.commit_module_edges",
            ],
            rolling_days(90, generated_at),
            input.bounds.aggregate_rows,
            "package_id,module_id",
        ),
        data: covered_analysis_page(
            treemap,
            input.bounds.aggregate_rows,
            "package_id,module_id",
            structure_unavailable.as_deref(),
        ),
    };

    let cadence_timeline = cadence(&input, generated_at)?;
    let (ownership, ownership_rows) = ownership(&input, &commits_by_module)?;

    let mut api_rows = input
        .modules
        .iter()
        .map(|module| ApiSurfaceRow {
            module_id: module.id.clone(),
            dependent_count: *fan_in.get(&module.id).unwrap_or(&0),
        })
        .collect::<Vec<_>>();
    api_rows.sort_by(|left, right| {
        right
            .dependent_count
            .cmp(&left.dependent_count)
            .then_with(|| left.module_id.cmp(&right.module_id))
    });
    let api_surface = Analysis {
        meta: meta(
            "views.api_surface",
            Granularity::ModuleSymbolDeferred,
            JoinTag::StructureOnly,
            structure_status,
            structure_unavailable.clone(),
            &["graph.structure_edges.depends_on"],
            all_history(),
            input.bounds.aggregate_rows,
            "dependent_count_desc,module_id",
        ),
        data: covered_analysis_page(
            api_rows,
            input.bounds.aggregate_rows,
            "dependent_count_desc,module_id",
            structure_unavailable.as_deref(),
        ),
    };

    let scorecard = scorecard(&input, generated_at, &hotspots, &cycles, &ownership_rows);

    Ok(RepoAggregates {
        dependency_topology,
        hotspot_quadrant,
        hidden_coupling,
        structure_treemap,
        cadence_timeline,
        ownership,
        api_surface,
        scorecard,
    })
}

fn cadence(
    input: &AggregateInput<'_>,
    generated_at: DateTime<Utc>,
) -> Result<CadenceAnalysis, ExportError> {
    let commit_coverage = history_source(input.provenance, |sources| &sources.commits);
    let issue_coverage = history_source(input.provenance, |sources| &sources.issues);
    let pr_coverage = history_source(input.provenance, |sources| &sources.pull_requests);
    let mut commit_weeks = BTreeMap::<NaiveDate, u64>::new();
    let mut issue_open_weeks = BTreeMap::<NaiveDate, u64>::new();
    let mut issue_close_weeks = BTreeMap::<NaiveDate, u64>::new();
    let mut pull_request_open_weeks = BTreeMap::<NaiveDate, u64>::new();
    let mut pull_request_merge_weeks = BTreeMap::<NaiveDate, u64>::new();
    let mut missing_issue_created_at = 0_u64;
    let mut missing_pull_request_created_at = 0_u64;
    let mut earliest = None::<DateTime<Utc>>;
    for commit in input.commits {
        if let Some(timestamp) = parse_time(&commit.node.committed_at) {
            increment_week(&mut commit_weeks, timestamp);
            update_earliest(&mut earliest, timestamp);
        }
    }
    if issue_coverage.completed() {
        for issue in input.issues {
            match &issue.created_at {
                Availability::Available { value } => {
                    let timestamp = parse_time(value)
                        .expect("Timestamp guarantees a valid RFC3339 issue creation time");
                    increment_week(&mut issue_open_weeks, timestamp);
                    update_earliest(&mut earliest, timestamp);
                }
                Availability::Unavailable { .. } => missing_issue_created_at += 1,
            }
            if let Availability::Available { value } = &issue.closed_at {
                let timestamp = parse_time(value)
                    .expect("Timestamp guarantees a valid RFC3339 issue closure time");
                increment_week(&mut issue_close_weeks, timestamp);
                update_earliest(&mut earliest, timestamp);
            }
        }
    }
    if pr_coverage.completed() {
        for pull_request in input.pull_requests {
            match &pull_request.created_at {
                Availability::Available { value } => {
                    let timestamp = parse_time(value)
                        .expect("Timestamp guarantees a valid RFC3339 pull-request creation time");
                    increment_week(&mut pull_request_open_weeks, timestamp);
                    update_earliest(&mut earliest, timestamp);
                }
                Availability::Unavailable { .. } => missing_pull_request_created_at += 1,
            }
            if let Availability::Available { value } = &pull_request.merged_at {
                let timestamp = parse_time(value)
                    .expect("Timestamp guarantees a valid RFC3339 pull-request merge time");
                increment_week(&mut pull_request_merge_weeks, timestamp);
                update_earliest(&mut earliest, timestamp);
            }
        }
    }
    for tag in &input.release_tags.items {
        if let Availability::Available { value } = &tag.committed_at {
            if let Some(timestamp) = parse_time(value) {
                update_earliest(&mut earliest, timestamp);
            }
        }
    }
    let lead_time = if pr_coverage.completed() {
        let mut hours = Vec::new();
        let mut merged_missing_created_at = 0_u64;
        for pull_request in input.pull_requests {
            let Availability::Available { value: merged } = &pull_request.merged_at else {
                continue;
            };
            let Availability::Available { value: created } = &pull_request.created_at else {
                merged_missing_created_at += 1;
                continue;
            };
            let created = parse_time(created)
                .expect("Timestamp guarantees a valid RFC3339 pull-request creation time");
            let merged = parse_time(merged)
                .expect("Timestamp guarantees a valid RFC3339 pull-request merge time");
            let duration = merged - created;
            if duration.num_seconds() < 0 {
                return Err(ExportError::InvalidData(format!(
                    "pull request #{} merge time precedes creation time",
                    pull_request.number
                )));
            }
            hours.push(duration.num_seconds() as f64 / 3600.0);
        }
        if merged_missing_created_at > 0 {
            Availability::unavailable(format!(
                "{merged_missing_created_at} merged pull request(s) lack creation timestamps"
            ))
        } else if hours.is_empty() {
            Availability::unavailable("no merged pull requests have complete lead-time timestamps")
        } else {
            hours.sort_by(f64::total_cmp);
            Availability::available(Percentiles {
                p50: percentile(&hours, 0.50),
                p90: percentile(&hours, 0.90),
                p95: percentile(&hours, 0.95),
            })
        }
    } else {
        Availability::unavailable(
            pr_coverage
                .unavailable_reason("pull-request lead time")
                .expect("non-completed coverage has reason"),
        )
    };
    let cadence_available = commit_coverage.completed()
        || issue_coverage.completed()
        || pr_coverage.completed()
        || input.release_tags.disclosure.status != DisclosureStatus::Unavailable;
    let cadence_reason = (!cadence_available).then(|| {
        "all commit, issue, pull-request, and release-tag cadence sources are unavailable"
            .to_string()
    });
    Ok(CadenceAnalysis {
        meta: meta(
            "views.cadence_timeline",
            Granularity::Repository,
            JoinTag::HistoryOnly,
            if cadence_available {
                ViewStatus::Available
            } else {
                ViewStatus::Unavailable
            },
            cadence_reason,
            &[
                "graph.commits",
                "graph.issues",
                "graph.pull_requests",
                "clone.tags",
            ],
            AnalysisWindow {
                kind: WindowKind::Range,
                start: earliest.map(|value| {
                    Timestamp::parse(value.to_rfc3339()).expect("chrono produced RFC3339")
                }),
                end: Some(
                    Timestamp::parse(generated_at.to_rfc3339()).expect("chrono produced RFC3339"),
                ),
                days: None,
            },
            input.bounds.aggregate_rows,
            "week_start",
        ),
        commits: cadence_page(
            commit_weeks,
            input.bounds.aggregate_rows,
            &commit_coverage,
            "commit cadence",
            None,
        ),
        issues_opened: cadence_page(
            issue_open_weeks,
            input.bounds.aggregate_rows,
            &issue_coverage,
            "issue-open cadence",
            (missing_issue_created_at > 0).then(|| {
                format!(
                    "{missing_issue_created_at} issue record(s) lack required creation timestamps"
                )
            }),
        ),
        issues_closed: cadence_page(
            issue_close_weeks,
            input.bounds.aggregate_rows,
            &issue_coverage,
            "issue-close cadence",
            None,
        ),
        pull_requests_opened: cadence_page(
            pull_request_open_weeks,
            input.bounds.aggregate_rows,
            &pr_coverage,
            "pull-request-open cadence",
            (missing_pull_request_created_at > 0).then(|| {
                format!(
                    "{missing_pull_request_created_at} pull-request record(s) lack required creation timestamps"
                )
            }),
        ),
        pull_requests_merged: cadence_page(
            pull_request_merge_weeks,
            input.bounds.aggregate_rows,
            &pr_coverage,
            "pull-request-merge cadence",
            None,
        ),
        release_tags: input.release_tags.clone(),
        pull_request_lead_time_hours: lead_time,
    })
}

fn ownership(
    input: &AggregateInput<'_>,
    commits_by_module: &BTreeMap<String, BTreeSet<String>>,
) -> Result<(OwnershipAnalysis, Vec<OwnershipRow>), ExportError> {
    let commit_coverage = history_source(input.provenance, |sources| &sources.commits);
    let commit_reason = commit_coverage.unavailable_reason("repository ownership");
    let join_reason =
        if input.graph.commit_module_edges.disclosure.status == DisclosureStatus::Unavailable {
            input
                .graph
                .commit_module_edges
                .disclosure
                .reason
                .clone()
                .or_else(|| Some("commit-to-module join is unavailable".into()))
        } else {
            None
        };
    let authors_by_commit = input
        .commits
        .iter()
        .map(|commit| (commit.node.id.as_str(), commit.node.author.as_str()))
        .collect::<BTreeMap<_, _>>();
    let mut modules = Vec::new();
    for module in input.modules {
        let mut counts = BTreeMap::<String, u64>::new();
        for commit_id in commits_by_module.get(&module.id).into_iter().flatten() {
            if let Some(author) = authors_by_commit.get(commit_id.as_str()) {
                *counts.entry((*author).to_string()).or_default() += 1;
            }
        }
        modules.push(ownership_row(
            module.id.clone(),
            counts,
            input.bounds.authors_per_scope,
        ));
    }
    modules.sort_by(|left, right| left.module_id.cmp(&right.module_id));
    let mut repository_counts = BTreeMap::<String, u64>::new();
    for commit in input.commits {
        *repository_counts
            .entry(commit.node.author.clone())
            .or_default() += 1;
    }
    let repository = ownership_row(
        input.graph.repository.id.clone(),
        repository_counts,
        input.bounds.authors_per_scope,
    );
    let (repository_author_concentration, repository_bus_factor, repository_authors) =
        match commit_reason {
            Some(reason) => (
                Availability::unavailable(reason.clone()),
                Availability::unavailable(reason.clone()),
                Page::unavailable(
                    input.bounds.authors_per_scope,
                    "commits_desc,author",
                    reason,
                ),
            ),
            None => (
                repository.author_concentration,
                repository.bus_factor,
                repository.authors,
            ),
        };
    let full_modules = modules.clone();
    Ok((
        OwnershipAnalysis {
            meta: meta(
                "views.ownership",
                Granularity::Module,
                JoinTag::Join,
                if join_reason.is_some() {
                    ViewStatus::Unavailable
                } else {
                    ViewStatus::Available
                },
                join_reason.clone(),
                &["graph.commits.author", "graph.commit_module_edges"],
                all_history(),
                input.bounds.aggregate_rows,
                "module_id",
            ),
            modules: covered_analysis_page(
                modules,
                input.bounds.aggregate_rows,
                "module_id",
                join_reason.as_deref(),
            ),
            repository_author_concentration,
            repository_bus_factor,
            repository_authors,
        },
        full_modules,
    ))
}

fn ownership_row(
    module_id: String,
    counts: BTreeMap<String, u64>,
    author_limit: u32,
) -> OwnershipRow {
    let total = counts.values().sum::<u64>();
    let mut authors = counts
        .into_iter()
        .map(|(author, commits)| AuthorShare {
            author,
            commits,
            share: if total == 0 {
                Ratio::new(0.0).expect("zero is a ratio")
            } else {
                Ratio::new(commits as f64 / total as f64)
                    .expect("author commits cannot exceed total commits")
            },
        })
        .collect::<Vec<_>>();
    authors.sort_by(|left, right| {
        right
            .commits
            .cmp(&left.commits)
            .then_with(|| left.author.cmp(&right.author))
    });
    let (author_concentration, bus_factor) = if total == 0 {
        (
            Availability::unavailable("scope has no linked commits"),
            Availability::unavailable("scope has no linked commits"),
        )
    } else {
        let concentration = authors
            .iter()
            .map(|author| author.share.get() * author.share.get())
            .sum::<f64>()
            .clamp(0.0, 1.0);
        let mut cumulative = 0.0;
        let mut bus_factor = 0;
        for author in &authors {
            cumulative += author.share.get();
            bus_factor += 1;
            if cumulative >= 0.5 {
                break;
            }
        }
        (
            Availability::available(Ratio::new(concentration).expect("concentration is clamped")),
            Availability::available(bus_factor),
        )
    };
    OwnershipRow {
        module_id,
        commit_count: total,
        author_concentration,
        bus_factor,
        authors: bounded_page(authors, author_limit, "commits_desc,author"),
    }
}

fn scorecard(
    input: &AggregateInput<'_>,
    generated_at: DateTime<Utc>,
    hotspots: &[HotspotRow],
    cycles: &[DependencyCycle],
    ownership_rows: &[OwnershipRow],
) -> ScorecardAnalysis {
    let commit_coverage = history_source(input.provenance, |sources| &sources.commits);
    let history_reason = commit_coverage.unavailable_reason("commit history");
    let join_reason = input
        .graph
        .commit_module_edges
        .disclosure
        .reason
        .clone()
        .filter(|_| {
            input.graph.commit_module_edges.disclosure.status == DisclosureStatus::Unavailable
        });
    let age = input
        .commits
        .first()
        .and_then(|commit| parse_time(&commit.node.committed_at))
        .map(|created| (generated_at - created).num_days().max(0) as u64);
    let current_start = generated_at - Duration::days(28);
    let prior_start = generated_at - Duration::days(56);
    let current = input
        .commits
        .iter()
        .filter(|commit| {
            parse_time(&commit.node.committed_at).is_some_and(|time| time >= current_start)
        })
        .count() as f64;
    let prior = input
        .commits
        .iter()
        .filter(|commit| {
            parse_time(&commit.node.committed_at)
                .is_some_and(|time| time >= prior_start && time < current_start)
        })
        .count() as f64;
    let activity = if let Some(reason) = &history_reason {
        Availability::unavailable(reason.clone())
    } else if prior == 0.0 {
        Availability::unavailable("prior 28-day window contains no commits")
    } else {
        Availability::available(ScorecardValue::Ratio {
            value: current / prior,
        })
    };
    let top_hotspots = hotspots
        .iter()
        .filter(|row| row.quadrant == HotspotQuadrant::HighChurnHighFanIn)
        .map(|row| row.module_id.clone())
        .collect::<Vec<_>>();
    let warnings = ownership_rows
        .iter()
        .filter(|row| {
            row.commit_count > 0
                && matches!(row.bus_factor, Availability::Available { value } if value <= 1)
        })
        .map(|row| row.module_id.clone())
        .collect::<Vec<_>>();
    let repository_age = match (&history_reason, age) {
        (Some(reason), _) => Availability::unavailable(reason.clone()),
        (None, Some(value)) => Availability::available(ScorecardValue::Count { value }),
        (None, None) => Availability::unavailable("repository has no commits"),
    };
    let package_count = match &input.graph.packages.total_count {
        Availability::Available { value } => {
            Availability::available(ScorecardValue::Count { value: *value })
        }
        Availability::Unavailable { reason } => Availability::unavailable(reason.clone()),
    };
    let module_count = match &input.graph.modules.total_count {
        Availability::Available { value } => {
            Availability::available(ScorecardValue::Count { value: *value })
        }
        Availability::Unavailable { reason } => Availability::unavailable(reason.clone()),
    };
    let cycle_count =
        if input.graph.structure_edges.disclosure.status == DisclosureStatus::Unavailable {
            Availability::unavailable(
                input
                    .graph
                    .structure_edges
                    .disclosure
                    .reason
                    .clone()
                    .unwrap_or_else(|| "structure edges are unavailable".into()),
            )
        } else {
            Availability::available(ScorecardValue::Count {
                value: cycles.len() as u64,
            })
        };
    let top_hotspots = match &join_reason {
        Some(reason) => Availability::unavailable(reason.clone()),
        None => Availability::available(ScorecardValue::ModuleIds {
            value: bounded_page(top_hotspots, 10, "hotspot_rank"),
        }),
    };
    let warnings = match &join_reason {
        Some(reason) => Availability::unavailable(reason.clone()),
        None => Availability::available(ScorecardValue::ModuleIds {
            value: bounded_page(warnings, input.bounds.aggregate_rows, "module_id"),
        }),
    };
    let fields = vec![
        score_field(
            ScorecardKey::RepositoryAgeDays,
            "metrics.repository_age",
            Granularity::Repository,
            JoinTag::HistoryOnly,
            repository_age,
        ),
        score_field(
            ScorecardKey::PackageCount,
            "metrics.package_count",
            Granularity::Module,
            JoinTag::StructureOnly,
            package_count,
        ),
        score_field(
            ScorecardKey::ModuleCount,
            "metrics.module_count",
            Granularity::Module,
            JoinTag::StructureOnly,
            module_count,
        ),
        score_field(
            ScorecardKey::SymbolCount,
            "metrics.symbol_count",
            Granularity::ModuleSymbolDeferred,
            JoinTag::StructureOnly,
            Availability::unavailable("symbol-tier ingest is deferred"),
        ),
        score_field(
            ScorecardKey::ActivityTrend,
            "metrics.activity_trend",
            Granularity::Repository,
            JoinTag::HistoryOnly,
            activity,
        ),
        score_field(
            ScorecardKey::TopHotspots,
            "metrics.top_hotspots",
            Granularity::Module,
            JoinTag::Join,
            top_hotspots,
        ),
        score_field(
            ScorecardKey::DependencyCycleCount,
            "metrics.cycle_count",
            Granularity::Module,
            JoinTag::StructureOnly,
            cycle_count,
        ),
        score_field(
            ScorecardKey::OwnershipWarnings,
            "metrics.ownership_warnings",
            Granularity::Module,
            JoinTag::Join,
            warnings,
        ),
    ];
    ScorecardAnalysis {
        meta: meta(
            "views.scorecard",
            Granularity::Repository,
            JoinTag::FieldTagged,
            ViewStatus::Available,
            None,
            &[
                "aggregates.hotspot_quadrant",
                "aggregates.dependency_topology",
                "aggregates.ownership",
                "graph.commits",
                "graph.modules",
            ],
            all_history(),
            fields.len() as u32,
            "scorecard_key",
        ),
        fields,
    }
}

fn score_field(
    key: ScorecardKey,
    label_key: &str,
    granularity: Granularity,
    join: JoinTag,
    value: Availability<ScorecardValue>,
) -> ScorecardField {
    ScorecardField {
        key,
        label_key: label_key.into(),
        granularity,
        join,
        value,
    }
}

fn degrees(edges: &[&GraphEdge]) -> (BTreeMap<String, u64>, BTreeMap<String, u64>) {
    let mut incoming = BTreeMap::new();
    let mut outgoing = BTreeMap::new();
    for edge in edges {
        *incoming.entry(edge.target.clone()).or_default() += 1;
        *outgoing.entry(edge.source.clone()).or_default() += 1;
    }
    (incoming, outgoing)
}

fn dependency_cycles(nodes: &BTreeSet<String>, edges: &[&GraphEdge]) -> Vec<DependencyCycle> {
    let mut adjacency = nodes
        .iter()
        .map(|node| (node.clone(), Vec::new()))
        .collect::<BTreeMap<_, _>>();
    let mut reverse = adjacency.clone();
    for edge in edges {
        adjacency
            .entry(edge.source.clone())
            .or_default()
            .push(edge.target.clone());
        reverse
            .entry(edge.target.clone())
            .or_default()
            .push(edge.source.clone());
    }
    for values in adjacency.values_mut().chain(reverse.values_mut()) {
        values.sort();
        values.dedup();
    }
    let mut visited = BTreeSet::new();
    let mut order = Vec::new();
    for node in nodes {
        if visited.contains(node) {
            continue;
        }
        let mut stack = vec![(node.clone(), false)];
        while let Some((current, expanded)) = stack.pop() {
            if expanded {
                order.push(current);
                continue;
            }
            if !visited.insert(current.clone()) {
                continue;
            }
            stack.push((current.clone(), true));
            if let Some(neighbors) = adjacency.get(&current) {
                for neighbor in neighbors.iter().rev() {
                    if !visited.contains(neighbor) {
                        stack.push((neighbor.clone(), false));
                    }
                }
            }
        }
    }
    visited.clear();
    let mut cycles = Vec::new();
    for node in order.into_iter().rev() {
        if !visited.insert(node.clone()) {
            continue;
        }
        let mut component = Vec::new();
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            component.push(current.clone());
            if let Some(neighbors) = reverse.get(&current) {
                for neighbor in neighbors.iter().rev() {
                    if visited.insert(neighbor.clone()) {
                        stack.push(neighbor.clone());
                    }
                }
            }
        }
        component.sort();
        let self_loop = component.len() == 1
            && adjacency
                .get(&component[0])
                .is_some_and(|neighbors| neighbors.contains(&component[0]));
        if component.len() > 1 || self_loop {
            cycles.push(DependencyCycle {
                id: natural_id(
                    "dependency_cycle",
                    &component.iter().map(String::as_str).collect::<Vec<_>>(),
                ),
                module_ids: component,
            });
        }
    }
    cycles.sort_by(|left, right| left.id.cmp(&right.id));
    cycles
}

#[allow(clippy::too_many_arguments)]
fn meta(
    label_key: &str,
    granularity: Granularity,
    join: JoinTag,
    status: ViewStatus,
    unavailable_reason: Option<String>,
    inputs: &[&str],
    window: AnalysisWindow,
    max_items: u32,
    order: &str,
) -> AnalysisMeta {
    AnalysisMeta {
        label_key: label_key.into(),
        granularity,
        join,
        status,
        unavailable_reason,
        inputs: inputs.iter().map(|value| (*value).to_string()).collect(),
        window,
        bound: PageBound {
            kind: BoundKind::TopN,
            max_items,
            order: order.into(),
        },
    }
}

fn covered_analysis_page<T>(
    items: Vec<T>,
    max_items: u32,
    order: &str,
    unavailable_reason: Option<&str>,
) -> Page<T> {
    match unavailable_reason {
        Some(reason) => Page::unavailable(max_items, order, reason),
        None => bounded_page(items, max_items, order),
    }
}

fn all_history() -> AnalysisWindow {
    AnalysisWindow {
        kind: WindowKind::AllHistory,
        start: None,
        end: None,
        days: None,
    }
}

fn rolling_days(days: u32, end: DateTime<Utc>) -> AnalysisWindow {
    AnalysisWindow {
        kind: WindowKind::RollingDays,
        start: Some(
            Timestamp::parse((end - Duration::days(i64::from(days))).to_rfc3339())
                .expect("chrono produced RFC3339"),
        ),
        end: Some(Timestamp::parse(end.to_rfc3339()).expect("chrono produced RFC3339")),
        days: Some(days),
    }
}

fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn week_start(date: NaiveDate) -> NaiveDate {
    date - Duration::days(i64::from(date.weekday().num_days_from_monday()))
}

fn increment_week(weeks: &mut BTreeMap<NaiveDate, u64>, timestamp: DateTime<Utc>) {
    *weeks.entry(week_start(timestamp.date_naive())).or_default() += 1;
}

fn update_earliest(earliest: &mut Option<DateTime<Utc>>, timestamp: DateTime<Utc>) {
    if earliest.is_none_or(|current| timestamp < current) {
        *earliest = Some(timestamp);
    }
}

fn cadence_page(
    weeks: BTreeMap<NaiveDate, u64>,
    max_items: u32,
    coverage: &SourceCoverage,
    label: &str,
    partial_reason: Option<String>,
) -> Page<CadencePoint> {
    let items = weeks
        .into_iter()
        .map(|(week, count)| CadencePoint {
            week_start: week.format("%Y-%m-%d").to_string(),
            count,
        })
        .collect();
    match coverage.unavailable_reason(label).or(partial_reason) {
        Some(reason) => Page::unavailable(max_items, "week_start", reason),
        None => bounded_page(items, max_items, "week_start"),
    }
}

fn history_source(
    provenance: &PipelineProvenance,
    pick: impl FnOnce(&HistorySourceCoverage) -> &SourceCoverage,
) -> SourceCoverage {
    match &provenance.git_digest {
        Availability::Available { value } => pick(&value.sources).clone(),
        Availability::Unavailable { reason } => SourceCoverage::Unknown {
            reason: reason.clone(),
        },
    }
}

fn percentile(sorted: &[f64], quantile: f64) -> f64 {
    let index = ((sorted.len() - 1) as f64 * quantile).round() as usize;
    sorted[index]
}

fn median_nonzero(values: impl Iterator<Item = u64>) -> u64 {
    let mut values = values.filter(|value| *value > 0).collect::<Vec<_>>();
    if values.is_empty() {
        return 1;
    }
    values.sort_unstable();
    values[values.len() / 2]
}

fn ordered_pair(left: &str, right: &str) -> (String, String) {
    if left <= right {
        (left.to_string(), right.to_string())
    } else {
        (right.to_string(), left.to_string())
    }
}

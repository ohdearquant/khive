//! `kkernel kg commit` — the tier-2 change-set commit primitive.
//!
//! Restores the `kg commit` verb [ADR-020](../../../../docs/adr/ADR-020-git-native-kg-implementation.md)
//! §5 specified but never shipped, scoped per [ADR-102](../../../../docs/adr/ADR-102-tiered-validate-and-merge.md)'s
//! "Amendment to ADR-020": this is the commit step for an already-staged
//! [ADR-101](../../../../docs/adr/ADR-101-kg-changeset-model.md) change-set, run
//! against ADR-102's own local-only staged-change-set/snapshot repository
//! (D6) — never the project-repository-embedded `.khive/kg/` layout the other
//! `kg` verbs operate on.
//!
//! # What this command does
//!
//! 1. Parse the change-set NDJSON-delta file via `khive_changeset::from_ndjson`
//!    (fail-loud on any parse/schema error — malformed input never reaches
//!    step 2).
//! 2. Project the change-set's `create`/`link` ops into synthetic
//!    `entities.ndjson` / `notes.ndjson` / `edges.ndjson` content and run a
//!    **subset** of the same rule pass `kkernel kg validate` uses against
//!    them (see "Commit-time validation scope" below). Any `error`-severity
//!    finding refuses the commit.
//! 3. On a clean pass: `git add` the change-set file into the target repo and
//!    `git commit`, carrying the ADR-101 D4 provenance trailers. Refuses
//!    (fail-loud, before touching git) if the target repo has any configured
//!    remote (ADR-102 D6).
//!
//! # Commit-time validation scope
//!
//! A change-set is a **partial** view of the graph: most `link` ops target
//! entities or notes created by an *earlier*, already-committed change-set,
//! not by this one. Rule classes that assume a complete known-ID universe
//! (`referential-integrity`, `dangling-refs`) would therefore flag the
//! overwhelming majority of ordinary edges as broken if run against this
//! change-set alone — a false-positive storm, not a real finding. Those two
//! classes are **not evaluated here**; they are deferred to stage time, where
//! the producer/reviewer has (or can obtain) full graph context, per
//! ADR-102 D5's own framing of `dangling-refs` as an offline, dataset-scoped
//! check. `edge-endpoint-types` and `edge-direction-conventions` do not need
//! this exclusion: both already skip any edge whose endpoint fails to resolve
//! within the given NDJSON dataset (see `validate::check_edge_endpoint_types`),
//! so restricting them to this change-set's own `create` ops degrades
//! gracefully to "check what we can see" rather than false-flagging.
//!
//! `update`, `delete`, and `merge` ops are not re-projected into the
//! synthetic view: they patch or remove records that already exist outside
//! this change-set, so this command has no fresh kind/name/relation data to
//! check for them beyond what ADR-102 D2 already routes to tier-2 review by
//! construction (`delete`, `merge`, and any edge-relation/weight change are
//! *always* tier-2). Re-validating already-reviewed preimage data offline
//! here would not catch anything new.
//!
//! No SQLite handle is opened anywhere in this module (ADR-102 D5 topology
//! guard) — `validate::build_taxonomy` builds its registry with `db_path:
//! None`, exactly as `kg validate` already does, and every NDJSON read below
//! is a plain file read against the synthetic projection or the change-set
//! file itself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use khive_changeset::{ChangeSet, CreateTarget, Op};

use super::types::{CommitArgs, CommitReport, OutputFormat, RuleResult, Violation};
use super::validate;

pub(super) fn cmd_commit(args: CommitArgs) -> Result<()> {
    // ── 1. Parse the change-set (fail-loud) ────────────────────────────────
    let changeset_text = std::fs::read_to_string(&args.changeset)
        .with_context(|| format!("reading change-set {}", args.changeset.display()))?;
    let changeset = khive_changeset::from_ndjson(&changeset_text).with_context(|| {
        format!(
            "parsing change-set {} as NDJSON-delta",
            args.changeset.display()
        )
    })?;

    if !args.rules.exists() {
        bail!(
            "rules file not found: {} — `kg commit` requires an explicit rules \
             file (ADR-102 D2: the tier predicate and its rule pass live in this \
             file, not a default)",
            args.rules.display()
        );
    }

    // ── 2. Commit-time validation ───────────────────────────────────────────
    let rule_results =
        run_commit_time_rules(&changeset, &args.rules).context("running commit-time rule pass")?;

    let errors: usize = rule_results
        .iter()
        .filter(|r| r.severity == "error" && !r.passed)
        .count();
    let warnings: usize = rule_results
        .iter()
        .filter(|r| r.severity == "warning" && !r.passed)
        .count();
    let info: usize = rule_results
        .iter()
        .filter(|r| r.severity == "info" && !r.passed)
        .count();
    let passed = errors == 0;

    print_report(&args.format, &rule_results, errors, warnings, info, passed);

    if !passed {
        // Refuse to commit: an error-severity finding on a staged change-set
        // means either the tier predicate under-tiered it, or a producer bug
        // slipped past stage-time review. Either way this primitive is the
        // last gate before the write lands, so it re-checks and refuses
        // rather than trusting an upstream "already reviewed" claim.
        std::process::exit(1);
    }

    // ── 3. Git add + commit ─────────────────────────────────────────────────
    let repo = args
        .repo
        .canonicalize()
        .with_context(|| format!("resolving repo path {}", args.repo.display()))?;
    if !repo.join(".git").exists() {
        bail!(
            "{} is not a git repository (no .git); run `git init` in the \
             staged change-set/snapshot repo first",
            repo.display()
        );
    }

    ensure_no_remote(&repo)?;

    let rel_path = stage_changeset_file(&repo, &args.changeset)?;

    run_git_ok(&repo, &["add", "--", &rel_path])?;

    // ADR-101 D4: the batch trailer is envelope-first — an explicit
    // `Envelope::batch_id` wins; the `producer@staged_atus` form is only the
    // derived fallback for envelopes staged before batch_id existed.
    let producer_batch = match &changeset.envelope.batch_id {
        Some(batch_id) => batch_id.clone(),
        None => format!(
            "{}@{}us",
            changeset.envelope.producer,
            changeset.envelope.staged_at.as_micros()
        ),
    };
    let full_message = format!(
        "{}\n\nChange-Set-Producer: {}\nChange-Set-Producer-Batch: {}\n",
        args.message.trim_end(),
        sanitize_trailer_value(&changeset.envelope.producer),
        sanitize_trailer_value(&producer_batch),
    );
    let message_file =
        tempfile::NamedTempFile::new().context("creating commit-message temp file")?;
    std::fs::write(message_file.path(), &full_message).context("writing commit message")?;

    let message_path = message_file
        .path()
        .to_str()
        .context("commit-message temp path is not valid UTF-8")?;
    run_git_ok(&repo, &["commit", "-F", message_path])?;

    let commit_sha = run_git_ok(&repo, &["rev-parse", "HEAD"])?;

    let report = CommitReport {
        commit_sha,
        changeset_path: rel_path,
        ops: changeset.ops.len(),
        producer: changeset.envelope.producer.clone(),
        producer_batch,
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("serialize CommitReport")
    );
    Ok(())
}

// ── Commit-time rule pass ───────────────────────────────────────────────────

/// Synthetic NDJSON projection of a change-set's `create`/`link` ops, in the
/// same per-substrate file shape `kg validate`'s rule functions read.
struct ProjectedNdjson {
    entities: String,
    notes: String,
    edges: String,
    /// Stage-time ids minted more than once by `create` ops in this
    /// change-set (across both entity and note targets).
    duplicate_ids: Vec<String>,
}

fn project_changeset(changeset: &ChangeSet) -> ProjectedNdjson {
    let mut entities = String::new();
    let mut notes = String::new();
    let mut edges = String::new();
    let mut seen_ids: HashMap<String, ()> = HashMap::new();
    let mut duplicate_ids = Vec::new();

    for op in &changeset.ops {
        match op {
            Op::Create(create) => {
                let id_str = create.id.to_string();
                if seen_ids.insert(id_str.clone(), ()).is_some() {
                    duplicate_ids.push(id_str.clone());
                }
                match &create.target {
                    CreateTarget::Entity(fields) => {
                        // Project every `EntityCreateFields` field the rule
                        // pass can read: `entity_type` (pack `EDGE_RULES` /
                        // `edge-endpoint-types`, via `collect_kind_map`) and
                        // `description` (generic `[[rules]] require_field`)
                        // are both consulted by `kg validate`'s rule classes,
                        // not just `id`/`kind`/`name`/`properties`/`tags`.
                        // Projecting a narrower record than the staged op
                        // causes false rejections and vacuous rule passes.
                        let rec = serde_json::json!({
                            "id": id_str,
                            "kind": serde_json::to_value(fields.entity_kind)
                                .expect("EntityKind serializes"),
                            "entity_type": fields.entity_type,
                            "name": fields.name,
                            "description": fields.description,
                            "properties": serde_json::to_value(&fields.properties)
                                .expect("properties serialize"),
                            "tags": fields.tags,
                        });
                        entities.push_str(&serde_json::to_string(&rec).expect("json line"));
                        entities.push('\n');
                    }
                    CreateTarget::Note(fields) => {
                        // Project every `NoteCreateFields` field the rule
                        // pass can read (generic `[[rules]]` can
                        // `require_field`/condition on any top-level field,
                        // not just `kind`/`properties`/`tags`).
                        let rec = serde_json::json!({
                            "id": id_str,
                            "kind": fields.note_kind,
                            "content": fields.content,
                            "properties": serde_json::to_value(&fields.properties)
                                .expect("properties serialize"),
                            "tags": fields.tags,
                            "salience": fields.salience,
                            "decay_factor": fields.decay_factor,
                        });
                        notes.push_str(&serde_json::to_string(&rec).expect("json line"));
                        notes.push('\n');
                    }
                }
            }
            Op::Link(link) => {
                let rec = serde_json::json!({
                    "edge_id": link.id.to_string(),
                    "source_id": link.source.to_string(),
                    "target_id": link.target.to_string(),
                    "relation": serde_json::to_value(link.relation)
                        .expect("EdgeRelation serializes"),
                    "weight": link.weight,
                    "properties": serde_json::to_value(&link.properties)
                        .expect("properties serialize"),
                });
                edges.push_str(&serde_json::to_string(&rec).expect("json line"));
                edges.push('\n');
            }
            // `update`/`delete`/`merge` are out of scope for commit-time
            // re-validation — see the module-level doc comment.
            Op::Update(_) | Op::Delete(_) | Op::Merge(_) => {}
        }
    }

    ProjectedNdjson {
        entities,
        notes,
        edges,
        duplicate_ids,
    }
}

fn check_no_duplicate_stage_ids(duplicate_ids: &[String]) -> RuleResult {
    let violations: Vec<Violation> = duplicate_ids
        .iter()
        .map(|id| Violation {
            entity_id: Some(id.clone()),
            entity_name: None,
            entity_kind: None,
            rule_id: "no-duplicate-uuids".into(),
            severity: "error",
            message: format!("Duplicate stage-time id within change-set: {id}"),
            fixable: false,
        })
        .collect();
    RuleResult {
        id: "no-duplicate-uuids".into(),
        severity: "error",
        passed: violations.is_empty(),
        violations,
    }
}

/// Run the commit-time rule subset against `changeset`, using `rules_path`
/// for the configurable rule classes. See the module doc comment for exactly
/// which classes run and why the built-in `dangling-refs` *finding* is not
/// meaningful against a partial change-set view. That exclusion is done by
/// calling `validate::configurable_rule_checks_partial_view`, which never
/// invokes the built-in dangling-ref evaluator — it is NOT a post-hoc filter
/// over the returned `RuleResult`s by public id. A post-hoc `id ==
/// "dangling-refs"` filter would also swallow the malformed-config error
/// result `validate_severity` emits under that same id, and any generic
/// `[[rules]]` entry a rules author happens to name `"dangling-refs"` — both
/// of which must still fail the commit.
fn run_commit_time_rules(changeset: &ChangeSet, rules_path: &Path) -> Result<Vec<RuleResult>> {
    let projected = project_changeset(changeset);

    let tmp = tempfile::TempDir::new().context("creating projection temp dir")?;
    let entities_path = tmp.path().join("entities.ndjson");
    let notes_path = tmp.path().join("notes.ndjson");
    let edges_path = tmp.path().join("edges.ndjson");
    std::fs::write(&entities_path, &projected.entities).context("writing projected entities")?;
    std::fs::write(&notes_path, &projected.notes).context("writing projected notes")?;
    std::fs::write(&edges_path, &projected.edges).context("writing projected edges")?;

    let taxonomy = validate::build_taxonomy().context("building KG taxonomy")?;

    let mut results = vec![
        check_no_duplicate_stage_ids(&projected.duplicate_ids),
        validate::check_valid_entity_kinds(&entities_path, &taxonomy.entity_kinds),
        validate::check_valid_note_kinds(&notes_path, &taxonomy.note_kinds),
    ];

    if rules_path.exists() {
        let configurable = validate::configurable_rule_checks_partial_view(
            &entities_path,
            &edges_path,
            &notes_path,
            rules_path,
        )
        .context("evaluating configurable rules")?;
        results.extend(configurable);
    }

    Ok(results)
}

fn print_report(
    format: &OutputFormat,
    rule_results: &[RuleResult],
    errors: usize,
    warnings: usize,
    info: usize,
    passed: bool,
) {
    match format {
        OutputFormat::Json => {
            let payload = serde_json::json!({
                "rules": rule_results,
                "summary": {
                    "errors": errors,
                    "warnings": warnings,
                    "info": info,
                    "passed": passed,
                },
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&payload).expect("serialize commit report")
            );
        }
        OutputFormat::Github => {
            for r in rule_results {
                for v in &r.violations {
                    let level = if r.severity == "error" {
                        "error"
                    } else {
                        "warning"
                    };
                    println!("::{level} ::{}", v.message);
                }
            }
        }
        OutputFormat::Text => {
            for r in rule_results {
                let symbol = if r.passed {
                    "\u{2713}"
                } else if r.severity == "error" {
                    "\u{2717}"
                } else {
                    "\u{26a0}"
                };
                println!("  {symbol} {}: {} violation(s)", r.id, r.violations.len());
                for v in &r.violations {
                    println!("    - {}", v.message);
                }
            }
            println!("\nSummary: {errors} error(s), {warnings} warning(s), {info} info");
            if passed {
                println!("clean pass — proceeding to commit");
            } else {
                println!("refusing to commit: {errors} error-severity finding(s)");
            }
        }
    }
}

// ── Git ──────────────────────────────────────────────────────────────────────

/// Refuse (ADR-102 D6) if `repo` has any configured git remote.
fn ensure_no_remote(repo: &Path) -> Result<()> {
    let stdout = run_git_ok(repo, &["remote"])?;
    let remotes: Vec<&str> = stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if !remotes.is_empty() {
        bail!(
            "refusing to commit: {} has configured git remote(s) [{}] — ADR-102 D6 \
             requires the staged change-set/snapshot repository to be local-only; \
             remove the remote(s) before running `kg commit`",
            repo.display(),
            remotes.join(", ")
        );
    }
    Ok(())
}

/// Ensure the change-set file is present inside `repo` and return its path
/// relative to `repo` (the path `git add`/`git commit` operate on).
///
/// If `changeset_src` already lives under `repo`, it is used in place. If
/// not — the common case for a producer staging into a scratch directory
/// before commit — it is copied into `<repo>/.khive/kg/changesets/<name>`,
/// creating that directory if needed, so the committed artifact always has a
/// stable, auditable home inside the target repository.
fn stage_changeset_file(repo: &Path, changeset_src: &Path) -> Result<String> {
    let repo_abs = repo
        .canonicalize()
        .with_context(|| format!("resolving repo path {}", repo.display()))?;
    let src_abs = changeset_src
        .canonicalize()
        .with_context(|| format!("resolving change-set path {}", changeset_src.display()))?;

    if let Ok(rel) = src_abs.strip_prefix(&repo_abs) {
        return Ok(rel.to_string_lossy().into_owned());
    }

    let file_name = changeset_src
        .file_name()
        .context("change-set path has no file name")?;
    let dest_dir = repo.join(".khive/kg/changesets");
    std::fs::create_dir_all(&dest_dir)
        .with_context(|| format!("creating {}", dest_dir.display()))?;
    let dest = dest_dir.join(file_name);
    std::fs::copy(&src_abs, &dest).with_context(|| {
        format!(
            "copying change-set {} into {}",
            src_abs.display(),
            dest.display()
        )
    })?;

    let rel: PathBuf = [".khive", "kg", "changesets"]
        .iter()
        .collect::<PathBuf>()
        .join(file_name);
    Ok(rel.to_string_lossy().into_owned())
}

fn run_git_ok(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        // This repo's whole purpose (ADR-102 D6) is committing exported KG
        // NDJSON — exactly the pattern the machine-wide `check-json-data.sh`
        // leak guard (`~/.git-hooks`, `core.hooksPath`) exists to catch by
        // default. `KHIVE_ALLOW_DATA=1` is that hook's own documented,
        // auditable bypass, not `--no-verify`; the hook's header comment
        // names this exact commit path ("khive-vcs kg git-sync write path
        // ... commits .khive/kg/*.ndjson BY DESIGN; it sets
        // KHIVE_ALLOW_DATA=1 on its own git subprocesses") as the precedent
        // this follows. A no-op for every non-commit invocation below.
        .env("KHIVE_ALLOW_DATA", "1")
        .output()
        .with_context(|| format!("running git {} in {}", args.join(" "), repo.display()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            repo.display(),
            stderr.trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Collapse embedded newlines so a value is safe to carry as a one-line
/// `git interpret-trailers`-shaped `Key: value` trailer.
fn sanitize_trailer_value(value: &str) -> String {
    value.replace(['\n', '\r'], " ").trim().to_string()
}

#[cfg(test)]
mod tests {
    use khive_changeset::{
        CreateOp, CreateTarget, EntityCreateFields, Envelope, LinkOp, NoteCreateFields,
    };
    use khive_types::{EdgeRelation, EntityKind, Id128, Namespace, Timestamp};
    use tempfile::TempDir;

    use super::*;

    fn run_git(dir: &Path, args: &[&str]) {
        // Hermetic: machine-wide hooks (e.g. leak-guard via core.hooksPath)
        // must not block commits inside throwaway test repos. Mirrors the
        // `run_git` test helper in `khive-vcs::sync::tests` / `kg::fetch::tests`.
        let status = std::process::Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap_or_else(|e| panic!("git {} failed to spawn: {e}", args.join(" ")));
        assert!(
            status.success(),
            "git {} exited with {}",
            args.join(" "),
            status
        );
    }

    fn init_repo(dir: &Path) {
        run_git(dir, &["init", "-b", "main"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
        run_git(dir, &["config", "user.name", "Test"]);
        // Persisted (not just a `-c` override on this call): the global
        // `check-json-data.sh` leak guard reads `core.hooksPath` from repo
        // config on every subsequent `git commit`, including the ones issued
        // by production code under test (`run_git_ok`, which never passes
        // `-c` itself). Per that hook's own documented guidance: "Fix is
        // test-side hermeticity ... Never add hook exemptions for test
        // paths."
        run_git(dir, &["config", "core.hooksPath", "/dev/null"]);
        run_git(dir, &["commit", "--allow-empty", "-m", "init"]);
    }

    fn sample_changeset() -> ChangeSet {
        let envelope = Envelope::new("agent:test", "family:sonnet", Timestamp::from_secs(1));
        let create = Op::Create(CreateOp {
            id: Id128::from_u128(1),
            namespace: Namespace::local(),
            target: CreateTarget::Entity(EntityCreateFields {
                entity_kind: EntityKind::Concept,
                entity_type: None,
                name: "X".into(),
                description: None,
                properties: Default::default(),
                tags: vec![],
            }),
        });
        ChangeSet::new(envelope, vec![create])
    }

    fn write_changeset(dir: &Path, cs: &ChangeSet) -> PathBuf {
        let path = dir.join("changeset.ndjson");
        std::fs::write(&path, khive_changeset::to_ndjson(cs).unwrap()).unwrap();
        path
    }

    // ── Projection / commit-time rule pass ─────────────────────────────────

    #[test]
    fn project_changeset_emits_entity_and_edge_records() {
        let a = Id128::from_u128(1);
        let b = Id128::from_u128(2);
        let envelope = Envelope::new("agent:test", "family:sonnet", Timestamp::from_secs(1));
        let ops = vec![
            Op::Create(CreateOp {
                id: a,
                namespace: Namespace::local(),
                target: CreateTarget::Entity(EntityCreateFields {
                    entity_kind: EntityKind::Concept,
                    entity_type: None,
                    name: "A".into(),
                    description: None,
                    properties: Default::default(),
                    tags: vec![],
                }),
            }),
            Op::Create(CreateOp {
                id: b,
                namespace: Namespace::local(),
                target: CreateTarget::Entity(EntityCreateFields {
                    entity_kind: EntityKind::Concept,
                    entity_type: None,
                    name: "B".into(),
                    description: None,
                    properties: Default::default(),
                    tags: vec![],
                }),
            }),
            Op::Link(LinkOp {
                id: Id128::from_u128(3),
                namespace: Namespace::local(),
                source: a,
                target: b,
                relation: EdgeRelation::Extends,
                weight: 1.0,
                properties: Default::default(),
            }),
        ];
        let cs = ChangeSet::new(envelope, ops);
        let projected = project_changeset(&cs);
        assert_eq!(projected.entities.lines().count(), 2);
        assert_eq!(projected.edges.lines().count(), 1);
        assert!(projected.duplicate_ids.is_empty());
        assert!(projected.edges.contains("extends"));
    }

    #[test]
    fn project_changeset_flags_duplicate_stage_ids() {
        let dup = Id128::from_u128(1);
        let envelope = Envelope::new("agent:test", "family:sonnet", Timestamp::from_secs(1));
        let mk = || {
            Op::Create(CreateOp {
                id: dup,
                namespace: Namespace::local(),
                target: CreateTarget::Entity(EntityCreateFields {
                    entity_kind: EntityKind::Concept,
                    entity_type: None,
                    name: "dup".into(),
                    description: None,
                    properties: Default::default(),
                    tags: vec![],
                }),
            })
        };
        let cs = ChangeSet::new(envelope, vec![mk(), mk()]);
        let projected = project_changeset(&cs);
        assert_eq!(projected.duplicate_ids.len(), 1);
    }

    #[test]
    fn commit_time_rules_reject_invalid_note_kind() {
        let envelope = Envelope::new("agent:test", "family:sonnet", Timestamp::from_secs(1));
        let ops = vec![Op::Create(CreateOp {
            id: Id128::from_u128(1),
            namespace: Namespace::local(),
            target: CreateTarget::Note(NoteCreateFields {
                note_kind: "not_a_real_kind".into(),
                content: "hello".into(),
                properties: Default::default(),
                tags: vec![],
                salience: None,
                decay_factor: None,
            }),
        })];
        let cs = ChangeSet::new(envelope, ops);
        let tmp = TempDir::new().unwrap();
        let rules_path = tmp.path().join("rules.toml");
        std::fs::write(&rules_path, "").unwrap();

        let results = run_commit_time_rules(&cs, &rules_path).unwrap();
        let note_rule = results
            .iter()
            .find(|r| r.id == "valid-note-kinds")
            .expect("valid-note-kinds must run");
        assert!(!note_rule.passed);
    }

    #[test]
    fn commit_time_rules_exclude_dangling_refs_from_results() {
        // A link whose endpoints are not part of this change-set (the
        // ordinary case: they were created by an earlier committed
        // change-set) must not be reported as a dangling-refs violation.
        let envelope = Envelope::new("agent:test", "family:sonnet", Timestamp::from_secs(1));
        let ops = vec![Op::Link(LinkOp {
            id: Id128::from_u128(3),
            namespace: Namespace::local(),
            source: Id128::from_u128(100),
            target: Id128::from_u128(200),
            relation: EdgeRelation::Extends,
            weight: 1.0,
            properties: Default::default(),
        })];
        let cs = ChangeSet::new(envelope, ops);
        let tmp = TempDir::new().unwrap();
        let rules_path = tmp.path().join("rules.toml");
        std::fs::write(
            &rules_path,
            "[dangling_refs]\nenabled = true\nseverity = \"error\"\n",
        )
        .unwrap();

        let results = run_commit_time_rules(&cs, &rules_path).unwrap();
        assert!(
            !results.iter().any(|r| r.id == "dangling-refs"),
            "dangling-refs must be excluded from commit-time results: {results:?}"
        );
    }

    // ── Git flow ─────────────────────────────────────────────────────────────

    #[test]
    fn ensure_no_remote_passes_for_remote_free_repo() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path());
        ensure_no_remote(tmp.path()).unwrap();
    }

    #[test]
    fn ensure_no_remote_refuses_when_remote_configured() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path());
        run_git(
            tmp.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        );
        let err = ensure_no_remote(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("local-only"), "{err}");
    }

    #[test]
    fn stage_changeset_file_copies_external_file_into_repo() {
        let repo_tmp = TempDir::new().unwrap();
        init_repo(repo_tmp.path());
        let outside_tmp = TempDir::new().unwrap();
        let cs = sample_changeset();
        let src = write_changeset(outside_tmp.path(), &cs);

        let rel = stage_changeset_file(repo_tmp.path(), &src).unwrap();
        assert_eq!(rel, ".khive/kg/changesets/changeset.ndjson");
        assert!(repo_tmp.path().join(&rel).exists());
    }

    #[test]
    fn stage_changeset_file_uses_in_place_path_when_already_inside_repo() {
        let repo_tmp = TempDir::new().unwrap();
        init_repo(repo_tmp.path());
        let cs = sample_changeset();
        let src = write_changeset(repo_tmp.path(), &cs);

        let rel = stage_changeset_file(repo_tmp.path(), &src).unwrap();
        assert_eq!(rel, "changeset.ndjson");
    }

    #[test]
    fn sanitize_trailer_value_collapses_newlines() {
        assert_eq!(sanitize_trailer_value("a\nb\r\nc"), "a b  c");
    }

    // ── End-to-end `cmd_commit` ─────────────────────────────────────────────

    #[test]
    fn cmd_commit_lands_clean_changeset_and_carries_trailers() {
        let repo_tmp = TempDir::new().unwrap();
        init_repo(repo_tmp.path());
        let stage_tmp = TempDir::new().unwrap();
        let cs = sample_changeset();
        let changeset_path = write_changeset(stage_tmp.path(), &cs);
        let rules_path = stage_tmp.path().join("rules.toml");
        std::fs::write(&rules_path, "").unwrap();

        let args = CommitArgs {
            changeset: changeset_path,
            rules: rules_path,
            repo: repo_tmp.path().to_path_buf(),
            message: "stage batch 1".into(),
            format: OutputFormat::Json,
        };
        cmd_commit(args).expect("clean change-set must commit");

        let log = run_git_ok(repo_tmp.path(), &["log", "-1", "--pretty=%B"]).unwrap();
        assert!(log.contains("stage batch 1"));
        assert!(log.contains("Change-Set-Producer: agent:test"));
        assert!(log.contains("Change-Set-Producer-Batch: agent:test@1000000us"));

        assert!(repo_tmp
            .path()
            .join(".khive/kg/changesets/changeset.ndjson")
            .exists());
    }

    #[test]
    fn cmd_commit_prefers_envelope_batch_id_over_derived_form() {
        let repo_tmp = TempDir::new().unwrap();
        init_repo(repo_tmp.path());
        let stage_tmp = TempDir::new().unwrap();
        let mut cs = sample_changeset();
        cs.envelope = cs.envelope.with_batch_id("batch-explicit-42");
        let changeset_path = write_changeset(stage_tmp.path(), &cs);
        let rules_path = stage_tmp.path().join("rules.toml");
        std::fs::write(&rules_path, "").unwrap();

        let args = CommitArgs {
            changeset: changeset_path,
            rules: rules_path,
            repo: repo_tmp.path().to_path_buf(),
            message: "stage batch 2".into(),
            format: OutputFormat::Json,
        };
        cmd_commit(args).expect("clean change-set must commit");

        let log = run_git_ok(repo_tmp.path(), &["log", "-1", "--pretty=%B"]).unwrap();
        assert!(log.contains("Change-Set-Producer-Batch: batch-explicit-42"));
        assert!(!log.contains("agent:test@1000000us"));
    }

    #[test]
    fn cmd_commit_refuses_repo_with_remote() {
        let repo_tmp = TempDir::new().unwrap();
        init_repo(repo_tmp.path());
        run_git(
            repo_tmp.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        );
        let stage_tmp = TempDir::new().unwrap();
        let cs = sample_changeset();
        let changeset_path = write_changeset(stage_tmp.path(), &cs);
        let rules_path = stage_tmp.path().join("rules.toml");
        std::fs::write(&rules_path, "").unwrap();

        let args = CommitArgs {
            changeset: changeset_path,
            rules: rules_path,
            repo: repo_tmp.path().to_path_buf(),
            message: "should not land".into(),
            format: OutputFormat::Json,
        };
        let err = cmd_commit(args).unwrap_err();
        assert!(err.to_string().contains("local-only"), "{err}");

        // No commit landed beyond the init commit.
        let log = run_git_ok(repo_tmp.path(), &["log", "--oneline"]).unwrap();
        assert_eq!(log.lines().count(), 1, "only the init commit must exist");
    }
}

//! Read-only, entity-aware presentation of Git's NDJSON working-tree patch.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::path::Path;

use anyhow::{bail, Context, Result};
use khive_repo_showcase::git_safety::hardened_git_command_for_repo;
use serde_json::{Map, Value};
use uuid::Uuid;

use super::types::DiffArgs;

const ENTITIES: &str = ".khive/kg/entities.ndjson";
const EDGES: &str = ".khive/kg/edges.ndjson";
type Records = BTreeMap<Uuid, Map<String, Value>>;

pub(super) fn cmd_diff(args: DiffArgs) -> Result<()> {
    let root = git(&args.repo, &["rev-parse", "--show-toplevel"])?;
    let repo = Path::new(
        root.strip_suffix('\n')
            .context("missing Git repository path")?,
    );
    let commit = git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{}^{{commit}}", args.reference),
        ],
    )?;
    let commit = commit.trim();
    if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("Git returned an invalid commit ID");
    }
    let (old_entities, new_entities) = changed_records(repo, commit, ENTITIES, "id")?;
    let (old_edges, new_edges) = changed_records(repo, commit, EDGES, "edge_id")?;
    let mut current_entities = Records::new();
    match std::fs::read_to_string(repo.join(ENTITIES)) {
        Ok(text) => {
            for (line, record) in text.lines().enumerate() {
                if !record.trim().is_empty() {
                    insert_record(&mut current_entities, record, "id")
                        .with_context(|| format!("{ENTITIES}: line {}", line + 1))?;
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("reading working-tree entities"),
    }
    let mut previous_names = current_entities.clone();
    previous_names.extend(old_entities.clone());
    let mut output = String::new();
    render(
        &mut output,
        "entity",
        &old_entities,
        &new_entities,
        &previous_names,
        &current_entities,
    );
    render(
        &mut output,
        "edge",
        &old_edges,
        &new_edges,
        &previous_names,
        &current_entities,
    );
    if output.is_empty() {
        println!("No KG changes.");
    } else {
        print!("{output}");
    }
    Ok(())
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let output = hardened_git_command_for_repo(repo)
        .context("inspect repository Git filters")?
        .args([
            "--no-pager",
            "--no-replace-objects",
            "-c",
            "protocol.allow=never",
        ])
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_NO_LAZY_FETCH", "1")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .output()
        .context("run read-only Git command")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("Git output is not UTF-8")
}

fn changed_records(
    repo: &Path,
    commit: &str,
    path: &str,
    id_key: &str,
) -> Result<(Records, Records)> {
    let patch = git(
        repo,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--no-renames",
            "--text",
            "--unified=0",
            "--inter-hunk-context=0",
            "--output-indicator-new=+",
            "--output-indicator-old=-",
            "--output-indicator-context= ",
            commit,
            "--",
            path,
        ],
    )?;
    let mut before = Records::new();
    let mut after = Records::new();
    let mut in_hunk = false;
    for line in patch.lines() {
        if line.starts_with("@@ ") {
            in_hunk = true;
        } else if in_hunk {
            let (records, text) = match line.as_bytes().first() {
                Some(b'-') => (&mut before, &line[1..]),
                Some(b'+') => (&mut after, &line[1..]),
                _ => continue,
            };
            if !text.trim().is_empty() {
                insert_record(records, text, id_key)
                    .with_context(|| format!("invalid changed record in {path}"))?;
            }
        }
    }
    Ok((before, after))
}

fn insert_record(records: &mut Records, text: &str, id_key: &str) -> Result<()> {
    let record: Map<String, Value> =
        serde_json::from_str(text).context("expected a JSON object")?;
    let id = Uuid::parse_str(required_string(&record, id_key)?).context("invalid record UUID")?;
    if id_key == "id" {
        required_string(&record, "name")?;
        required_string(&record, "kind")?;
    } else {
        for key in ["source", "target", "relation"] {
            required_string(&record, key)?;
        }
    }
    if record
        .get("properties")
        .is_some_and(|value| !value.is_object())
    {
        bail!("record {id} properties must be an object");
    }
    if records.insert(id, record).is_some() {
        bail!("duplicate record UUID {id}");
    }
    Ok(())
}

fn required_string<'a>(record: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    record
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("{key} must be a string"))
}

fn endpoint(value: &Value, names: &Records) -> String {
    let raw = value.as_str().expect("validated endpoint string");
    match Uuid::parse_str(raw).ok().and_then(|id| names.get(&id)) {
        Some(record) => format!("{raw} ({})", record["name"]),
        None => value.to_string(),
    }
}

fn render(
    output: &mut String,
    substrate: &str,
    before: &Records,
    after: &Records,
    old_names: &Records,
    new_names: &Records,
) {
    for id in before.keys().chain(after.keys()).collect::<BTreeSet<_>>() {
        let old = before.get(id);
        let new = after.get(id);
        if old == new {
            continue;
        }
        let (marker, record, names) = match (old, new) {
            (None, Some(record)) => ('+', record, new_names),
            (Some(record), None) => ('-', record, old_names),
            (Some(_), Some(record)) => ('~', record, new_names),
            (None, None) => continue,
        };
        if substrate == "entity" {
            writeln!(
                output,
                "{marker} entity {id} ({} {})",
                record["kind"], record["name"]
            )
            .unwrap();
        } else {
            writeln!(
                output,
                "{marker} edge {id} {} --[{}]--> {}",
                endpoint(&record["source"], names),
                record["relation"],
                endpoint(&record["target"], names)
            )
            .unwrap();
        }
        let fields: BTreeSet<_> = old
            .into_iter()
            .flat_map(|r| r.keys())
            .chain(new.into_iter().flat_map(|r| r.keys()))
            .collect();
        for field in fields {
            let left = old.and_then(|r| r.get(field));
            let right = new.and_then(|r| r.get(field));
            if left == right || field == "id" || field == "edge_id" {
                continue;
            }
            if field == "properties" {
                let keys: BTreeSet<_> = left
                    .and_then(Value::as_object)
                    .into_iter()
                    .flat_map(|r| r.keys())
                    .chain(
                        right
                            .and_then(Value::as_object)
                            .into_iter()
                            .flat_map(|r| r.keys()),
                    )
                    .collect();
                for key in keys {
                    let a = left.and_then(|r| r.get(key));
                    let b = right.and_then(|r| r.get(key));
                    if a != b {
                        change(output, &format!("properties.{}", field_name(key)), a, b);
                    }
                }
                if left.is_none() || right.is_none() {
                    change(output, field, left, right);
                }
            } else {
                change(output, &field_name(field), left, right);
            }
        }
    }
}

fn field_name(key: &str) -> String {
    if !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        key.to_owned()
    } else {
        serde_json::to_string(key).expect("string serialization")
    }
}

fn change(output: &mut String, field: &str, before: Option<&Value>, after: Option<&Value>) {
    let display = |value: Option<&Value>| {
        value
            .map(Value::to_string)
            .unwrap_or_else(|| "<absent>".into())
    };
    writeln!(
        output,
        "    {field}: {} -> {}",
        display(before),
        display(after)
    )
    .unwrap();
}

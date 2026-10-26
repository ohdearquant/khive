use std::path::Path;

use anyhow::{Context, Result};

use super::{edge_source_id, edge_target_id, SortFix};

pub(super) fn apply_fixes(repo: &Path) -> Result<Vec<SortFix>> {
    let kg_dir = repo.join(".khive/kg");
    let mut fixes = Vec::new();
    for (file, order) in [
        ("entities.ndjson", SortOrder::Field("id")),
        ("edges.ndjson", SortOrder::Edges),
    ] {
        let moved = match order {
            SortOrder::Field(key) => fix_sort_order(&kg_dir.join(file), key)?,
            SortOrder::Edges => sort_ndjson(&kg_dir.join(file), order)?,
        };
        if moved > 0 {
            fixes.push(SortFix { file, moved });
        }
    }
    Ok(fixes)
}

pub(super) fn fix_sort_order(path: &Path, sort_key: &str) -> Result<usize> {
    sort_ndjson(path, SortOrder::Field(sort_key))
}

#[derive(Clone, Copy)]
enum SortOrder<'a> {
    Field(&'a str),
    Edges,
}

/// Count nonblank record positions changed by the stable sort, not inversions.
/// Keep canonical serialization on rewrites and leave unchanged files byte-identical.
fn sort_ndjson(path: &Path, order: SortOrder<'_>) -> Result<usize> {
    if !path.exists() {
        return Ok(0);
    }
    let content =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut lines: Vec<(usize, serde_json::Value)> = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value = serde_json::from_str(line).with_context(|| {
            format!(
                "{} line {}: refusing to apply --fix over malformed JSON; run `kg validate` for details",
                path.display(),
                idx + 1
            )
        })?;
        lines.push((lines.len(), value));
    }
    lines.sort_by(|(_, a), (_, b)| match order {
        SortOrder::Field(key) => {
            let ak = a.get(key).and_then(|value| value.as_str()).unwrap_or("");
            let bk = b.get(key).and_then(|value| value.as_str()).unwrap_or("");
            ak.cmp(bk)
        }
        SortOrder::Edges => {
            let ak = (
                edge_source_id(a).unwrap_or(""),
                edge_target_id(a).unwrap_or(""),
                a.get("relation")
                    .and_then(|value| value.as_str())
                    .unwrap_or(""),
            );
            let bk = (
                edge_source_id(b).unwrap_or(""),
                edge_target_id(b).unwrap_or(""),
                b.get("relation")
                    .and_then(|value| value.as_str())
                    .unwrap_or(""),
            );
            ak.cmp(&bk)
        }
    });
    let moved = lines
        .iter()
        .enumerate()
        .filter(|(position, (original, _))| position != original)
        .count();
    if moved > 0 {
        let out = lines
            .iter()
            .map(|(_, value)| serde_json::to_string(value).expect("serialize JSON value"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(path, out + "\n").with_context(|| format!("write {}", path.display()))?;
    }
    Ok(moved)
}

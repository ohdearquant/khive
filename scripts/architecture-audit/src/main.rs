//! Read a fixed commit through finite Git read-only argv; never read source from the working tree.
mod production;
mod scanner;
mod sql_targets;
use production::{Blob, Snapshot};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
const TREE_LIMIT: usize = 32 * 1024 * 1024;
const BLOB_LIMIT: usize = 16 * 1024 * 1024;
const SNAPSHOT_LIMIT: usize = 256 * 1024 * 1024;
fn oid(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit())
}
fn git(repo: &PathBuf, args: &[&str], limit: usize) -> Result<Vec<u8>, String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn Git read: {e}"))?;
    let stderr = child.stderr.take().ok_or("missing Git stderr pipe")?;
    // Drain stderr while reading stdout. Retain only the finite diagnostic prefix.
    let stderr = std::thread::spawn(move || {
        let mut reader = stderr;
        let mut kept = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let available = 65536usize.saturating_sub(kept.len());
                    kept.extend_from_slice(&buffer[..n.min(available)]);
                }
                Err(_) => break,
            }
        }
        kept
    });
    let mut stdout = Vec::new();
    let result = child
        .stdout
        .take()
        .ok_or("missing Git stdout pipe")?
        .take((limit + 1) as u64)
        .read_to_end(&mut stdout);
    if result.is_err() || stdout.len() > limit {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|e| format!("wait Git read: {e}"))?;
    let error = stderr.join().map_err(|_| "Git diagnostic reader failed")?;
    result.map_err(|e| format!("read Git output: {e}"))?;
    if stdout.len() > limit {
        return Err(format!("Git read exceeds {limit}-byte bound"));
    }
    if !status.success() {
        return Err(format!(
            "Git {:?}: {}",
            args,
            String::from_utf8_lossy(&error).trim()
        ));
    }
    Ok(stdout)
}
fn snapshot(repo: &PathBuf, commit: &str) -> Result<Snapshot, String> {
    if !oid(commit) {
        return Err("--commit requires an exact full 40-hex commit OID".into());
    }
    let commit = commit.to_ascii_lowercase();
    let verified = String::from_utf8(git(
        repo,
        &["rev-parse", "--verify", &format!("{commit}^{{commit}}")],
        128,
    )?)
    .map_err(|_| "non-UTF8 Git commit response")?;
    if verified.trim() != commit {
        return Err("selected OID is not the exact commit object".into());
    }
    let tree = String::from_utf8(git(
        repo,
        &["rev-parse", "--verify", &format!("{commit}^{{tree}}")],
        128,
    )?)
    .map_err(|_| "non-UTF8 Git tree response")?
    .trim()
    .to_owned();
    if !oid(&tree) {
        return Err("invalid Git tree identity".into());
    }
    let inventory = git(
        repo,
        &["ls-tree", "-r", "-z", "--full-tree", &commit],
        TREE_LIMIT,
    )?;
    if !inventory.is_empty() && inventory.last() != Some(&0) {
        return Err("unterminated Git tree inventory".into());
    }
    let mut blobs = BTreeMap::new();
    let mut total = 0usize;
    for record in inventory.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let separator = record
            .iter()
            .position(|b| *b == b'\t')
            .ok_or("malformed Git tree record")?;
        let header = std::str::from_utf8(&record[..separator])
            .map_err(|_| "non-UTF8 Git tree header")?
            .split(' ')
            .collect::<Vec<_>>();
        let path =
            std::str::from_utf8(&record[separator + 1..]).map_err(|_| "non-UTF8 Git tree path")?;
        if path.starts_with('/')
            || path
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == "..")
            || path.contains('\\')
        {
            return Err(format!("invalid snapshot path {path:?}"));
        }
        if header.len() != 3 || !oid(header[2]) {
            return Err("invalid Git blob header".into());
        }
        let relevant =
            path.ends_with(".rs") || path.ends_with(".sql") || path.ends_with("Cargo.toml");
        if !relevant {
            continue;
        }
        if header[1] != "blob" || !matches!(header[0], "100644" | "100755") {
            return Err(format!(
                "source/asset is not a regular Git blob: {path} ({})",
                header[0]
            ));
        }
        let bytes = git(repo, &["cat-file", "blob", header[2]], BLOB_LIMIT)?;
        total = total
            .checked_add(bytes.len())
            .ok_or("snapshot size overflow")?;
        if total > SNAPSHOT_LIMIT {
            return Err("source snapshot exceeds 256 MiB bound".into());
        }
        let text = String::from_utf8(bytes).map_err(|_| format!("non-UTF8 source/asset {path}"))?;
        if blobs
            .insert(
                path.to_owned(),
                Blob {
                    mode: header[0].into(),
                    oid: header[2].into(),
                    text,
                },
            )
            .is_some()
        {
            return Err(format!("duplicate Git path {path}"));
        }
    }
    Ok(Snapshot {
        commit,
        tree,
        blobs,
    })
}
fn run() -> Result<(Value, bool), String> {
    let mut args = std::env::args().skip(1);
    let mut repo = None;
    let mut commit = None;
    let mut crates = Vec::new();
    let mut json_output = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--repo" if repo.is_none() => {
                repo = Some(PathBuf::from(args.next().ok_or("missing --repo value")?))
            }
            "--commit" if commit.is_none() => {
                commit = Some(args.next().ok_or("missing --commit value")?)
            }
            "--crate" => {
                let value = args.next().ok_or("missing --crate value")?;
                if value.is_empty()
                    || !value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                {
                    return Err("invalid crate directory identifier".into());
                }
                crates.push(value);
            }
            "--json" => json_output = true,
            "--help" => {
                return Ok((
                    json!({"help":"architecture-audit --repo PATH --commit FULL40OID [--crate NAME ...] [--json]"}),
                    true,
                ));
            }
            _ => return Err(format!("unknown or duplicate argument {arg}")),
        }
    }
    let snapshot = snapshot(
        &repo.ok_or("--repo is required")?,
        &commit.ok_or("--commit is required")?,
    )?;
    Ok((production::analyze(&snapshot, &crates), json_output))
}
fn main() {
    let (report, json_output) = match run() {
        Ok(result) => result,
        Err(error) => (
            json!({"complete":false,"diagnostics":[error],"crates":[]}),
            true,
        ),
    };
    let complete = report["complete"]
        .as_bool()
        .unwrap_or(report.get("help").is_some());
    let mut output = std::io::BufWriter::new(std::io::stdout().lock());
    let written = if json_output {
        serde_json::to_writer_pretty(&mut output, &report)
            .map_err(std::io::Error::other)
            .and_then(|()| writeln!(output))
    } else {
        (|| {
            writeln!(
                output,
                "commit={} tree={} complete={} source-union",
                report["audited_commit"], report["audited_tree"], complete
            )?;
            for row in report["crates"].as_array().into_iter().flatten() {
                writeln!(output, "{} production_sql={} = assets_read={} + inline={} declared={} readers={} test_only={} unread={} test_read_unknown={} complete={}", row["crate"], row["production_sql"], row["asset_production"], row["inline_sql_occurrences"], row["asset_total"], row["reader_site_count"], row["test_only_asset_count"], row["unread_asset_count"], row["test_read_unknown_asset_count"], row["complete"])?;
            }
            // Text mode retains the same complete, sorted evidence rows as JSON mode.
            serde_json::to_writer_pretty(&mut output, &report).map_err(std::io::Error::other)?;
            writeln!(output)
        })()
    };
    if written.and_then(|()| output.flush()).is_err() {
        std::process::exit(2);
    }
    if !complete {
        std::process::exit(2);
    }
}

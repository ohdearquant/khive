//! `kkernel db import-notes`: restore exported note rows into a database file with their original ids, namespaces,
//! timestamps, salience, decay, keys and properties.
//!
//! Input is JSONL, one note per line, in the shape the `list` verb returns under the verbose presentation: full
//! UUIDs and RFC 3339 timestamps (integer microseconds are accepted too). Each row goes through
//! `NoteStore::insert_note_if_absent`, so a row whose id already exists is left exactly as it is and counted, and a
//! re-run after an interruption writes only the rows still missing.
//!
//! No verb runs here: no id is minted, no actor is stamped, no message is delivered, no hook fires. Derived search
//! data is not written; run `kkernel reindex` against the destination afterwards. The destination must already
//! carry the current schema (`kkernel db migrate --db <path>` on a fresh file), so the daemon that later opens it
//! finds its migrations applied.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use khive_storage::note::Note;
use serde_json::Value;
use uuid::Uuid;

#[derive(clap::Parser, Debug)]
pub struct ImportNotesArgs {
    /// Destination database file. It must exist; this command never creates one.
    #[arg(long)]
    pub db: PathBuf,

    /// After writing, read every input row back by id and compare the stored fields with the input.
    /// Any difference is printed and the command exits nonzero.
    #[arg(long)]
    pub verify: bool,

    /// JSONL files to import, one note per line.
    #[arg(required = true)]
    pub files: Vec<PathBuf>,
}

#[derive(Default, Debug)]
struct Tally {
    rows: u64,
    inserted: u64,
    present: u64,
    verified: u64,
    mismatched: u64,
}

fn micros(row: &Value, field: &str) -> Result<Option<i64>> {
    match row.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n
            .as_i64()
            .map(Some)
            .ok_or_else(|| anyhow!("{field}: {n} is not an integer")),
        Some(Value::String(s)) => chrono::DateTime::parse_from_rfc3339(s)
            .map(|t| Some(t.timestamp_micros()))
            .with_context(|| format!("{field}: {s:?} is not RFC 3339")),
        Some(other) => bail!("{field}: unexpected value {other}"),
    }
}

fn text<'a>(row: &'a Value, field: &str) -> Result<&'a str> {
    row.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{field} is missing or not a string"))
}

fn optional_text(row: &Value, field: &str) -> Option<String> {
    row.get(field).and_then(Value::as_str).map(str::to_owned)
}

/// Build the storage row from one exported line. `version` is not carried: storage assigns it on insert.
fn note_from_row(row: &Value) -> Result<Note> {
    let id = Uuid::parse_str(text(row, "id")?).context("id is not a full UUID")?;
    let created_at = micros(row, "created_at")?.ok_or_else(|| anyhow!("created_at is missing"))?;
    let updated_at = micros(row, "updated_at")?.ok_or_else(|| anyhow!("updated_at is missing"))?;
    Ok(Note {
        id,
        namespace: text(row, "namespace")?.to_owned(),
        kind: text(row, "kind")?.to_owned(),
        status: text(row, "status")?.to_owned(),
        name: optional_text(row, "name"),
        content: text(row, "content")?.to_owned(),
        salience: row.get("salience").and_then(Value::as_f64),
        decay_factor: row.get("decay_factor").and_then(Value::as_f64),
        expires_at: micros(row, "expires_at")?,
        properties: row.get("properties").filter(|p| !p.is_null()).cloned(),
        created_at,
        updated_at,
        deleted_at: micros(row, "deleted_at")?,
        key: optional_text(row, "key"),
        version: 1,
    })
}

/// The fields a restored row must carry unchanged. Version is excluded (storage assigns it).
fn differences(want: &Note, got: &Note) -> Vec<&'static str> {
    let mut out = Vec::new();
    if want.namespace != got.namespace {
        out.push("namespace");
    }
    if want.kind != got.kind {
        out.push("kind");
    }
    if want.status != got.status {
        out.push("status");
    }
    if want.name != got.name {
        out.push("name");
    }
    if want.content != got.content {
        out.push("content");
    }
    if want.salience != got.salience {
        out.push("salience");
    }
    if want.decay_factor != got.decay_factor {
        out.push("decay_factor");
    }
    if want.expires_at != got.expires_at {
        out.push("expires_at");
    }
    if want.properties != got.properties {
        out.push("properties");
    }
    if want.created_at != got.created_at {
        out.push("created_at");
    }
    if want.updated_at != got.updated_at {
        out.push("updated_at");
    }
    if want.deleted_at != got.deleted_at {
        out.push("deleted_at");
    }
    if want.key != got.key {
        out.push("key");
    }
    out
}

pub async fn run(args: ImportNotesArgs) -> Result<()> {
    if !args.db.exists() {
        bail!(
            "{} does not exist; create it with `kkernel db migrate --db <path>` first",
            args.db.display()
        );
    }
    let backend = khive_db::StorageBackend::sqlite(&args.db)
        .with_context(|| format!("open {}", args.db.display()))?;
    let notes = backend
        .notes()
        .map_err(|e| anyhow!("open the note store: {e}"))?;

    let mut total = Tally::default();
    for file in &args.files {
        let mut tally = Tally::default();
        let reader = BufReader::new(
            std::fs::File::open(file).with_context(|| format!("open {}", file.display()))?,
        );
        let mut parsed = Vec::new();
        for (n, line) in reader.lines().enumerate() {
            let line = line.with_context(|| format!("{}:{}", file.display(), n + 1))?;
            if line.trim().is_empty() {
                continue;
            }
            let row: Value = serde_json::from_str(&line)
                .with_context(|| format!("{}:{} is not JSON", file.display(), n + 1))?;
            let note =
                note_from_row(&row).with_context(|| format!("{}:{}", file.display(), n + 1))?;
            parsed.push(note);
        }
        for note in &parsed {
            tally.rows += 1;
            if notes
                .insert_note_if_absent(note.clone())
                .await
                .map_err(|e| anyhow!("{}: insert {}: {e}", file.display(), note.id))?
            {
                tally.inserted += 1;
            } else {
                tally.present += 1;
            }
        }
        if args.verify {
            for want in &parsed {
                let got = notes
                    .get_note_including_deleted(want.id)
                    .await
                    .map_err(|e| anyhow!("{}: read back {}: {e}", file.display(), want.id))?;
                match got {
                    None => {
                        tally.mismatched += 1;
                        eprintln!("MISSING {} {}", file.display(), want.id);
                    }
                    Some(got) => {
                        let diff = differences(want, &got);
                        if diff.is_empty() {
                            tally.verified += 1;
                        } else {
                            tally.mismatched += 1;
                            eprintln!("DIFF {} {} {}", file.display(), want.id, diff.join(","));
                        }
                    }
                }
            }
        }
        println!(
            "{}\trows={}\tinserted={}\talready_present={}\tverified={}\tmismatched={}",
            file.display(),
            tally.rows,
            tally.inserted,
            tally.present,
            tally.verified,
            tally.mismatched
        );
        total.rows += tally.rows;
        total.inserted += tally.inserted;
        total.present += tally.present;
        total.verified += tally.verified;
        total.mismatched += tally.mismatched;
    }
    println!(
        "TOTAL\trows={}\tinserted={}\talready_present={}\tverified={}\tmismatched={}",
        total.rows, total.inserted, total.present, total.verified, total.mismatched
    );
    if args.verify && total.mismatched > 0 {
        bail!(
            "{} rows differ from the input after import",
            total.mismatched
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row() -> Value {
        json!({
            "id": "87a2baf7-5e7e-4a2b-9b06-53dfc11e4136",
            "namespace": "lambda:khive",
            "kind": "memory",
            "status": "active",
            "name": null,
            "content": "checkpoint",
            "salience": 0.55,
            "decay_factor": 0.02,
            "expires_at": null,
            "deleted_at": null,
            "created_at": "2026-06-15T22:13:59.737849Z",
            "updated_at": "2026-06-16T01:00:00.000001Z",
            "properties": {"memory_type": "episodic", "tags": ["a"]},
            "display_name": "checkpoint",
            "version": 7
        })
    }

    #[test]
    fn a_row_keeps_its_id_times_and_fields() {
        let note = note_from_row(&row()).unwrap();
        assert_eq!(note.id.to_string(), "87a2baf7-5e7e-4a2b-9b06-53dfc11e4136");
        assert_eq!(note.created_at, 1_781_561_639_737_849);
        assert_eq!(note.updated_at, 1_781_571_600_000_001);
        assert_eq!(note.salience, Some(0.55));
        assert_eq!(note.decay_factor, Some(0.02));
        assert_eq!(
            note.properties,
            Some(json!({"memory_type": "episodic", "tags": ["a"]}))
        );
        assert_eq!(note.key, None);
        assert_eq!(
            note.version, 1,
            "storage assigns the version, the export's is not carried"
        );
    }

    #[test]
    fn integer_microseconds_are_accepted() {
        let mut r = row();
        r["created_at"] = json!(1_781_561_639_737_849_i64);
        assert_eq!(note_from_row(&r).unwrap().created_at, 1_781_561_639_737_849);
    }

    #[test]
    fn a_row_without_created_at_or_with_a_short_id_is_refused() {
        let mut r = row();
        r.as_object_mut().unwrap().remove("created_at");
        assert!(note_from_row(&r).is_err());
        let mut r = row();
        r["id"] = json!("87a2baf7");
        assert!(note_from_row(&r).is_err());
    }

    #[tokio::test]
    async fn import_round_trips_and_a_second_run_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("dest.db");
        std::fs::File::create(&db).unwrap();
        let file = dir.path().join("memory.jsonl");
        let mut keyed = row();
        keyed["id"] = json!("0b0c6a7e-1d2f-4a3b-8c4d-5e6f70819203");
        keyed["key"] = json!("k-1");
        std::fs::write(&file, format!("{}\n{}\n", row(), keyed)).unwrap();

        run(ImportNotesArgs {
            db: db.clone(),
            verify: true,
            files: vec![file.clone()],
        })
        .await
        .unwrap();
        let backend = khive_db::StorageBackend::sqlite(&db).unwrap();
        let notes = backend.notes().unwrap();
        let id = Uuid::parse_str("87a2baf7-5e7e-4a2b-9b06-53dfc11e4136").unwrap();
        let got = notes.get_note(id).await.unwrap().unwrap();
        assert_eq!(got.created_at, 1_781_561_639_737_849);
        assert_eq!(got.namespace, "lambda:khive");
        let keyed_id = Uuid::parse_str("0b0c6a7e-1d2f-4a3b-8c4d-5e6f70819203").unwrap();
        assert_eq!(
            notes
                .get_note(keyed_id)
                .await
                .unwrap()
                .unwrap()
                .key
                .as_deref(),
            Some("k-1")
        );
        drop(notes);
        drop(backend);

        // A second run finds both rows and leaves them as they are.
        run(ImportNotesArgs {
            db,
            verify: true,
            files: vec![file],
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn verify_reports_a_row_that_differs_from_the_input() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("dest.db");
        std::fs::File::create(&db).unwrap();
        let first = dir.path().join("a.jsonl");
        std::fs::write(&first, format!("{}\n", row())).unwrap();
        run(ImportNotesArgs {
            db: db.clone(),
            verify: false,
            files: vec![first],
        })
        .await
        .unwrap();
        // Same id, different content: insert leaves the stored row, so verify must see the difference.
        let mut changed = row();
        changed["content"] = json!("something else");
        let second = dir.path().join("b.jsonl");
        std::fs::write(&second, format!("{}\n", changed)).unwrap();
        let err = run(ImportNotesArgs {
            db,
            verify: true,
            files: vec![second],
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("1 rows differ"), "{err}");
    }

    #[tokio::test]
    async fn a_missing_destination_is_refused_and_not_created() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("absent.db");
        let file = dir.path().join("m.jsonl");
        std::fs::write(&file, format!("{}\n", row())).unwrap();
        assert!(run(ImportNotesArgs {
            db: db.clone(),
            verify: false,
            files: vec![file]
        })
        .await
        .is_err());
        assert!(!db.exists());
    }
}

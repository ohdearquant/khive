//! Real executable acceptance controls for explicit import and local VCS sync.
//! These tests use local NDJSON files, never a Git fixture or remote clone.
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

const KNOWN: &str = "00000000-0000-0000-0000-000000000001";
const UNKNOWN: &str = "00000000-0000-0000-0000-000000000002";
const DUPLICATE_KIND: &str = "00000000-0000-0000-0000-000000000003";
const TIME: &str = "2026-01-02T03:04:05Z";

fn command(tmp: &TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kkernel"));
    for (key, _) in std::env::vars_os() {
        if key
            .to_str()
            .is_some_and(|key| key.starts_with("KHIVE_") && key != "KHIVE_TEST_HARNESS")
        {
            command.env_remove(key);
        }
    }
    command
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("KHIVE_VOLUME_LOCK_DIR", tmp.path().join("volume-locks"))
        .env("KHIVE_NO_DAEMON", "1");
    command
}
fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
fn config(tmp: &TempDir, strict: bool) -> PathBuf {
    let path = tmp.path().join(if strict {
        "strict.toml"
    } else {
        "relaxed.toml"
    });
    std::fs::write(&path, format!("[schema]\nstrict = {strict}\n")).unwrap();
    path
}
fn entity(id: &str, kind: &str, name: &str) -> Value {
    json!({"id":id,"kind":kind,"entity_type":"kept-subtype","name":name,"description":"complete source text","properties":{"keep":[1,true,"text"]},"tags":["tag"],"created_at":TIME,"updated_at":TIME})
}
fn records() -> Vec<Value> {
    vec![
        entity(KNOWN, "concept", "Known"),
        entity(UNKNOWN, " Future型 ", "Unknown"),
        entity(DUPLICATE_KIND, " Future型 ", "UnknownAgain"),
    ]
}
fn archive(records: Vec<Value>, edges: Vec<Value>) -> Value {
    json!({"format":"khive-kg","version":"0.1","namespace":"local","exported_at":TIME,"entities":records,"edges":edges})
}
fn import(
    tmp: &TempDir,
    source: &Path,
    db: &Path,
    config: &Path,
    format: &str,
    log: &str,
) -> Output {
    command(tmp)
        .args(["--log", log, "kg", "import"])
        .arg(source)
        .arg("--db")
        .arg(db)
        .arg("--config")
        .arg(config)
        .args(["--format", format])
        .output()
        .unwrap()
}
fn export(tmp: &TempDir, db: &Path) -> Value {
    let output_path = tmp.path().join("export.json");
    let output = command(tmp)
        .args(["kg", "export"])
        .arg(&output_path)
        .arg("--db")
        .arg(db)
        .output()
        .unwrap();
    assert_success(&output);
    serde_json::from_slice(&std::fs::read(output_path).unwrap()).unwrap()
}
fn no_destination(db: &Path) {
    assert!(!db.exists(), "unexpected destination {}", db.display());
    for suffix in ["-wal", "-shm", ".sync.lock"] {
        let sibling = PathBuf::from(format!("{}{suffix}", db.display()));
        assert!(
            !sibling.exists(),
            "unexpected sidecar {}",
            sibling.display()
        );
    }
    assert!(
        !db.parent().unwrap().exists(),
        "preflight created target parents"
    );
}
fn seed(tmp: &TempDir, db: &Path, config: &Path) -> Vec<u8> {
    let source = tmp.path().join("seed.json");
    std::fs::write(
        &source,
        archive(vec![entity(KNOWN, "concept", "Seed")], vec![]).to_string(),
    )
    .unwrap();
    assert_success(&import(tmp, &source, db, config, "archive", "error"));
    std::fs::read(db).unwrap()
}
fn write_format(tmp: &TempDir, format: &str, records: &[Value]) -> PathBuf {
    let path = tmp.path().join(format!("input.{format}"));
    let input = match format {
        "archive" => archive(records.to_vec(), vec![]).to_string(),
        "json" => Value::Array(records.to_vec()).to_string(),
        "ndjson" => records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
        "csv" | "tsv" => {
            let sep = if format == "csv" { ',' } else { '\t' };
            let mut text = format!("id{sep}kind{sep}name{sep}entity_type{sep}description{sep}created_at{sep}updated_at\n");
            for record in records {
                let row = [
                    "id",
                    "kind",
                    "name",
                    "entity_type",
                    "description",
                    "created_at",
                    "updated_at",
                ]
                .map(|field| record[field].as_str().unwrap())
                .join(&sep.to_string());
                text.push_str(&row);
                text.push('\n');
            }
            text
        }
        _ => unreachable!(),
    };
    std::fs::write(&path, input).unwrap();
    path
}

#[test]
fn every_explicit_import_route_refuses_strict_and_preserves_relaxed_kinds_without_verbose() {
    for format in ["archive", "json", "ndjson", "csv", "tsv"] {
        for log in ["error", "off"] {
            let tmp = TempDir::new().unwrap();
            let strict = config(&tmp, true);
            let relaxed = config(&tmp, false);
            let input = records();
            let source = write_format(&tmp, format, &input);
            let db = tmp.path().join("fresh/target.db");
            let output = import(&tmp, &source, &db, &strict, format, log);
            assert!(!output.status.success(), "{format}: {}", stderr(&output));
            no_destination(&db);
            let output = import(&tmp, &source, &db, &relaxed, format, log);
            assert_success(&output);
            let summary: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(summary["entities_imported"], 3);
            assert_eq!(summary["edges_imported"], 0);
            assert_eq!(summary["edges_skipped"], 0);
            assert_eq!(
                stderr(&output)
                    .matches("preserving unknown entity kind during explicit import")
                    .count(),
                1,
                "{}",
                stderr(&output)
            );
            let exported = export(&tmp, &db);
            for expected in &input {
                let actual = exported["entities"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|record| record["id"] == expected["id"])
                    .unwrap();
                for field in [
                    "kind",
                    "name",
                    "entity_type",
                    "description",
                    "created_at",
                    "updated_at",
                ] {
                    assert_eq!(actual[field], expected[field], "{format}: {field}");
                }
                if !matches!(format, "csv" | "tsv") {
                    assert_eq!(actual["properties"], expected["properties"]);
                    assert_eq!(actual["tags"], expected["tags"]);
                }
            }
            let replay = tmp.path().join("replay.json");
            std::fs::write(&replay, exported.to_string()).unwrap();
            let before = std::fs::read(&db).unwrap();
            assert!(!import(&tmp, &replay, &db, &strict, "archive", log)
                .status
                .success());
            assert_eq!(std::fs::read(db).unwrap(), before);
        }
    }
}

#[test]
fn identical_snapshot_distinguishes_strict_relaxed_and_actual_local_sync() {
    for existing in [false, true] {
        for sync_strict in [false, true] {
            let tmp = TempDir::new().unwrap();
            let strict = config(&tmp, true);
            let relaxed = config(&tmp, false);
            let source = write_format(&tmp, "ndjson", &records());
            let bytes = std::fs::read(&source).unwrap();
            let repo = tmp.path().join("snapshot");
            std::fs::create_dir_all(repo.join(".khive/kg")).unwrap();
            std::fs::write(repo.join(".khive/kg/entities.ndjson"), &bytes).unwrap();
            std::fs::write(repo.join(".khive/kg/edges.ndjson"), "").unwrap();
            let strict_db = tmp.path().join("strict-target/db");
            let relaxed_db = tmp.path().join("relaxed-target/db");
            let sync_db = tmp.path().join("sync-target/db");
            let before = if existing {
                Some(seed(&tmp, &strict_db, &strict))
            } else {
                None
            };
            let output = import(&tmp, &source, &strict_db, &strict, "ndjson", "off");
            assert!(!output.status.success());
            if let Some(before) = before {
                assert_eq!(std::fs::read(&strict_db).unwrap(), before);
            } else {
                no_destination(&strict_db);
            }
            if existing {
                seed(&tmp, &relaxed_db, &strict);
                seed(&tmp, &sync_db, &strict);
            }
            let output = import(&tmp, &source, &relaxed_db, &relaxed, "ndjson", "off");
            assert_success(&output);
            assert_eq!(
                stderr(&output)
                    .matches("preserving unknown entity kind during explicit import")
                    .count(),
                1
            );
            let preserved = export(&tmp, &relaxed_db);
            let output = command(&tmp)
                .env("KHIVE_CONFIG", if sync_strict { &strict } else { &relaxed })
                .args(["--log", "off", "sync", "--repo"])
                .arg(&repo)
                .arg("--db")
                .arg(&sync_db)
                .output()
                .unwrap();
            assert_success(&output);
            assert_eq!(
                stderr(&output)
                    .matches("degrading unknown local snapshot entity kind")
                    .count(),
                1,
                "{}",
                stderr(&output)
            );
            let degraded = export(&tmp, &sync_db);
            assert_eq!(degraded["entities"].as_array().unwrap().len(), 3);
            for expected in records() {
                let find = |data: &Value| {
                    data["entities"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|record| record["id"] == expected["id"])
                        .unwrap()
                        .clone()
                };
                let explicit = find(&preserved);
                let local = find(&degraded);
                for field in ["id", "name", "description", "created_at", "updated_at"] {
                    assert_eq!(local[field], expected[field]);
                    assert_eq!(explicit[field], expected[field]);
                }
                assert_eq!(explicit["kind"], expected["kind"]);
                assert_eq!(explicit["entity_type"], expected["entity_type"]);
                assert_eq!(explicit["properties"], expected["properties"]);
                assert_eq!(explicit["tags"], expected["tags"]);
                if expected["kind"] == "concept" {
                    assert_eq!(local["kind"], "concept");
                    assert_eq!(local["entity_type"], expected["entity_type"]);
                    assert_eq!(local["properties"], expected["properties"]);
                    assert_eq!(local["tags"], expected["tags"]);
                } else {
                    assert_eq!(local["kind"], "concept");
                    assert!(local.get("entity_type").is_none());
                    assert_eq!(local["properties"]["khive:original_kind"], expected["kind"]);
                    assert_eq!(
                        local["properties"]["khive:original_entity_type"],
                        expected["entity_type"]
                    );
                    assert_eq!(local["properties"]["keep"], expected["properties"]["keep"]);
                    assert_eq!(local["tags"], json!(["tag", "khive:degraded_kind"]));
                }
            }
            assert_eq!(std::fs::read(source).unwrap(), bytes);
            assert_eq!(
                std::fs::read(repo.join(".khive/kg/entities.ndjson")).unwrap(),
                bytes
            );
        }
    }
}

#[test]
fn closed_relations_reservations_credentials_and_late_bad_records_never_touch_destination() {
    for strict_mode in [true, false] {
        for existing in [false, true] {
            for case in 0..10 {
                let tmp = TempDir::new().unwrap();
                let config = config(&tmp, strict_mode);
                let db = tmp.path().join("target/db");
                let before = if existing {
                    Some(seed(&tmp, &db, &config))
                } else {
                    None
                };
                let mut entities = vec![
                    entity(KNOWN, "concept", "A"),
                    entity(UNKNOWN, "concept", "B"),
                ];
                let mut edge = json!({"edge_id":DUPLICATE_KIND,"source":KNOWN,"target":UNKNOWN,"relation":"extends","weight":0.7,"created_at":TIME,"updated_at":TIME});
                match case {
                    0 => edge["relation"] = json!("future"),
                    1 => edge["weight"] = json!(1.1),
                    2 => edge["properties"] = json!({"khive:secret_gate":"forged"}),
                    3 => edge["properties"] = json!({"khive:web_receipt":"forged"}),
                    4 => edge["properties"] = json!({"api_key":"AKIAFAKEKEY1234567890"}),
                    5 => entities[1]["properties"] = json!({"khive:secret_gate":"forged"}),
                    6 => entities[1]["properties"] = json!({"khive:web_receipt":"forged"}),
                    7 => entities[1]["kind"] = json!(" "),
                    8 => entities[1]["name"] = json!(" "),
                    9 => edge["updated_at"] = json!("invalid"),
                    _ => unreachable!(),
                }
                let source = tmp.path().join("archive.json");
                std::fs::write(&source, archive(entities, vec![edge]).to_string()).unwrap();
                let output = import(&tmp, &source, &db, &config, "archive", "error");
                assert!(!output.status.success(), "case {case}");
                if let Some(before) = before {
                    assert_eq!(std::fs::read(db).unwrap(), before);
                } else {
                    no_destination(&db);
                }
            }
        }
        // Positive controls prove the same archive and adapter edges reach real writes.
        for format in ["archive", "json", "ndjson"] {
            let tmp = TempDir::new().unwrap();
            let config = config(&tmp, strict_mode);
            let db = tmp.path().join("db");
            let entities = vec![
                entity(KNOWN, "concept", "A"),
                entity(UNKNOWN, "concept", "B"),
            ];
            let edge = json!({"edge_id":DUPLICATE_KIND,"source":KNOWN,"target":UNKNOWN,"relation":"extends","weight":0.7,"properties":{"keep":true},"created_at":TIME,"updated_at":TIME});
            let content = if format == "archive" {
                archive(entities, vec![edge]).to_string()
            } else {
                let records = [entities, vec![edge]].concat();
                if format == "json" {
                    json!(records).to_string()
                } else {
                    records
                        .iter()
                        .map(Value::to_string)
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            };
            let source = tmp.path().join("valid");
            std::fs::write(&source, content).unwrap();
            let output = import(&tmp, &source, &db, &config, format, "error");
            assert_success(&output);
            let summary: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(summary["entities_imported"], 2);
            assert_eq!(summary["edges_imported"], 1);
            assert_eq!(
                export(&tmp, &db)["edges"][0]["properties"],
                json!({"keep":true})
            );
        }
    }
}

#[test]
fn actual_config_selection_defaults_warnings_and_gate_fail_before_target_setup() {
    for text in ["", "[schema]\n", "[schema]\nstrict=true\n"] {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join("selected.toml");
        std::fs::write(&cfg, text).unwrap();
        let source = write_format(&tmp, "json", &records());
        let db = tmp.path().join("fresh/db");
        assert!(!import(&tmp, &source, &db, &cfg, "json", "off")
            .status
            .success());
        no_destination(&db);
    }
    for text in ["[schema]\nstrict=\"false\"\n","[schema]\nstrict=false\n[gate]\n","[schema]\nstrict=false\n[actor]\nid=\"lambda:test\"\n[gate]\ngranted_actors=[\"lambda:test\"]\ndeny_writes_for=[\"lambda:test\"]\n"] {
        let tmp=TempDir::new().unwrap(); let cfg=tmp.path().join("selected.toml"); std::fs::write(&cfg,text).unwrap();
        let source=write_format(&tmp,"json",&records()); let db=tmp.path().join("fresh/db");
        let output=import(&tmp,&source,&db,&cfg,"json","off"); assert!(!output.status.success(),"{}",stderr(&output)); no_destination(&db);
    }
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("env-selected.toml");
    std::fs::write(&cfg, "[schema]\nstrict=false\nfuture=\"private-value\"\n").unwrap();
    let source = write_format(&tmp, "json", &records());
    let db = tmp.path().join("target/db");
    let output = command(&tmp)
        .env("KHIVE_CONFIG", &cfg)
        .args(["--log", "off", "kg", "import"])
        .arg(&source)
        .arg("--db")
        .arg(&db)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert_success(&output);
    let text = stderr(&output);
    assert_eq!(
        text.matches("ignoring unknown schema configuration key")
            .count(),
        1
    );
    assert!(!text.contains("private-value"));
    let missing = tmp.path().join("missing.toml");
    let fresh = tmp.path().join("missing-target/db");
    let output = import(&tmp, &source, &fresh, &missing, "json", "error");
    assert!(!output.status.success());
    no_destination(&fresh);
    std::fs::write(
        tmp.path().join("khive.toml"),
        "[schema]\nstrict=\"false\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(tmp.path().join(".khive")).unwrap();
    std::fs::write(
        tmp.path().join(".khive/config.toml"),
        "[schema]\nstrict=false\n",
    )
    .unwrap();
    let output = command(&tmp)
        .args(["kg", "import"])
        .arg(&source)
        .arg("--db")
        .arg(&fresh)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    no_destination(&fresh);
}

#[test]
fn declared_destination_read_only_conflicts_and_aliases_refuse_without_writes() {
    for case in 0..3 {
        let tmp = TempDir::new().unwrap();
        let source = write_format(&tmp, "json", &records());
        let db = tmp.path().join("fresh/db");
        let declared = if case == 1 {
            tmp.path().join("different/db")
        } else {
            db.clone()
        };
        let mut text = format!(
            "[schema]\nstrict=false\n[[backends]]\nname=\"main\"\npath={:?}\nread_only={}\n",
            declared.to_str().unwrap(),
            case == 0
        );
        if case == 2 {
            text.push_str(&format!(
                "[[backends]]\nname=\"alias\"\npath={:?}\nread_only=true\n",
                db.to_str().unwrap()
            ));
        }
        let cfg = tmp.path().join("selected.toml");
        std::fs::write(&cfg, text).unwrap();
        let output = import(&tmp, &source, &db, &cfg, "json", "error");
        assert!(!output.status.success(), "{}", stderr(&output));
        no_destination(&db);
        if case == 1 {
            no_destination(&declared);
        }
    }
}

#[test]
fn standalone_delimited_edges_keep_existing_cli_endpoint_refusal() {
    for strict_mode in [true, false] {
        for (format, sep) in [("csv", ','), ("tsv", '\t')] {
            let tmp = TempDir::new().unwrap();
            let cfg = config(&tmp, strict_mode);
            let db = tmp.path().join("fresh/db");
            let source = tmp.path().join("edges");
            std::fs::write(
                &source,
                format!("source{sep}target{sep}relation\n{KNOWN}{sep}{UNKNOWN}{sep}extends\n"),
            )
            .unwrap();
            assert!(!import(&tmp, &source, &db, &cfg, format, "error")
                .status
                .success());
            no_destination(&db);
        }
    }
}

#[test]
fn distinct_raw_unknown_kind_warning_counts_survive_quiet_global_filters() {
    for format in ["archive", "json", "ndjson", "csv", "tsv"] {
        let tmp = TempDir::new().unwrap();
        let cfg = config(&tmp, false);
        let input = vec![
            entity(KNOWN, "Future", "A"),
            entity(UNKNOWN, "Future", "B"),
            entity(DUPLICATE_KIND, "future", "C"),
            entity("00000000-0000-0000-0000-000000000004", "Other", "D"),
        ];
        let source = write_format(&tmp, format, &input);
        let db = tmp.path().join("db");
        let output = import(&tmp, &source, &db, &cfg, format, "off");
        assert_success(&output);
        assert_eq!(
            stderr(&output)
                .matches("preserving unknown entity kind during explicit import")
                .count(),
            3,
            "{}",
            stderr(&output)
        );
    }
}

#[test]
fn gate_write_denials_preserve_seeded_destination_and_fresh_parent() {
    for existing in [false, true] {
        let tmp = TempDir::new().unwrap();
        let strict = config(&tmp, true);
        let source = write_format(&tmp, "json", &records());
        let db = tmp.path().join("target/db");
        let before = if existing {
            Some(seed(&tmp, &db, &strict))
        } else {
            None
        };
        let cfg = tmp.path().join("denied.toml");
        std::fs::write(&cfg,"[schema]\nstrict=false\n[actor]\nid=\"lambda:import-test\"\n[gate]\ngranted_actors=[\"lambda:import-test\"]\ndeny_writes_for=[\"lambda:import-test\"]\n").unwrap();
        let output = import(&tmp, &source, &db, &cfg, "json", "off");
        assert!(!output.status.success(), "{}", stderr(&output));
        assert!(
            stderr(&output).contains("authorize kg import before destination setup"),
            "{}",
            stderr(&output)
        );
        if let Some(before) = before {
            assert_eq!(std::fs::read(db).unwrap(), before);
        } else {
            no_destination(&db);
        }
    }
}

#[test]
fn declared_fresh_main_import_succeeds_with_configured_writer_limits() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("fresh/nested/db");
    let source = write_format(&tmp, "json", &records());
    let cfg = tmp.path().join("main.toml");
    std::fs::write(&cfg,format!("[schema]\nstrict=false\n[[backends]]\nname=\"main\"\npath={:?}\nwal_ceiling_bytes=104857600\ndisk_reserve_bytes=0\ndisk_guard_deadline_ms=777\n",db.to_str().unwrap())).unwrap();
    assert_success(&import(&tmp, &source, &db, &cfg, "json", "off"));
    assert_eq!(export(&tmp, &db)["entities"].as_array().unwrap().len(), 3);
}

#[cfg(unix)]
#[test]
fn declared_chmod_guard_refuses_existing_main_before_writable_open() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    let strict = config(&tmp, true);
    let db = tmp.path().join("target/db");
    let before = seed(&tmp, &db, &strict);
    let source = write_format(&tmp, "json", &records());
    let cfg = tmp.path().join("main.toml");
    std::fs::write(
        &cfg,
        format!(
            "[schema]\nstrict=false\n[[backends]]\nname=\"main\"\npath={:?}\n",
            db.to_str().unwrap()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o444)).unwrap();
    let output = import(&tmp, &source, &db, &cfg, "json", "off");
    assert!(!output.status.success(), "{}", stderr(&output));
    assert_eq!(std::fs::read(&db).unwrap(), before);
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn actual_adapter_routes_keep_known_alias_and_pack_normalization_in_both_modes() {
    for strict_mode in [true, false] {
        for format in ["json", "ndjson", "csv", "tsv"] {
            let tmp = TempDir::new().unwrap();
            let cfg = config(&tmp, strict_mode);
            let db = tmp.path().join("db");
            let source = write_format(
                &tmp,
                format,
                &[
                    entity(KNOWN, " Paper ", "Alias"),
                    entity(UNKNOWN, " RESOURCE ", "Pack"),
                ],
            );
            let output = import(&tmp, &source, &db, &cfg, format, "off");
            assert_success(&output);
            assert_eq!(
                stderr(&output)
                    .matches("preserving unknown entity kind during explicit import")
                    .count(),
                0
            );
            let actual = export(&tmp, &db);
            let kinds: Vec<_> = actual["entities"]
                .as_array()
                .unwrap()
                .iter()
                .map(|record| record["kind"].as_str().unwrap())
                .collect();
            assert!(kinds.contains(&"document"));
            assert!(kinds.contains(&"resource"));
        }
    }
}

#[test]
fn actual_json_and_ndjson_adapter_relations_stay_closed_before_destination_setup() {
    for strict_mode in [true, false] {
        for format in ["json", "ndjson"] {
            for relation in ["extends", "future"] {
                let tmp = TempDir::new().unwrap();
                let cfg = config(&tmp, strict_mode);
                let db = tmp.path().join("fresh/db");
                let input = vec![
                    entity(KNOWN, "concept", "A"),
                    entity(UNKNOWN, "concept", "B"),
                    json!({"edge_id":DUPLICATE_KIND,"source":KNOWN,"target":UNKNOWN,"relation":relation,"weight":0.7}),
                ];
                let source = write_format(&tmp, format, &input);
                let output = import(&tmp, &source, &db, &cfg, format, "off");
                if relation == "extends" {
                    assert_success(&output);
                    assert_eq!(export(&tmp, &db)["edges"].as_array().unwrap().len(), 1);
                } else {
                    assert!(!output.status.success());
                    no_destination(&db);
                }
            }
        }
    }
}

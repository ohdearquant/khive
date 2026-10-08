use std::io::Write;
use std::sync::Arc;

use khive_runtime::{
    AllowAllGate, BackendId, KhiveRuntime, Namespace, RuntimeConfig, RuntimeError,
};
use khive_storage::types::{SqlStatement, SqlValue};
use tempfile::{NamedTempFile, TempDir};

use super::*;
use crate::vocab::SESSION_SCHEMA_PLAN_STMTS;

#[cfg(unix)]
#[test]
fn source_open_refuses_a_symlink() {
    let dir = TempDir::new().expect("tempdir");
    let outside = NamedTempFile::new().expect("outside file");
    let link = dir.path().join("linked.jsonl");
    std::os::unix::fs::symlink(outside.path(), &link).expect("symlink");
    assert!(open_source_file(&link).is_err());
}

#[cfg(unix)]
#[test]
fn scheduled_file_rejects_replaced_parent_symlink_at_probe_and_open() {
    let dir = TempDir::new().expect("tempdir");
    let fixture = std::fs::canonicalize(dir.path()).expect("fixture directory");
    let root = fixture.join("root");
    let parent = root.join("staged");
    let outside = fixture.join("outside");
    std::fs::create_dir_all(&parent).expect("inside parent");
    std::fs::create_dir_all(&outside).expect("outside parent");
    let source = parent.join("source.jsonl");
    std::fs::write(&source, b"inside\n").expect("inside source");
    let outside_source = outside.join("source.jsonl");
    std::fs::write(&outside_source, b"outside\n").expect("outside source");

    let (original_probe, directory_identities) =
        open_source_file_beneath(&root, &source, None).expect("initial probe");
    assert!(original_probe
        .metadata()
        .expect("inside metadata")
        .is_file());
    std::fs::rename(&parent, root.join("staged-old")).expect("move inside parent");
    std::os::unix::fs::symlink(&outside, &parent).expect("replace parent with symlink");

    assert!(open_source_file_beneath(&root, &source, None).is_err());
    let outside_identity = file_identity(&std::fs::File::open(&outside_source).expect("outside"))
        .expect("outside identity");
    assert!(read_bounded_chunk(
        &source,
        0,
        LineTailSource::ClaudeCode,
        None,
        MirrorLimits::production(),
        Some(&outside_identity),
        Some(TrustedSource {
            root: &root,
            directory_identities: &directory_identities,
        }),
    )
    .is_err());
}

#[cfg(unix)]
#[test]
fn source_root_opens_beneath_a_root_owned_system_ancestor_symlink() {
    use std::os::unix::fs::MetadataExt;

    let mut fixture = None;
    for parent in [PathBuf::from("/tmp"), std::env::temp_dir()] {
        let mut has_root_owned_link = false;
        let mut all_links_root_owned = true;
        for ancestor in parent.ancestors() {
            let Ok(metadata) = std::fs::symlink_metadata(ancestor) else {
                all_links_root_owned = false;
                break;
            };
            if metadata.file_type().is_symlink() {
                has_root_owned_link = true;
                all_links_root_owned &= metadata.uid() == 0;
            }
        }
        if !has_root_owned_link || !all_links_root_owned {
            continue;
        }
        if let Ok(temp) = TempDir::new_in(&parent) {
            assert!(
                temp.path().starts_with(&parent),
                "fixture retains the system ancestor spelling"
            );
            fixture = Some(temp);
            break;
        }
    }
    let Some(temp) = fixture else {
        eprintln!("QUALIFIED SKIP: no writable temporary base beneath a root-owned system ancestor symlink; run on macOS /tmp or /var/folders");
        return;
    };
    let root = temp.path().join("root");
    std::fs::create_dir(&root).expect("configured root");
    let source = root.join("source.jsonl");
    std::fs::write(&source, b"inside\n").expect("source fixture");
    let physical_source = std::fs::canonicalize(&source).expect("physical source fixture");
    let expected_identity =
        file_identity(&std::fs::File::open(&physical_source).expect("physical source"))
            .expect("physical source identity");
    let physical_root = std::fs::canonicalize(&root).expect("physical root fixture");
    let expected_root_identity =
        file_identity(&std::fs::File::open(&physical_root).expect("physical root"))
            .expect("physical root identity");

    let (opened, directories) =
        open_source_file_beneath(&root, &source, None).expect("root-owned ancestor admission");
    assert_eq!(
        file_identity(&opened).expect("opened identity"),
        expected_identity
    );
    assert_eq!(directories, vec![expected_root_identity]);
    let (reopened, _) = open_source_file_beneath(&root, &source, Some(&directories))
        .expect("checked source reopen through system ancestor");
    assert_eq!(
        file_identity(&reopened).expect("reopened identity"),
        expected_identity
    );
    assert_eq!(
        std::fs::read(&physical_source).expect("unchanged source"),
        b"inside\n"
    );
}

#[cfg(unix)]
#[test]
fn source_root_refuses_an_ancestor_symlink_in_a_writable_parent() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().expect("fixture outside source tree");
    let fixture = std::fs::canonicalize(temp.path()).expect("physical fixture anchor");
    let protected = fixture.join("protected");
    let root = protected.join("exports");
    std::fs::create_dir_all(&root).expect("protected fixture root");
    let source = root.join("source.jsonl");
    std::fs::write(&source, b"protected\n").expect("protected source");
    let before = std::fs::metadata(&source).expect("protected metadata before");
    let (original, original_directories) =
        open_source_file_beneath(&root, &source, None).expect("ordinary root admission");
    let original_identity = file_identity(&original).expect("protected source identity");
    let ancestor = fixture.join("linked");
    std::os::unix::fs::symlink(&protected, &ancestor).expect("fixture ancestor link");
    // A parent writable by group and others without the sticky bit is refused whoever owns
    // the link.
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o777))
        .expect("unsafe link parent");
    let linked_root = ancestor.join("exports");
    let linked_source = linked_root.join("source.jsonl");
    let Err(error) = open_source_file_beneath(&linked_root, &linked_source, None) else {
        panic!("an ancestor link in an unsafe parent must refuse before admitting a root");
    };
    let refusal: &khive_fs::directory_walk::AncestorLinkRefusal = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref())
        .expect("shared ancestor policy refusal");
    assert_eq!(
        refusal.condition,
        khive_fs::directory_walk::AncestorLinkCondition::ParentPermissions
    );
    let (reopened, directories) =
        open_source_file_beneath(&root, &source, None).expect("ordinary root remains usable");
    assert_eq!(
        file_identity(&reopened).expect("reopened identity"),
        original_identity
    );
    assert_eq!(directories, original_directories);
    let after = std::fs::metadata(&source).expect("protected metadata after");
    assert_eq!(
        std::fs::read(&source).expect("protected bytes after"),
        b"protected\n"
    );
    assert_eq!(after.len(), before.len());
    assert_eq!(
        after.modified().expect("mtime after"),
        before.modified().expect("mtime before")
    );
}

#[cfg(unix)]
#[test]
fn source_root_refuses_its_leaf_symlink() {
    let temp = TempDir::new().expect("fixture outside source tree");
    let fixture = std::fs::canonicalize(temp.path()).expect("physical fixture anchor");
    let root = fixture.join("root");
    std::fs::create_dir(&root).expect("ordinary source root");
    let source = root.join("source.jsonl");
    std::fs::write(&source, b"inside\n").expect("source fixture");
    let linked_root = fixture.join("linked-root");
    std::os::unix::fs::symlink(&root, &linked_root).expect("configured-root leaf link");
    assert!(
        open_source_file_beneath(&linked_root, &linked_root.join("source.jsonl"), None).is_err()
    );
    let (opened, _) = open_source_file_beneath(&root, &source, None).expect("ordinary root");
    assert_eq!(
        file_identity(&opened).expect("opened identity"),
        file_identity(&std::fs::File::open(&source).expect("source handle"))
            .expect("source identity")
    );
    assert_eq!(
        std::fs::read(&source).expect("unchanged source"),
        b"inside\n"
    );
}

#[cfg(unix)]
#[test]
fn source_root_keeps_absolute_relative_empty_and_parent_directory_semantics() {
    let cwd = std::fs::canonicalize(std::env::current_dir().expect("current directory"))
        .expect("physical current directory");
    let dir = TempDir::new().expect("fixture outside source tree");
    let absolute_fixture = std::fs::canonicalize(dir.path()).expect("fixture directory");
    let absolute_root = absolute_fixture.join("root");
    std::fs::create_dir(&absolute_root).expect("source root");
    let source = absolute_root.join("source.jsonl");
    std::fs::write(&source, b"inside\n").expect("source fixture");
    let identity = file_identity(&std::fs::File::open(&source).expect("source handle"))
        .expect("source identity");
    let mut cwd_components = cwd.components().peekable();
    let mut fixture_components = absolute_fixture.components().peekable();
    while cwd_components.peek().is_some() && cwd_components.peek() == fixture_components.peek() {
        cwd_components.next();
        fixture_components.next();
    }
    let mut relative_fixture = PathBuf::new();
    for _ in cwd_components {
        relative_fixture.push("..");
    }
    for component in fixture_components {
        relative_fixture.push(component.as_os_str());
    }
    let relative_root = relative_fixture.join("root");

    for root in [
        absolute_root.clone(),
        relative_root.clone(),
        relative_root.join("..").join("root"),
    ] {
        let path = root.join("source.jsonl");
        let (file, directories) =
            open_source_file_beneath(&root, &path, None).expect("configured root probe");
        assert_eq!(file_identity(&file).expect("opened identity"), identity);
        assert_eq!(directories.len(), 1, "root witness shape remains unchanged");
        let (file, _) = open_source_file_beneath(&root, &path, Some(&directories))
            .expect("checked source reopen");
        assert_eq!(file_identity(&file).expect("reopened identity"), identity);
    }

    let cwd_identity = file_identity(&std::fs::File::open(&cwd).expect("current directory"))
        .expect("current-directory identity");
    for root in [Path::new(""), Path::new(".")] {
        let directories = open_source_root(root).expect("current-directory root probe");
        assert_eq!(directories.len(), 1);
        assert_eq!(
            file_identity(&directories[0]).expect("root identity"),
            cwd_identity
        );
    }

    let filesystem_root = open_source_root(Path::new("/")).expect("filesystem root");
    assert_eq!(filesystem_root.len(), 1);
    assert!(filesystem_root[0]
        .metadata()
        .expect("root metadata")
        .is_dir());
}

#[cfg(windows)]
#[test]
fn mirror_windows_file_identity_changes_when_renamed_replacement_takes_the_path() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("transcript.jsonl");
    std::fs::write(&path, b"original\n").expect("original file");
    let original =
        file_identity(&open_source_file(&path).expect("open original")).expect("original identity");
    assert!(
        original.starts_with("windows:"),
        "unexpected identity spelling: {original}"
    );

    let mut appender = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open for append");
    appender.write_all(b"appended\n").expect("append");
    drop(appender);
    assert_eq!(
        file_identity(&open_source_file(&path).expect("open appended")).expect("appended identity"),
        original,
        "an append keeps the file identity"
    );

    // Save-by-rename: write a sibling temp file, then rename it over the
    // original name. The replacement may inherit the original's creation
    // time, so only the handle's file id can tell the two files apart.
    let staged = dir.path().join("transcript.jsonl.tmp");
    std::fs::write(&staged, b"replacement\n").expect("staged file");
    std::fs::rename(&staged, &path).expect("rename over original");
    let replaced = file_identity(&open_source_file(&path).expect("open replacement"))
        .expect("replacement identity");
    assert_ne!(
        replaced, original,
        "a same-path replacement must not reuse the original identity"
    );
}

#[cfg(windows)]
#[test]
fn mirror_windows_scheduled_file_rejects_parent_and_file_reparse_points() {
    use std::os::windows::fs::{symlink_dir, symlink_file};

    let dir = TempDir::new().expect("tempdir");
    let root = dir.path().join("root");
    let parent = root.join("staged");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&parent).expect("inside parent");
    std::fs::create_dir_all(&outside).expect("outside parent");
    let source = parent.join("source.jsonl");
    std::fs::write(&source, b"inside\n").expect("inside source");
    let outside_source = outside.join("source.jsonl");
    std::fs::write(&outside_source, b"outside\n").expect("outside source");

    let (_, directory_identities) =
        open_source_file_beneath(&root, &source, None).expect("initial probe");
    std::fs::rename(&parent, root.join("staged-old")).expect("move inside parent");
    symlink_dir(&outside, &parent).expect("create parent link");
    assert!(open_source_file_beneath(&root, &source, None).is_err());
    assert!(open_source_file_beneath(&root, &source, Some(&directory_identities)).is_err());

    std::fs::remove_dir(&parent).expect("remove parent link");
    std::fs::create_dir(&parent).expect("restore parent");
    symlink_file(&outside_source, &source).expect("link source");
    assert!(open_source_file_beneath(&root, &source, None).is_err());
    assert!(open_source_file(&source).is_err());

    let linked_root_parent = dir.path().join("linked-root-parent");
    symlink_dir(&root, &linked_root_parent).expect("link root parent");
    let nested_root = linked_root_parent.join("staged-old");
    let nested_source = nested_root.join("source.jsonl");
    assert!(open_source_file_beneath(&nested_root, &nested_source, None).is_err());
}

/// Build a file-backed runtime (exercises the real `atomic_unit`
/// single-writer path) and apply the session schema. Caller must keep
/// the returned `TempDir` alive.
async fn setup() -> (KhiveRuntime, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("test.db");
    let rt = KhiveRuntime::new(RuntimeConfig {
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        events_split: None,
        db_path: Some(db_path),
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("file-backed runtime");
    apply_session_schema(&rt).await;
    (rt, dir)
}

async fn apply_session_schema(rt: &KhiveRuntime) {
    let sql = rt.sql();
    let mut w = sql.writer().await.expect("writer");
    for stmt in &SESSION_SCHEMA_PLAN_STMTS {
        w.execute_script(stmt.to_string())
            .await
            .expect("schema stmt");
    }
    // w dropped here — releases the writer connection.
}

/// Count rows in a table.
async fn count_rows(rt: &KhiveRuntime, table: &str) -> i64 {
    let sql = rt.sql();
    let mut r = sql.reader().await.expect("reader");
    let row = r
        .query_row(SqlStatement {
            sql: format!("SELECT COUNT(*) FROM {table}"),
            params: vec![],
            label: None,
        })
        .await
        .expect("count query")
        .expect("count row");
    match row.columns.first().map(|c| &c.value) {
        Some(SqlValue::Integer(n)) => *n,
        _ => 0,
    }
}

/// Retrieve the stored byte_offset for a file path.
async fn cursor_offset(rt: &KhiveRuntime, path_str: &str) -> Option<i64> {
    let sql = rt.sql();
    let mut r = sql.reader().await.expect("reader");
    let row = r
        .query_row(SqlStatement {
            sql: "SELECT byte_offset FROM session_mirror_cursor WHERE file_path=?1".into(),
            params: vec![SqlValue::Text(path_str.to_string())],
            label: None,
        })
        .await
        .expect("cursor query")?;
    match row.columns.first().map(|c| &c.value) {
        Some(SqlValue::Integer(n)) => Some(*n),
        _ => None,
    }
}

fn user_line(uuid: &str, session_id: &str, text: &str) -> String {
    format!(
        r#"{{"uuid":"{uuid}","sessionId":"{session_id}","type":"user","timestamp":"2026-06-29T10:00:00Z","message":{{"role":"user","content":"{text}"}}}}"#
    )
}

/// A user line with NO `timestamp` field — `created_at` falls back to `now_us`.
fn user_line_no_ts(uuid: &str, session_id: &str, text: &str) -> String {
    format!(
        r#"{{"uuid":"{uuid}","sessionId":"{session_id}","type":"user","message":{{"role":"user","content":"{text}"}}}}"#
    )
}

/// Retrieve the stored `last_seen_at` for a session id.
async fn last_seen_at(rt: &KhiveRuntime, session_id: &str) -> Option<i64> {
    let sql = rt.sql();
    let mut r = sql.reader().await.expect("reader");
    let row = r
        .query_row(SqlStatement {
            sql: "SELECT last_seen_at FROM sessions WHERE id=?1".into(),
            params: vec![SqlValue::Text(session_id.to_string())],
            label: None,
        })
        .await
        .expect("last_seen query")?;
    match row.columns.first().map(|c| &c.value) {
        Some(SqlValue::Integer(n)) => Some(*n),
        _ => None,
    }
}

#[tokio::test]
async fn test_mirror_three_lines_and_idempotency() {
    let (rt, _dir) = setup().await;

    // Build a fixture JSONL with 3 lines, all ending in '\n'.
    let line1 = user_line("uuid-1", "sess-A", "Hello");
    let line2 = user_line("uuid-2", "sess-A", "World");
    let line3 = user_line("uuid-3", "sess-A", "Done");

    let mut file = NamedTempFile::new().expect("tmpfile");
    writeln!(file, "{line1}").unwrap();
    writeln!(file, "{line2}").unwrap();
    writeln!(file, "{line3}").unwrap();

    let path = file.path().to_path_buf();

    // First call: should insert all 3 rows.
    let stats = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
        .await
        .expect("mirror_file first call");
    assert_eq!(stats.inserted, 3, "should insert 3 new messages");
    assert_eq!(stats.scanned, 3, "should scan 3 lines");
    assert!(stats.new_offset > 0, "offset should advance");

    let msg_count = count_rows(&rt, "session_messages").await;
    assert_eq!(msg_count, 3, "3 messages in DB");

    let session_count = count_rows(&rt, "sessions").await;
    assert_eq!(session_count, 1, "1 session row");

    // Idempotency: second call over the SAME range inserts 0 rows.
    let stats2 = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
        .await
        .expect("mirror_file second call");
    assert_eq!(stats2.inserted, 0, "second pass must insert 0 rows");
    assert_eq!(count_rows(&rt, "session_messages").await, 3);

    // Offset-aware: calling from the advanced offset finds nothing new.
    let stats3 = mirror_file(
        &rt,
        &path,
        stats.new_offset,
        LineTailSource::ClaudeCode,
        None,
    )
    .await
    .expect("mirror_file from new_offset");
    assert_eq!(stats3.inserted, 0, "no new data past advanced offset");
    assert_eq!(stats3.new_offset, stats.new_offset);

    // Cursor was recorded.
    let stored_offset = cursor_offset(&rt, &path.to_string_lossy()).await;
    assert!(stored_offset.is_some(), "cursor should be recorded");
    assert_eq!(stored_offset.unwrap(), stats.new_offset as i64);
}

#[tokio::test]
async fn mirror_file_respects_low_test_cap_and_advances_over_multiple_passes() {
    // PACKSESSION-AUD-003 regression: multi-pass bounded reads (see docs guide).
    let (rt, _dir) = setup().await;

    let lines: Vec<String> = (0..6)
        .map(|i| user_line(&format!("uuid-cap-{i}"), "sess-CAP", &format!("line{i}")))
        .collect();

    let mut file = NamedTempFile::new().expect("tmpfile");
    for line in &lines {
        writeln!(file, "{line}").unwrap();
    }
    let path = file.path().to_path_buf();
    let file_len = std::fs::metadata(&path).unwrap().len();

    // All 6 fixture lines are byte-identical in length, so capping at
    // exactly two lines' worth of bytes forces a 2-line-per-pass split
    // without needing a giant fixture.
    let cap_bytes = (lines[0].len() + 1) + (lines[1].len() + 1);
    let limits = MirrorLimits {
        max_bytes_per_pass: cap_bytes,
        max_events_per_pass: 1024,
        max_line_bytes: MIRROR_MAX_LINE_BYTES,
    };

    let stats1 = mirror_file_with_limits(&rt, &path, 0, LineTailSource::ClaudeCode, None, limits)
        .await
        .expect("first bounded pass");
    assert_eq!(
        stats1.inserted, 2,
        "first pass must stop at the byte cap, not read the whole file"
    );
    assert_eq!(stats1.scanned, 2);
    assert!(
        stats1.new_offset < file_len,
        "new_offset {new} must be less than file_len {file_len} for a bounded pass",
        new = stats1.new_offset
    );
    assert_eq!(
        cursor_offset(&rt, &path.to_string_lossy()).await,
        Some(stats1.new_offset as i64),
        "cursor must be committed after the first bounded pass"
    );

    let stats2 = mirror_file_with_limits(
        &rt,
        &path,
        stats1.new_offset,
        LineTailSource::ClaudeCode,
        None,
        limits,
    )
    .await
    .expect("second bounded pass");
    assert_eq!(stats2.inserted, 2);
    assert!(stats2.new_offset > stats1.new_offset);
    assert!(stats2.new_offset < file_len);

    let stats3 = mirror_file_with_limits(
        &rt,
        &path,
        stats2.new_offset,
        LineTailSource::ClaudeCode,
        None,
        limits,
    )
    .await
    .expect("third bounded pass");
    assert_eq!(stats3.inserted, 2);
    assert_eq!(stats3.new_offset, file_len, "final pass must reach EOF");

    // All 6 rows landed across 3 bounded passes, and the cursor reflects
    // the full file — no pass allocated or inserted the entire file at
    // once.
    assert_eq!(count_rows(&rt, "session_messages").await, 6);
    assert_eq!(
        cursor_offset(&rt, &path.to_string_lossy()).await,
        Some(file_len as i64)
    );

    // A pass with no remaining bytes is a clean no-op.
    let stats4 = mirror_file_with_limits(
        &rt,
        &path,
        stats3.new_offset,
        LineTailSource::ClaudeCode,
        None,
        limits,
    )
    .await
    .expect("fourth pass at EOF");
    assert_eq!(stats4.inserted, 0);
    assert_eq!(stats4.scanned, 0);
}

#[tokio::test]
async fn test_oversized_line_is_skipped_and_offset_advances() {
    // PACKSESSION-AUD-003 regression: oversized complete line (see docs guide).
    let (rt, _dir) = setup().await;

    let small1 = user_line("uuid-small1", "sess-OV", "ok");
    let huge_text = "x".repeat(2000);
    let huge = user_line("uuid-huge", "sess-OV", &huge_text);
    let small2 = user_line("uuid-small2", "sess-OV", "after");

    let mut file = NamedTempFile::new().expect("tmpfile");
    writeln!(file, "{small1}").unwrap();
    writeln!(file, "{huge}").unwrap();
    writeln!(file, "{small2}").unwrap();
    let path = file.path().to_path_buf();
    let file_len = std::fs::metadata(&path).unwrap().len();

    let max_line_bytes: usize = 256;
    assert!(
        huge.len() + 1 > max_line_bytes,
        "fixture huge line must exceed the cap"
    );
    assert!(
        small1.len() + 1 < max_line_bytes && small2.len() + 1 < max_line_bytes,
        "fixture small lines must fit under the cap"
    );

    let limits = MirrorLimits {
        max_bytes_per_pass: MIRROR_MAX_BYTES_PER_PASS,
        max_events_per_pass: MIRROR_MAX_EVENTS_PER_PASS,
        max_line_bytes,
    };

    let stats = mirror_file_with_limits(&rt, &path, 0, LineTailSource::ClaudeCode, None, limits)
        .await
        .expect("mirror with a small line cap");

    assert_eq!(stats.inserted, 2, "only the two small lines are inserted");
    assert_eq!(
        stats.scanned, 2,
        "the oversized line must not count toward scanned"
    );
    assert_eq!(
        stats.new_offset, file_len,
        "offset must advance past the oversized line, not wedge on it"
    );
    assert_eq!(count_rows(&rt, "session_messages").await, 2);
}

#[tokio::test]
async fn test_line_just_under_cap_then_oversized_next_line_is_bounded() {
    // PACKSESSION-AUD-003 regression: under-cap line followed by an
    // oversized one (see docs guide).
    let (rt, _dir) = setup().await;

    let max_line_bytes: usize = 256;
    let shell_len = user_line("uuid-a", "sess-BND", "").len() + 1; // + '\n'
    let pad = max_line_bytes.saturating_sub(shell_len).saturating_sub(4);
    let text_a = "y".repeat(pad);
    let line_a = user_line("uuid-a", "sess-BND", &text_a);

    let huge_text = "z".repeat(max_line_bytes * 4);
    let line_b = user_line("uuid-b", "sess-BND", &huge_text);

    let mut file = NamedTempFile::new().expect("tmpfile");
    writeln!(file, "{line_a}").unwrap();
    writeln!(file, "{line_b}").unwrap();
    let path = file.path().to_path_buf();
    let file_len = std::fs::metadata(&path).unwrap().len();

    assert!(
        line_a.len() + 1 < max_line_bytes,
        "fixture line A must land just under the cap"
    );
    assert!(
        line_b.len() + 1 > max_line_bytes,
        "fixture line B must land over the cap"
    );

    let limits = MirrorLimits {
        max_bytes_per_pass: MIRROR_MAX_BYTES_PER_PASS,
        max_events_per_pass: MIRROR_MAX_EVENTS_PER_PASS,
        max_line_bytes,
    };

    let stats = mirror_file_with_limits(&rt, &path, 0, LineTailSource::ClaudeCode, None, limits)
        .await
        .expect("mirror with a boundary line cap");

    assert_eq!(stats.inserted, 1, "only the under-cap line is inserted");
    assert_eq!(
        stats.scanned, 1,
        "the oversized line must not count toward scanned"
    );
    assert_eq!(
        stats.new_offset, file_len,
        "offset must advance past both lines, including the skipped oversized one"
    );
    assert_eq!(count_rows(&rt, "session_messages").await, 1);
}

#[tokio::test]
async fn test_complete_oversized_line_spanning_multiple_windows_makes_progress() {
    let (rt, _dir) = setup().await;

    let max_line_bytes: usize = 256;
    let before = user_line("uuid-before-long", "sess-LONG", "before");
    let oversized_target_bytes = max_line_bytes * 256;
    let oversized_shell_bytes = user_line("uuid-long", "sess-LONG", "").len();
    let oversized = user_line(
        "uuid-long",
        "sess-LONG",
        &"z".repeat(oversized_target_bytes - oversized_shell_bytes),
    );
    let after = user_line("uuid-after-long", "sess-LONG", "after");

    let mut file = NamedTempFile::new().expect("tmpfile");
    writeln!(file, "{before}").unwrap();
    writeln!(file, "{oversized}").unwrap();
    writeln!(file, "{after}").unwrap();
    let path = file.path().to_path_buf();
    let file_len = std::fs::metadata(&path).unwrap().len();

    assert_eq!(
        oversized.len(),
        oversized_target_bytes,
        "fixture line must be an exact max_line_bytes multiple"
    );
    assert!(
        oversized.len() + 1 > max_line_bytes + 3 * 8 * 1024,
        "fixture must extend several reader windows beyond the line cap"
    );

    let limits = MirrorLimits {
        max_bytes_per_pass: MIRROR_MAX_BYTES_PER_PASS,
        max_events_per_pass: MIRROR_MAX_EVENTS_PER_PASS,
        max_line_bytes,
    };
    let mut offset = 0;
    let max_passes = oversized_target_bytes / max_line_bytes + 4;

    for pass in 1..=max_passes {
        if offset == file_len {
            break;
        }
        let stats =
            mirror_file_with_limits(&rt, &path, offset, LineTailSource::ClaudeCode, None, limits)
                .await
                .expect("bounded pass over complete oversized line");
        assert!(
            stats.new_offset > offset,
            "pass {pass} must advance beyond offset {offset}"
        );
        offset = stats.new_offset;
    }

    assert_eq!(offset, file_len, "bounded passes must eventually reach EOF");
    assert_eq!(
        count_rows(&rt, "session_messages").await,
        2,
        "valid records before and after the oversized line must land"
    );
    assert_eq!(
        cursor_offset(&rt, &path.to_string_lossy()).await,
        Some(file_len as i64),
        "the persisted cursor must clear the complete oversized line"
    );
}

/// Counts every byte pulled through `Read::read`, for asserting a hard
/// ceiling on `read_line_bounded`'s reads independent of buffer size.
struct CountingReader<R> {
    inner: R,
    total_read: std::rc::Rc<std::cell::Cell<usize>>,
}

impl<R: std::io::Read> std::io::Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.total_read.set(self.total_read.get() + n);
        Ok(n)
    }
}

#[test]
fn test_read_line_bounded_oversized_unterminated_reads_are_capped_per_call() {
    // PACKSESSION-AUD-003 regression: oversized-unterminated reads are
    // capped per call, not just per buffered byte (see docs guide).
    let max_line_bytes: usize = 64;
    let buf_capacity: usize = 256;
    // Far larger than max_line_bytes + a handful of buffer refills, and
    // containing NO '\n' anywhere — the pathological unterminated case.
    let data = vec![b'x'; 200_000];

    let total_read = std::rc::Rc::new(std::cell::Cell::new(0));
    let counting = CountingReader {
        inner: std::io::Cursor::new(data),
        total_read: total_read.clone(),
    };
    let mut reader = std::io::BufReader::with_capacity(buf_capacity, counting);
    let mut buf = Vec::new();

    let outcome = read_line_bounded(&mut reader, &mut buf, max_line_bytes, false)
        .expect("read must not error");

    match outcome {
        LineRead::OversizedUnterminated { bytes } => {
            assert!(
                bytes > max_line_bytes,
                "must have detected the crossing of the cap, got {bytes}"
            );
        }
        other => panic!("expected OversizedUnterminated, got {other:?}"),
    }
    assert!(
        buf.is_empty(),
        "buf must never buffer anything once the line is flagged oversized"
    );

    // The load-bearing assertion: total bytes ever pulled from the
    // underlying 200,000-byte source must be bounded to roughly
    // max_line_bytes plus a small, constant number of buffer refills —
    // never anywhere close to scanning the whole remaining file.
    let read_bytes = total_read.get();
    assert!(
        read_bytes <= max_line_bytes + buf_capacity * 4,
        "read_line_bounded pulled {read_bytes} bytes from the source for an \
             unterminated oversized line — expected at most {} (bounded to the \
             cap plus a few buffer refills), not an unbounded scan toward EOF",
        max_line_bytes + buf_capacity * 4
    );
}

#[tokio::test]
async fn test_oversized_unterminated_line_persists_skip_progress_and_resumes_after_terminator() {
    // PACKSESSION-AUD-003 regression: persist bounded progress through
    // an oversized line, including across a simulated restart.
    let (rt, _dir) = setup().await;

    let max_line_bytes: usize = 256;
    // One line, far larger than the cap AND larger than the reader's
    // internal buffer window, with no terminating '\n' at all. Staying
    // within one buffer window would let the first bounded read drain
    // the whole fixture in a single pass, landing exactly at EOF and
    // never exercising a genuine mid-line resume on the next pass.
    let huge_unterminated = "z".repeat(max_line_bytes + 4 * 8 * 1024);

    let mut file = NamedTempFile::new().expect("tmpfile");
    file.write_all(huge_unterminated.as_bytes())
        .expect("write unterminated line");
    let path = file.path().to_path_buf();
    let initial_file_len = std::fs::metadata(&path).unwrap().len();
    assert!(
        huge_unterminated.len() > max_line_bytes + 3 * 8 * 1024,
        "fixture must extend several reader windows beyond the line cap so the \
             first bounded read stops mid-line rather than draining the whole file"
    );

    let limits = MirrorLimits {
        max_bytes_per_pass: MIRROR_MAX_BYTES_PER_PASS,
        max_events_per_pass: MIRROR_MAX_EVENTS_PER_PASS,
        max_line_bytes,
    };

    // Once the line is known to be oversized, a bounded discarded prefix
    // is durably checkpointed instead of being replayed on every poll.
    let stats1 = mirror_file_with_limits(&rt, &path, 0, LineTailSource::ClaudeCode, None, limits)
        .await
        .expect("first pass over an unterminated oversized line");
    assert!(
        stats1.new_offset > 0 && stats1.new_offset < initial_file_len,
        "the first bounded skip must land strictly inside the oversized line \
             (not at EOF), so the next pass exercises a genuine mid-line resume \
             through the persisted-cursor path rather than a fresh read starting \
             after a coincidental full drain"
    );
    assert_eq!(stats1.scanned, 0);
    assert_eq!(stats1.inserted, 0);
    assert_eq!(
        count_rows(&rt, "session_messages").await,
        0,
        "no partial/garbage row may be written for an unterminated oversized line"
    );
    assert_eq!(
        cursor_offset(&rt, &path.to_string_lossy()).await,
        Some(stats1.new_offset as i64),
        "mid-skip progress must survive a daemon restart"
    );
    assert_ne!(
        huge_unterminated.as_bytes()[stats1.new_offset as usize - 1],
        b'\n',
        "the persisted cursor must sit strictly inside the oversized line, not \
             just after a line terminator, to exercise the mid-line resume path"
    );

    // Drain every full bounded prefix, then a simulated daemon restart at
    // the stable tail is a no-op. A final under-cap fragment may remain
    // uncheckpointed until its newline arrives.
    let mut offset = stats1.new_offset;
    for _ in 0..32 {
        let stats =
            mirror_file_with_limits(&rt, &path, offset, LineTailSource::ClaudeCode, None, limits)
                .await
                .expect("bounded continuation over the same unterminated line");
        assert_eq!(stats.scanned, 0);
        assert_eq!(stats.inserted, 0);
        if stats.new_offset == offset {
            break;
        }
        assert!(stats.new_offset > offset);
        offset = stats.new_offset;
    }
    assert!(offset <= initial_file_len);

    let stats2 =
        mirror_file_with_limits(&rt, &path, offset, LineTailSource::ClaudeCode, None, limits)
            .await
            .expect("simulated daemon restart at the stable unterminated tail");
    assert_eq!(stats2.new_offset, offset);
    assert_eq!(stats2.scanned, 0);
    assert_eq!(stats2.inserted, 0);
    assert_eq!(count_rows(&rt, "session_messages").await, 0);

    // Now the line completes (append a terminating '\n' and a bit more,
    // simulating the file finishing its write): it must be recognized
    // as the ordinary complete-oversized-line skip, advance past it, and
    // ingest anything that follows normally.
    let small_after = user_line("uuid-after-huge", "sess-UNTERM", "after");
    {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("reopen for append");
        writeln!(f).unwrap(); // terminate the huge line
        writeln!(f, "{small_after}").unwrap();
    }
    let file_len = std::fs::metadata(&path).unwrap().len();

    let stats3 = mirror_file_with_limits(
        &rt,
        &path,
        stats2.new_offset,
        LineTailSource::ClaudeCode,
        None,
        limits,
    )
    .await
    .expect("third pass once the huge line terminates");
    assert_eq!(
        stats3.new_offset, file_len,
        "once terminated, the skip-and-advance path must clear past the whole \
             oversized line plus the following valid line"
    );
    assert_eq!(stats3.scanned, 1, "only the small trailing line is scanned");
    assert_eq!(stats3.inserted, 1);
    assert_eq!(count_rows(&rt, "session_messages").await, 1);
}

#[tokio::test]
async fn test_still_growing_partial_line_under_cap_is_unaffected() {
    // Guard: still-growing partial line under the cap must not regress
    // from the oversized-unterminated handling (see docs guide).
    let (rt, _dir) = setup().await;

    let small1 = user_line("uuid-g1", "sess-GROW", "first");
    let mut file = NamedTempFile::new().expect("tmpfile");
    writeln!(file, "{small1}").unwrap();
    // Partial trailing line: valid JSON-shaped prefix, no newline yet.
    let partial_prefix = user_line("uuid-g2", "sess-GROW", "second");
    file.write_all(partial_prefix.as_bytes())
        .expect("write partial line, no trailing newline");
    let path = file.path().to_path_buf();

    let limits = MirrorLimits::production();

    let stats1 = mirror_file_with_limits(&rt, &path, 0, LineTailSource::ClaudeCode, None, limits)
        .await
        .expect("first pass: complete line + partial trailing line");
    assert_eq!(stats1.scanned, 1, "only the complete first line is scanned");
    assert_eq!(stats1.inserted, 1);
    assert_eq!(
        stats1.new_offset,
        (small1.len() + 1) as u64,
        "cursor must stop right after the first complete line, not consume the partial tail"
    );

    // The file "grows": the trailing line now gets its newline.
    writeln!(file).unwrap();
    let file_len = std::fs::metadata(&path).unwrap().len();

    let stats2 = mirror_file_with_limits(
        &rt,
        &path,
        stats1.new_offset,
        LineTailSource::ClaudeCode,
        None,
        limits,
    )
    .await
    .expect("second pass: the previously-partial line now completes");
    assert_eq!(stats2.new_offset, file_len);
    assert_eq!(stats2.scanned, 1);
    assert_eq!(stats2.inserted, 1);
    assert_eq!(count_rows(&rt, "session_messages").await, 2);
}

#[tokio::test]
async fn test_large_run_of_blank_lines_is_bounded_and_persists_cursor() {
    // PACKSESSION-AUD-003 regression: blank-line runs are bounded and the
    // cursor persists on every durable advance (see docs guide).
    let (rt, _dir) = setup().await;

    let mut file = NamedTempFile::new().expect("tmpfile");
    for _ in 0..500 {
        writeln!(file).unwrap(); // blank line: just "\n"
    }
    let path = file.path().to_path_buf();
    let file_len = std::fs::metadata(&path).unwrap().len();
    assert_eq!(file_len, 500, "500 one-byte blank lines");

    // A tiny per-pass byte cap forces the blank-line run across multiple
    // passes instead of reading straight to EOF in one call.
    let limits = MirrorLimits {
        max_bytes_per_pass: 50,
        max_events_per_pass: MIRROR_MAX_EVENTS_PER_PASS,
        max_line_bytes: MIRROR_MAX_LINE_BYTES,
    };

    let stats1 = mirror_file_with_limits(&rt, &path, 0, LineTailSource::ClaudeCode, None, limits)
        .await
        .expect("first blank-line pass");

    assert_eq!(stats1.inserted, 0);
    assert_eq!(stats1.scanned, 0, "blank lines never count toward scanned");
    assert!(
        stats1.new_offset > 0,
        "the pass cap must trip after at least one blank line, not read unbounded"
    );
    assert!(
        stats1.new_offset < file_len,
        "a bounded pass over an all-blank file must not reach EOF in one call"
    );

    // The cursor must be durably persisted even though `scanned == 0`.
    let stored_offset = cursor_offset(&rt, &path.to_string_lossy()).await;
    assert_eq!(
        stored_offset,
        Some(stats1.new_offset as i64),
        "cursor must be persisted even when the pass scanned zero events"
    );

    // Repeated calls continue from the persisted offset (not from 0) and
    // eventually reach EOF, never re-reading already-consumed blanks.
    let mut offset = stats1.new_offset;
    loop {
        let stats =
            mirror_file_with_limits(&rt, &path, offset, LineTailSource::ClaudeCode, None, limits)
                .await
                .expect("subsequent blank-line pass");
        assert_eq!(stats.inserted, 0);
        if stats.new_offset == offset {
            break; // EOF reached, no further progress
        }
        offset = stats.new_offset;
    }
    assert_eq!(
        offset, file_len,
        "all blank lines eventually consumed to EOF"
    );
}

/// Regression for the multi-candidate atomicity finding: the deferred
/// dispatch variant must NOT commit the cursor on an empty advance —
/// the commit is the dispatch loop's final step, vetoable until then.
#[tokio::test]
async fn deferred_empty_advance_leaves_cursor_uncommitted_until_commit_empty_advance() {
    let (rt, _dir) = setup().await;

    let mut file = NamedTempFile::new().expect("tmpfile");
    for _ in 0..16 {
        writeln!(file).unwrap(); // blank line: just "\n"
    }
    let path = file.path().to_path_buf();
    let file_len = std::fs::metadata(&path).unwrap().len();

    let stats = mirror_file_deferred(&rt, &path, 0, LineTailSource::ClaudeCode, None)
        .await
        .expect("deferred blank-line pass");
    assert_eq!(stats.inserted, 0);
    assert_eq!(
        stats.new_offset, file_len,
        "bytes were consumed off the file"
    );
    assert_eq!(
        cursor_offset(&rt, &path.to_string_lossy()).await,
        None,
        "the deferred pass must not commit the cursor itself"
    );

    // Dispatch ends without an inserting candidate: the loop commits.
    commit_empty_advance(&rt, &path, &stats)
        .await
        .expect("end-of-dispatch commit");
    assert_eq!(
        cursor_offset(&rt, &path.to_string_lossy()).await,
        Some(file_len as i64),
        "the commit lands exactly where the deferred pass consumed"
    );
}

/// A deferred pass that DOES parse events commits rows and cursor in one
/// transaction, exactly like the immediate variant.
#[tokio::test]
async fn deferred_pass_with_events_commits_rows_and_cursor_together() {
    let (rt, _dir) = setup().await;

    let mut file = NamedTempFile::new().expect("tmpfile");
    let session_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    for index in 0..3 {
        writeln!(
            file,
            "{}",
            user_line(&format!("u{index}"), session_id, "hello")
        )
        .unwrap();
    }
    let path = file.path().to_path_buf();
    let file_len = std::fs::metadata(&path).unwrap().len();

    let stats = mirror_file_deferred(&rt, &path, 0, LineTailSource::ClaudeCode, None)
        .await
        .expect("deferred event pass");
    assert_eq!(stats.inserted, 3);
    assert_eq!(stats.new_offset, file_len);
    assert_eq!(
        cursor_offset(&rt, &path.to_string_lossy()).await,
        Some(file_len as i64),
        "an inserting pass commits its cursor inside the same atomic unit"
    );
    assert_eq!(count_rows(&rt, "session_messages").await, 3);
}

#[tokio::test]
async fn test_partial_trailing_line_not_consumed() {
    let (rt, _dir) = setup().await;

    let line1 = user_line("uuid-p1", "sess-B", "Complete");
    // Write one complete line + a partial line without trailing '\n'.
    let partial = r#"{"uuid":"uuid-p2","sessionId":"sess-B","type":"user""#;

    let mut file = NamedTempFile::new().expect("tmpfile");
    writeln!(file, "{line1}").unwrap(); // complete line (has \n)
    write!(file, "{partial}").unwrap(); // partial — NO trailing \n

    let path = file.path().to_path_buf();
    let full_len = std::fs::metadata(&path).unwrap().len();

    let stats = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
        .await
        .expect("mirror_file partial");

    // Only the complete line should be consumed.
    assert_eq!(stats.inserted, 1, "only 1 complete line inserted");
    assert!(
        stats.new_offset < full_len,
        "new_offset {new} must be less than file_len {full_len}",
        new = stats.new_offset
    );

    // The partial bytes remain; calling again from new_offset finds no complete lines.
    let stats2 = mirror_file(
        &rt,
        &path,
        stats.new_offset,
        LineTailSource::ClaudeCode,
        None,
    )
    .await
    .expect("second call");
    assert_eq!(
        stats2.inserted, 0,
        "partial line must not be consumed on re-poll"
    );
    assert_eq!(
        stats2.new_offset, stats.new_offset,
        "offset must not advance on partial-only content"
    );
}

#[tokio::test]
async fn test_duplicate_uuid_across_two_calls() {
    let (rt, _dir) = setup().await;

    let line = user_line("uuid-dup", "sess-C", "First");

    let mut file = NamedTempFile::new().expect("tmpfile");
    writeln!(file, "{line}").unwrap();

    let path = file.path().to_path_buf();

    // First call inserts.
    let s1 = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
        .await
        .unwrap();
    assert_eq!(s1.inserted, 1);

    // Append same uuid again.
    writeln!(file, "{line}").unwrap();

    // Second call from offset 0 should see both lines but insert 0 new rows.
    let s2 = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
        .await
        .unwrap();
    assert_eq!(s2.inserted, 0, "duplicate uuid must not be re-inserted");
    assert_eq!(count_rows(&rt, "session_messages").await, 1);

    // Incremental: call from first call's new_offset; the second line is the dup.
    let s3 = mirror_file(&rt, &path, s1.new_offset, LineTailSource::ClaudeCode, None)
        .await
        .unwrap();
    assert_eq!(s3.inserted, 0, "incremental dup must also insert 0");
}

#[tokio::test]
async fn test_replay_does_not_mutate_session_metadata() {
    // Replay-idempotency regression (see docs guide): a pure replay must
    // not advance last_seen_at.
    let (rt, _dir) = setup().await;

    let line = user_line_no_ts("uuid-nts", "sess-NTS", "no timestamp here");
    let mut file = NamedTempFile::new().expect("tmpfile");
    writeln!(file, "{line}").unwrap();
    let path = file.path().to_path_buf();

    let s1 = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
        .await
        .unwrap();
    assert_eq!(s1.inserted, 1);
    let seen_after_first = last_seen_at(&rt, "sess-NTS")
        .await
        .expect("session row exists");

    // Replay from offset 0: re-scans the same line, inserts 0, and must
    // leave last_seen_at byte-identical even though now_us has advanced.
    let s2 = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
        .await
        .unwrap();
    assert_eq!(s2.inserted, 0, "replay must insert 0 rows");
    let seen_after_replay = last_seen_at(&rt, "sess-NTS").await.unwrap();
    assert_eq!(
        seen_after_first, seen_after_replay,
        "replay must not advance last_seen_at for a timestamp-missing event"
    );
}

#[tokio::test]
async fn test_empty_file_is_a_no_op() {
    let (rt, _dir) = setup().await;

    let file = NamedTempFile::new().expect("tmpfile");
    let path = file.path().to_path_buf();

    let stats = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
        .await
        .unwrap();
    assert_eq!(stats.inserted, 0);
    assert_eq!(stats.scanned, 0);
    assert_eq!(stats.new_offset, 0);
}

#[tokio::test]
async fn test_missing_file_returns_error() {
    let (rt, _dir) = setup().await;
    let bad_path = std::path::PathBuf::from("/nonexistent/path/session.jsonl");
    let result = mirror_file(&rt, &bad_path, 0, LineTailSource::ClaudeCode, None).await;
    assert!(
        matches!(result, Err(RuntimeError::Internal(_))),
        "missing file should return Internal error"
    );
}

// ── Codex source integration tests ────────────────────────────────────────

/// Build a minimal Codex response_item/message line (`input_text` for
/// user, `output_text` for assistant — the generic `text` type does not
/// occur in real Codex transcripts).
fn codex_message_line(role: &str, text: &str) -> String {
    let block_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    format!(
        r#"{{"type":"response_item","timestamp":"2026-06-30T08:00:00Z","payload":{{"type":"message","role":"{role}","content":[{{"type":"{block_type}","text":"{text}"}}]}}}}"#
    )
}

/// Build a minimal Codex session_meta line.
fn codex_meta_line(session_id: &str, cwd: &str, branch: &str) -> String {
    format!(
        r#"{{"type":"session_meta","timestamp":"2026-06-30T08:00:00Z","payload":{{"id":"{session_id}","cwd":"{cwd}","git":{{"branch":"{branch}","commit_hash":"abc","repository_url":"https://github.com/example/repo"}}}}}}"#
    )
}

/// Build a Codex event_msg line (should be skipped).
fn codex_event_msg_line() -> String {
    r#"{"type":"event_msg","timestamp":"2026-06-30T08:00:00Z","payload":{"type":"user_message","content":"should be skipped"}}"#.to_string()
}

#[tokio::test]
async fn test_codex_mirror_inserts_with_source_codex() {
    let (rt, _dir) = setup().await;

    let session_id = "cdx-sess-0001-0001-0001-000000000001";
    let meta = codex_meta_line(session_id, "/home/lion/proj", "feat-x");
    let user_msg = codex_message_line("user", "Hello from Codex");
    let asst_msg = codex_message_line("assistant", "Hello back from Codex");
    let skip_msg = codex_event_msg_line();

    let mut file = NamedTempFile::new().expect("tmpfile");
    writeln!(file, "{meta}").unwrap();
    writeln!(file, "{user_msg}").unwrap();
    writeln!(file, "{asst_msg}").unwrap();
    writeln!(file, "{skip_msg}").unwrap();

    let path = file.path().to_path_buf();

    // Mirror the file as Codex source.
    let stats = mirror_file(&rt, &path, 0, LineTailSource::Codex, Some(session_id))
        .await
        .expect("codex mirror_file");

    // session_meta + 2 response_item/message rows = 3 parseable, event_msg skipped.
    assert_eq!(stats.inserted, 3, "meta + 2 messages inserted");
    assert_eq!(
        stats.scanned, 4,
        "4 lines total (including skipped event_msg)"
    );
    assert!(stats.new_offset > 0);

    // Session row exists with source='codex'.
    let sql = rt.sql();
    let mut r = sql.reader().await.expect("reader");
    let session_row = r
        .query_row(SqlStatement {
            sql: "SELECT source FROM sessions WHERE id=?1".into(),
            params: vec![SqlValue::Text(session_id.to_string())],
            label: None,
        })
        .await
        .expect("query ok")
        .expect("session row must exist");
    match session_row.columns.first().map(|c| &c.value) {
        Some(SqlValue::Text(s)) => assert_eq!(s, "codex", "source must be 'codex'"),
        other => panic!("unexpected source value: {other:?}"),
    }

    // All 3 message rows are stored.
    assert_eq!(count_rows(&rt, "session_messages").await, 3);

    // The two response_item/message rows carry their real input_text/
    // output_text content through to session_messages.text — not just a
    // row count, but the actual extracted string for each role.
    let mut r2 = sql.reader().await.expect("reader");
    let rows = r2
        .query_all(SqlStatement {
            sql: "SELECT role, text FROM session_messages \
                      WHERE session_id=?1 AND role IS NOT NULL ORDER BY seq"
                .into(),
            params: vec![SqlValue::Text(session_id.to_string())],
            label: None,
        })
        .await
        .expect("query ok");
    let texts: Vec<(String, String)> = rows
        .iter()
        .map(|row| {
            let role = match row.get("role") {
                Some(SqlValue::Text(s)) => s.clone(),
                other => panic!("unexpected role value: {other:?}"),
            };
            let text = match row.get("text") {
                Some(SqlValue::Text(s)) => s.clone(),
                other => panic!("unexpected text value: {other:?}"),
            };
            (role, text)
        })
        .collect();
    assert_eq!(
        texts,
        vec![
            ("user".to_string(), "Hello from Codex".to_string()),
            ("assistant".to_string(), "Hello back from Codex".to_string()),
        ],
        "input_text/output_text blocks must round-trip to session_messages.text by role"
    );
}

#[tokio::test]
async fn test_codex_event_id_is_stable_and_idempotent() {
    // Verifies that: (a) synthetic uuid format is "{session_id}:{offset}",
    // and (b) a second mirror_file pass over the same bytes inserts 0 rows.
    let (rt, _dir) = setup().await;

    let session_id = "cdx-sess-idem-0001-0001-000000000002";
    let user_msg = codex_message_line("user", "Idempotency test");

    let mut file = NamedTempFile::new().expect("tmpfile");
    writeln!(file, "{user_msg}").unwrap();

    let path = file.path().to_path_buf();

    // First pass.
    let s1 = mirror_file(&rt, &path, 0, LineTailSource::Codex, Some(session_id))
        .await
        .unwrap();
    assert_eq!(s1.inserted, 1);

    // Verify the stored id matches the expected synthetic format.
    let sql = rt.sql();
    let mut r = sql.reader().await.expect("reader");
    let msg_row = r
        .query_row(SqlStatement {
            sql: "SELECT id FROM session_messages WHERE session_id=?1".into(),
            params: vec![SqlValue::Text(session_id.to_string())],
            label: None,
        })
        .await
        .expect("query ok")
        .expect("message row must exist");
    let stored_id = match msg_row.columns.first().map(|c| &c.value) {
        Some(SqlValue::Text(s)) => s.clone(),
        other => panic!("unexpected id type: {other:?}"),
    };
    // The line starts at byte offset 0.
    let expected_id = format!("{session_id}:0");
    assert_eq!(
        stored_id, expected_id,
        "synthetic uuid must be {{session_id}}:{{offset}}"
    );

    // Second pass from offset 0: same lines, 0 new rows (idempotent).
    let s2 = mirror_file(&rt, &path, 0, LineTailSource::Codex, Some(session_id))
        .await
        .unwrap();
    assert_eq!(s2.inserted, 0, "second pass must be idempotent");
    assert_eq!(count_rows(&rt, "session_messages").await, 1);

    // Incremental pass from advanced offset: no new data.
    let s3 = mirror_file(
        &rt,
        &path,
        s1.new_offset,
        LineTailSource::Codex,
        Some(session_id),
    )
    .await
    .unwrap();
    assert_eq!(s3.inserted, 0, "incremental pass finds nothing new");
}

#[tokio::test]
async fn test_codex_and_cc_are_independent_sessions() {
    // Both sources can coexist in the same DB; source column distinguishes them.
    let (rt, _dir) = setup().await;

    let cc_line = user_line("cc-uuid-1", "cc-sess-1", "from claude code");
    let mut cc_file = NamedTempFile::new().expect("cc tmpfile");
    writeln!(cc_file, "{cc_line}").unwrap();

    let cdx_session_id = "cdx-sess-coex-0001-0001-000000000003";
    let cdx_msg = codex_message_line("user", "from codex");
    let mut cdx_file = NamedTempFile::new().expect("cdx tmpfile");
    writeln!(cdx_file, "{cdx_msg}").unwrap();

    mirror_file(&rt, cc_file.path(), 0, LineTailSource::ClaudeCode, None)
        .await
        .unwrap();

    mirror_file(
        &rt,
        cdx_file.path(),
        0,
        LineTailSource::Codex,
        Some(cdx_session_id),
    )
    .await
    .unwrap();

    assert_eq!(count_rows(&rt, "sessions").await, 2);
    assert_eq!(count_rows(&rt, "session_messages").await, 2);

    // Verify sources are distinct.
    let sql = rt.sql();
    let mut r = sql.reader().await.expect("reader");
    let rows = r
        .query_all(SqlStatement {
            sql: "SELECT source FROM sessions ORDER BY source".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query ok");
    let sources: Vec<String> = rows
        .iter()
        .filter_map(|row| match row.get("source") {
            Some(SqlValue::Text(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(sources, vec!["claude_code", "codex"]);
}

// ── ChatGPT export whole-file ingest tests ────────────────────────────────
//
// All fixtures below are hand-authored synthetic JSON, not real export
// content. Node ids are set equal to their own `message.id` so that
// `parent_uuid` (which threads through the mapping node id, per
// `parse::build_chatgpt_event`) resolves to the expected message uuid.

use serde_json::json;

fn write_export_file(content: &str) -> (NamedTempFile, std::path::PathBuf) {
    let mut file = NamedTempFile::new().expect("tmpfile");
    write!(file, "{content}").unwrap();
    let path = file.path().to_path_buf();
    (file, path)
}

fn chatgpt_happy_export_json() -> String {
    let conv = json!({
        "id": "conv-happy",
        "title": "Synthetic Happy",
        "create_time": 1_751_462_400.0,
        "current_node": "msg-happy-assistant",
        "mapping": {
            "root-happy": {
                "id": "root-happy",
                "message": null,
                "parent": null,
                "children": ["msg-happy-user"]
            },
            "msg-happy-user": {
                "id": "msg-happy-user",
                "parent": "root-happy",
                "children": ["msg-happy-assistant"],
                "message": {
                    "id": "msg-happy-user",
                    "author": {"role": "user"},
                    "create_time": 1_751_462_400.0,
                    "content": {"content_type": "text", "parts": ["Hello synthetic"]}
                }
            },
            "msg-happy-assistant": {
                "id": "msg-happy-assistant",
                "parent": "msg-happy-user",
                "children": [],
                "message": {
                    "id": "msg-happy-assistant",
                    "author": {"role": "assistant"},
                    "create_time": 1_751_462_401.0,
                    "content": {"content_type": "text", "parts": ["Hi synthetic"]}
                }
            }
        }
    });
    serde_json::to_string(&json!([conv])).unwrap()
}

#[tokio::test]
async fn test_chatgpt_happy_conversations_json() {
    let (rt, _dir) = setup().await;
    let (_file, path) = write_export_file(&chatgpt_happy_export_json());
    let file_len = std::fs::metadata(&path).unwrap().len();

    let stats = mirror_chatgpt_export_file(&rt, &path, 0)
        .await
        .expect("happy path ingest");
    assert_eq!(stats.inserted, 2, "2 message-bearing nodes");
    assert_eq!(stats.scanned, 2, "2 events parsed");
    assert_eq!(stats.new_offset, file_len, "whole-file cursor-at-length");

    let sql = rt.sql();
    let mut r = sql.reader().await.expect("reader");
    let row = r
        .query_row(SqlStatement {
            sql: "SELECT source, slug, cwd, git_branch FROM sessions WHERE id='conv-happy'".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query ok")
        .expect("session row must exist");
    match row.get("source") {
        Some(SqlValue::Text(s)) => assert_eq!(s, "chatgpt_export"),
        other => panic!("unexpected source: {other:?}"),
    }
    match row.get("slug") {
        Some(SqlValue::Text(s)) => assert_eq!(s, "Synthetic Happy"),
        other => panic!("unexpected slug: {other:?}"),
    }
    assert!(
        matches!(row.get("cwd"), Some(SqlValue::Null) | None),
        "chatgpt export never carries a cwd"
    );
    assert!(
        matches!(row.get("git_branch"), Some(SqlValue::Null) | None),
        "chatgpt export never carries a git branch"
    );

    let mut r2 = sql.reader().await.expect("reader");
    let rows = r2
        .query_all(SqlStatement {
            sql: "SELECT seq, role FROM session_messages \
                      WHERE session_id='conv-happy' ORDER BY seq"
                .into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query ok");
    let roles: Vec<(i64, String)> = rows
        .iter()
        .map(|row| {
            let seq = match row.get("seq") {
                Some(SqlValue::Integer(n)) => *n,
                other => panic!("unexpected seq: {other:?}"),
            };
            let role = match row.get("role") {
                Some(SqlValue::Text(s)) => s.clone(),
                other => panic!("unexpected role: {other:?}"),
            };
            (seq, role)
        })
        .collect();
    assert_eq!(
        roles,
        vec![(0, "user".to_string()), (1, "assistant".to_string())]
    );
}

fn chatgpt_idempotency_export_json() -> String {
    let conv = json!({
        "id": "conv-idem",
        "title": "Synthetic Idempotency",
        "current_node": "msg-idem-assistant",
        "mapping": {
            "root-idem": {
                "id": "root-idem",
                "message": null,
                "parent": null,
                "children": ["msg-idem-user"]
            },
            "msg-idem-user": {
                "id": "msg-idem-user",
                "parent": "root-idem",
                "children": ["msg-idem-assistant"],
                "message": {
                    "id": "msg-idem-user",
                    "author": {"role": "user"},
                    "content": {"content_type": "text", "parts": ["Same question again"]}
                }
            },
            "msg-idem-assistant": {
                "id": "msg-idem-assistant",
                "parent": "msg-idem-user",
                "children": [],
                "message": {
                    "id": "msg-idem-assistant",
                    "author": {"role": "assistant"},
                    "content": {"content_type": "text", "parts": ["Same answer again"]}
                }
            }
        }
    });
    serde_json::to_string(&json!([conv])).unwrap()
}

#[tokio::test]
async fn test_chatgpt_reingest_idempotency_conversations_json() {
    let (rt, _dir) = setup().await;
    let (_file, path) = write_export_file(&chatgpt_idempotency_export_json());

    let s1 = mirror_chatgpt_export_file(&rt, &path, 0)
        .await
        .expect("first ingest");
    assert_eq!(s1.inserted, 2);

    let seen_after_first = last_seen_at(&rt, "conv-idem")
        .await
        .expect("session row exists");

    // Re-ingest from offset 0 (the service always re-reads the whole file
    // for this source): same event uuids, INSERT OR IGNORE must dedup.
    let s2 = mirror_chatgpt_export_file(&rt, &path, 0)
        .await
        .expect("second ingest");
    assert_eq!(s2.inserted, 0, "re-ingest must insert 0 new rows");

    let sql = rt.sql();
    let mut r = sql.reader().await.expect("reader");
    let count = r
        .query_row(SqlStatement {
            sql: "SELECT COUNT(*) FROM session_messages WHERE session_id='conv-idem'".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query ok")
        .expect("count row");
    match count.columns.first().map(|c| &c.value) {
        Some(SqlValue::Integer(n)) => assert_eq!(*n, 2, "message count stays at 2"),
        other => panic!("unexpected count: {other:?}"),
    }

    let seen_after_replay = last_seen_at(&rt, "conv-idem")
        .await
        .expect("session row still exists");
    assert_eq!(
        seen_after_first, seen_after_replay,
        "pure replay must not advance last_seen_at"
    );
}

fn chatgpt_branch_sidechain_export_json() -> String {
    let conv = json!({
        "id": "conv-branch",
        "title": "Synthetic Branch",
        "current_node": "msg-branch-main",
        "mapping": {
            "root-branch": {
                "id": "root-branch",
                "message": null,
                "parent": null,
                "children": ["msg-branch-user"]
            },
            "msg-branch-user": {
                "id": "msg-branch-user",
                "parent": "root-branch",
                "children": ["msg-branch-main", "msg-branch-alt"],
                "message": {
                    "id": "msg-branch-user",
                    "author": {"role": "user"},
                    "content": {"content_type": "text", "parts": ["Branch question"]}
                }
            },
            "msg-branch-main": {
                "id": "msg-branch-main",
                "parent": "msg-branch-user",
                "children": [],
                "message": {
                    "id": "msg-branch-main",
                    "author": {"role": "assistant"},
                    "content": {"content_type": "text", "parts": ["Main answer"]}
                }
            },
            "msg-branch-alt": {
                "id": "msg-branch-alt",
                "parent": "msg-branch-user",
                "children": [],
                "message": {
                    "id": "msg-branch-alt",
                    "author": {"role": "assistant"},
                    "content": {"content_type": "text", "parts": ["Alternate answer"]}
                }
            }
        }
    });
    serde_json::to_string(&json!([conv])).unwrap()
}

#[tokio::test]
async fn test_chatgpt_branch_sidechain_conversations_json() {
    let (rt, _dir) = setup().await;
    let (_file, path) = write_export_file(&chatgpt_branch_sidechain_export_json());

    let stats = mirror_chatgpt_export_file(&rt, &path, 0)
        .await
        .expect("branch ingest");
    assert_eq!(stats.inserted, 3, "user + main + alt all stored");

    let sql = rt.sql();
    let mut r = sql.reader().await.expect("reader");
    let rows = r
        .query_all(SqlStatement {
            sql: "SELECT id, is_sidechain, text FROM session_messages \
                      WHERE session_id='conv-branch' ORDER BY id"
                .into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query ok");
    assert_eq!(rows.len(), 3);

    for row in &rows {
        let id = match row.get("id") {
            Some(SqlValue::Text(s)) => s.clone(),
            other => panic!("unexpected id: {other:?}"),
        };
        let is_sidechain = match row.get("is_sidechain") {
            Some(SqlValue::Integer(n)) => *n,
            other => panic!("unexpected is_sidechain: {other:?}"),
        };
        let text = match row.get("text") {
            Some(SqlValue::Text(s)) => s.clone(),
            other => panic!("unexpected text: {other:?}"),
        };
        match id.as_str() {
            "msg-branch-user" | "msg-branch-main" => {
                assert_eq!(is_sidechain, 0, "{id} is on the current-node path")
            }
            "msg-branch-alt" => {
                assert_eq!(is_sidechain, 1, "alt branch is off the current-node path");
                assert_eq!(
                    text, "Alternate answer",
                    "sidechain content must be preserved, not dropped"
                );
            }
            other => panic!("unexpected message id: {other}"),
        }
    }
}

#[tokio::test]
async fn test_chatgpt_malformed_conversations_json_cursor_does_not_advance() {
    let (rt, _dir) = setup().await;

    // Seed the path with a valid (if empty) export and record its cursor.
    let (mut file, path) = write_export_file("[]");
    let seeded_stats = mirror_chatgpt_export_file(&rt, &path, 0)
        .await
        .expect("seeding with an empty array is a valid parse");
    assert_eq!(seeded_stats.inserted, 0);
    let seeded_offset = seeded_stats.new_offset;

    let seeded_sessions = count_rows(&rt, "sessions").await;
    let seeded_messages = count_rows(&rt, "session_messages").await;

    // Overwrite with a longer, malformed (valid-JSON-but-not-an-array) body.
    let malformed = r#"{"oops": "not a chatgpt export array"}"#;
    file.as_file_mut().set_len(0).expect("truncate");
    std::io::Seek::seek(file.as_file_mut(), std::io::SeekFrom::Start(0)).unwrap();
    write!(file, "{malformed}").unwrap();

    let result = mirror_chatgpt_export_file(&rt, &path, seeded_offset).await;
    assert!(
        matches!(result, Err(RuntimeError::Internal(_))),
        "malformed export must return Internal error, got {result:?}"
    );

    let stored_offset = cursor_offset(&rt, &path.to_string_lossy()).await;
    assert_eq!(
        stored_offset,
        Some(seeded_offset as i64),
        "cursor must remain at the pre-error value"
    );
    assert_eq!(
        count_rows(&rt, "sessions").await,
        seeded_sessions,
        "no new session rows on parse failure"
    );
    assert_eq!(
        count_rows(&rt, "session_messages").await,
        seeded_messages,
        "no new message rows on parse failure"
    );
}

#[tokio::test]
async fn test_chatgpt_export_over_max_bytes_is_skipped_without_reading() {
    // PACKSESSION-AUD-003 regression: oversized ChatGPT exports are
    // skipped without reading (see docs guide).
    let (rt, _dir) = setup().await;
    let (_file, path) = write_export_file("[]");

    let file_len = std::fs::metadata(&path).unwrap().len();
    let max_bytes = 1u64; // smaller than even an empty-array export
    assert!(
        file_len > max_bytes,
        "fixture export must exceed the tiny ceiling"
    );

    let stats = mirror_chatgpt_export_file_with_max_bytes(&rt, &path, 0, max_bytes)
        .await
        .expect("an oversized export must be skipped, not error");

    assert_eq!(stats.inserted, 0);
    assert_eq!(stats.scanned, 0);
    assert_eq!(
        stats.new_offset, 0,
        "cursor must not advance past a skipped oversized export"
    );
    assert_eq!(
        cursor_offset(&rt, &path.to_string_lossy()).await,
        None,
        "no cursor row should be written for a skipped pass"
    );
    assert_eq!(count_rows(&rt, "sessions").await, 0);
    assert_eq!(count_rows(&rt, "session_messages").await, 0);
}

#[tokio::test]
async fn test_chatgpt_secret_bearing_conversations_json_is_masked() {
    // Assembled from fragments at runtime so no credential-shaped literal
    // is committed to the repo; matches the AWS-key shape already covered
    // by `khive_runtime::secret_gate`'s own detector tests.
    let secret_fragment_a = "AKIA";
    let secret_fragment_b = "FAKEKEY1234567890";
    let secret = format!("{secret_fragment_a}{secret_fragment_b}");
    let user_text = format!("here is my key: {secret}");

    let conv = json!({
        "id": "conv-secret",
        "title": "Synthetic Secret",
        "current_node": "msg-secret-user",
        "mapping": {
            "root-secret": {
                "id": "root-secret",
                "message": null,
                "parent": null,
                "children": ["msg-secret-user"]
            },
            "msg-secret-user": {
                "id": "msg-secret-user",
                "parent": "root-secret",
                "children": [],
                "message": {
                    "id": "msg-secret-user",
                    "author": {"role": "user"},
                    "content": {"content_type": "text", "parts": [user_text]}
                }
            }
        }
    });
    let content = serde_json::to_string(&json!([conv])).unwrap();
    let (_file, path) = write_export_file(&content);

    let (rt, _dir) = setup().await;
    let stats = mirror_chatgpt_export_file(&rt, &path, 0)
        .await
        .expect("secret-bearing content must still ingest, only masked");
    assert_eq!(stats.inserted, 1);

    let sql = rt.sql();
    let mut r = sql.reader().await.expect("reader");
    let row = r
        .query_row(SqlStatement {
            sql: "SELECT text, raw FROM session_messages WHERE session_id='conv-secret'".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query ok")
        .expect("message row must exist");
    let (stored_text, stored_raw) = match (row.get("text"), row.get("raw")) {
        (Some(SqlValue::Text(t)), Some(SqlValue::Text(r))) => (t.clone(), r.clone()),
        other => panic!("unexpected text/raw shape: {other:?}"),
    };

    assert!(
        !stored_text.contains(&secret),
        "stored text must not contain the raw secret"
    );
    assert!(
        !stored_raw.contains(&secret),
        "stored raw must not contain the raw secret"
    );
    assert!(
        stored_text.contains("***MASKED***"),
        "stored text must carry the secret_gate redaction marker"
    );
    assert!(
        stored_raw.contains("***MASKED***"),
        "stored raw must carry the secret_gate redaction marker"
    );
}

#[tokio::test]
async fn test_chatgpt_export_secret_bearing_title_is_masked_in_stored_slug() {
    let (rt, _dir) = setup().await;
    let secret = format!("{}{}", "AKIA", "FAKEKEY1234567890");
    let conv = json!({
        "id": "conv-secret-title",
        "title": format!("prod creds {secret}"),
        "current_node": "msg-secret-title-user",
        "mapping": {
            "root-secret-title": {
                "id": "root-secret-title",
                "message": null,
                "parent": null,
                "children": ["msg-secret-title-user"]
            },
            "msg-secret-title-user": {
                "id": "msg-secret-title-user",
                "parent": "root-secret-title",
                "children": [],
                "message": {
                    "id": "msg-secret-title-user",
                    "author": {"role": "user"},
                    "content": {"content_type": "text", "parts": ["Hello"]}
                }
            }
        }
    });
    let content = serde_json::to_string(&json!([conv])).unwrap();
    let (_file, path) = write_export_file(&content);

    mirror_chatgpt_export_file(&rt, &path, 0)
        .await
        .expect("secret-title chatgpt conversation ingest");
    assert_eq!(count_rows(&rt, "sessions").await, 1);

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let session = reader
        .query_row(SqlStatement {
            sql: "SELECT slug FROM sessions WHERE id='conv-secret-title'".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query secret-title session")
        .expect("secret-title session row");
    let Some(SqlValue::Text(slug)) = session.get("slug") else {
        panic!("stored slug must be text");
    };
    assert!(!slug.contains(&secret));
    assert!(slug.contains("***MASKED***"));

    // The pre-mask title must not be recoverable from any other stored
    // column either — the message-bearing node this conversation carries
    // has no title field of its own, so text/raw must not echo it back.
    let message_row = reader
        .query_row(SqlStatement {
            sql: "SELECT text, raw FROM session_messages WHERE session_id='conv-secret-title'"
                .into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query secret-title message")
        .expect("secret-title message row");
    let (Some(SqlValue::Text(stored_text)), Some(SqlValue::Text(stored_raw))) =
        (message_row.get("text"), message_row.get("raw"))
    else {
        panic!("stored text/raw must be text");
    };
    assert!(!stored_text.contains(&secret));
    assert!(!stored_raw.contains(&secret));
}

fn claude_ai_happy_export_json() -> String {
    serde_json::to_string(&json!([{
        "uuid": "claude-conv-happy",
        "name": "Synthetic Claude.ai Export",
        "created_at": "2026-07-31T10:00:00Z",
        "current_leaf_message_uuid": "claude-msg-main",
        "chat_messages": [
            {
                "uuid": "claude-msg-user",
                "sender": "human",
                "index": 0,
                "parent_message_uuid": "00000000-0000-4000-8000-000000000000",
                "created_at": "2026-07-31T10:00:01Z",
                "content": [{"type": "text", "text": "Question"}]
            },
            {
                "uuid": "claude-msg-main",
                "sender": "assistant",
                "index": 1,
                "parent_message_uuid": "claude-msg-user",
                "created_at": "2026-07-31T10:00:02Z",
                "content": [{"type": "text", "text": "Current answer"}]
            },
            {
                "uuid": "claude-msg-alt",
                "sender": "assistant",
                "index": 2,
                "parent_message_uuid": "claude-msg-user",
                "created_at": "2026-07-31T10:00:03Z",
                "content": [{"type": "text", "text": "Alternate answer"}]
            }
        ]
    }]))
    .unwrap()
}

#[tokio::test]
async fn test_claude_ai_export_ingest_is_idempotent_and_preserves_branches() {
    let (rt, _dir) = setup().await;
    let (_file, path) = write_export_file(&claude_ai_happy_export_json());
    let file_len = std::fs::metadata(&path).unwrap().len();

    let first = mirror_claude_ai_export_file(&rt, &path, 0)
        .await
        .expect("claude.ai export ingest");
    assert_eq!(first.inserted, 3);
    assert_eq!(first.scanned, 3);
    assert_eq!(first.new_offset, file_len);

    let replay = mirror_claude_ai_export_file(&rt, &path, 0)
        .await
        .expect("idempotent replay");
    assert_eq!(replay.inserted, 0);
    assert_eq!(count_rows(&rt, "sessions").await, 1);
    assert_eq!(count_rows(&rt, "session_messages").await, 3);
    assert_eq!(
        cursor_offset(&rt, &path.to_string_lossy()).await,
        Some(file_len as i64)
    );

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let session = reader
        .query_row(SqlStatement {
            sql: "SELECT provider_session_id, source, slug, message_count \
                      FROM sessions WHERE id='claude-conv-happy'"
                .into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query ok")
        .expect("session row");
    assert!(matches!(
        session.get("provider_session_id"),
        Some(SqlValue::Text(value)) if value == "claude-conv-happy"
    ));
    assert!(matches!(
        session.get("source"),
        Some(SqlValue::Text(value)) if value == "claude_ai_export"
    ));
    assert!(matches!(
        session.get("slug"),
        Some(SqlValue::Text(value)) if value == "Synthetic Claude.ai Export"
    ));
    assert!(matches!(
        session.get("message_count"),
        Some(SqlValue::Integer(3))
    ));

    let messages = reader
        .query_all(SqlStatement {
            sql: "SELECT id, parent_uuid, is_sidechain FROM session_messages \
                      WHERE session_id='claude-conv-happy' ORDER BY seq"
                .into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("message query");
    assert_eq!(messages.len(), 3);
    assert!(matches!(
        messages[0].get("parent_uuid"),
        Some(SqlValue::Null) | None
    ));
    assert!(matches!(
        messages[1].get("is_sidechain"),
        Some(SqlValue::Integer(0))
    ));
    assert!(matches!(
        messages[2].get("id"),
        Some(SqlValue::Text(value)) if value == "claude-msg-alt"
    ));
    assert!(matches!(
        messages[2].get("parent_uuid"),
        Some(SqlValue::Text(value)) if value == "claude-msg-user"
    ));
    assert!(matches!(
        messages[2].get("is_sidechain"),
        Some(SqlValue::Integer(1))
    ));
}

#[tokio::test]
async fn test_claude_ai_empty_conversation_creates_session_and_advances_cursor() {
    let (rt, _dir) = setup().await;
    let export = serde_json::to_string(&json!([{
        "uuid": "claude-conv-empty",
        "name": "Empty Claude Conversation",
        "created_at": "2026-07-31T10:00:00Z",
        "chat_messages": []
    }]))
    .unwrap();
    let (_file, path) = write_export_file(&export);
    let file_len = std::fs::metadata(&path).unwrap().len();

    let first = mirror_claude_ai_export_file(&rt, &path, 0)
        .await
        .expect("empty claude.ai conversation ingest");
    assert_eq!(first.inserted, 0);
    assert_eq!(first.scanned, 0);
    assert_eq!(first.new_offset, file_len);
    assert_eq!(count_rows(&rt, "sessions").await, 1);
    assert_eq!(count_rows(&rt, "session_messages").await, 0);
    assert_eq!(
        cursor_offset(&rt, &path.to_string_lossy()).await,
        Some(file_len as i64)
    );

    let replay = mirror_claude_ai_export_file(&rt, &path, 0)
        .await
        .expect("empty conversation replay");
    assert_eq!(replay.inserted, 0);
    assert_eq!(count_rows(&rt, "sessions").await, 1);

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let session = reader
        .query_row(SqlStatement {
            sql: "SELECT provider_session_id, source, slug, message_count \
                      FROM sessions WHERE id='claude-conv-empty'"
                .into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query empty session")
        .expect("empty session row");
    assert!(matches!(
        session.get("provider_session_id"),
        Some(SqlValue::Text(value)) if value == "claude-conv-empty"
    ));
    assert!(matches!(
        session.get("source"),
        Some(SqlValue::Text(value)) if value == "claude_ai_export"
    ));
    assert!(matches!(
        session.get("slug"),
        Some(SqlValue::Text(value)) if value == "Empty Claude Conversation"
    ));
    assert!(matches!(
        session.get("message_count"),
        Some(SqlValue::Integer(0))
    ));
}

#[tokio::test]
async fn test_claude_ai_unsupported_only_conversation_creates_session() {
    let (rt, _dir) = setup().await;
    let export = serde_json::to_string(&json!([{
        "uuid": "claude-conv-internal-only",
        "summary": "Internal-only Claude Conversation",
        "updated_at": "2026-07-31T10:00:00Z",
        "chat_messages": [{
            "uuid": "claude-msg-internal-only",
            "sender": "assistant",
            "content": [
                {"type": "thinking", "thinking": "not display text"},
                {"type": "provider_internal", "payload": {"hidden": true}}
            ]
        }]
    }]))
    .unwrap();
    let (_file, path) = write_export_file(&export);
    let file_len = std::fs::metadata(&path).unwrap().len();

    let stats = mirror_claude_ai_export_file(&rt, &path, 0)
        .await
        .expect("unsupported-only claude.ai conversation ingest");
    assert_eq!(stats.inserted, 0);
    assert_eq!(stats.scanned, 0);
    assert_eq!(stats.new_offset, file_len);
    assert_eq!(count_rows(&rt, "sessions").await, 1);
    assert_eq!(count_rows(&rt, "session_messages").await, 0);
    assert_eq!(
        cursor_offset(&rt, &path.to_string_lossy()).await,
        Some(file_len as i64)
    );

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let session = reader
        .query_row(SqlStatement {
            sql: "SELECT slug, message_count FROM sessions \
                      WHERE id='claude-conv-internal-only'"
                .into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query unsupported-only session")
        .expect("unsupported-only session row");
    assert!(matches!(
        session.get("slug"),
        Some(SqlValue::Text(value)) if value == "Internal-only Claude Conversation"
    ));
    assert!(matches!(
        session.get("message_count"),
        Some(SqlValue::Integer(0))
    ));
}

#[tokio::test]
async fn test_claude_ai_export_secret_bearing_title_is_masked_in_stored_slug() {
    let (rt, _dir) = setup().await;
    let secret = format!("{}{}", "AKIA", "FAKEKEY1234567890");
    let export = serde_json::to_string(&json!([{
        "uuid": "claude-conv-secret-title",
        "name": format!("prod creds {secret}"),
        "created_at": "2026-07-31T10:00:00Z",
        "chat_messages": []
    }]))
    .unwrap();
    let (_file, path) = write_export_file(&export);

    mirror_claude_ai_export_file(&rt, &path, 0)
        .await
        .expect("secret-title claude.ai conversation ingest");
    assert_eq!(count_rows(&rt, "sessions").await, 1);

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let session = reader
        .query_row(SqlStatement {
            sql: "SELECT slug FROM sessions WHERE id='claude-conv-secret-title'".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query secret-title session")
        .expect("secret-title session row");
    let Some(SqlValue::Text(slug)) = session.get("slug") else {
        panic!("stored slug must be text");
    };
    assert!(!slug.contains(&secret));
    assert!(slug.contains("***MASKED***"));
}

#[tokio::test]
async fn test_claude_ai_export_over_max_bytes_leaves_cursor_untouched() {
    let (rt, _dir) = setup().await;
    let (_file, path) = write_export_file("[]");

    let stats = mirror_claude_ai_export_file_with_max_bytes(&rt, &path, 0, 1)
        .await
        .expect("oversized export is skipped");
    // The pass reports the identity it observed (the in-memory poll state
    // needs it), but persists nothing: no cursor row is written.
    assert_eq!(
        stats,
        MirrorStats {
            file_identity: Some(
                file_identity(&std::fs::File::open(&path).expect("open export"))
                    .expect("export identity")
            ),
            ..MirrorStats::default()
        }
    );
    assert_eq!(cursor_offset(&rt, &path.to_string_lossy()).await, None);
}

/// SS6 invariants #4/#5 (error never advances cursor; one transaction per
/// pass) — see
/// `crates/khive-pack-session/docs/api/mirror-ingest.md#test_mid_transaction_db_error_leaves_no_partial_state_and_cursor_unadvanced`
/// for why this drives `atomic_unit` directly instead of through crafted event data.
#[tokio::test]
async fn test_mid_transaction_db_error_leaves_no_partial_state_and_cursor_unadvanced() {
    let (rt, _dir) = setup().await;
    let sql = rt.sql();
    let path = std::path::Path::new("/synthetic/mid-tx-probe.json");
    let path_owned = path.to_path_buf();

    let op: khive_storage::AtomicUnitOp = Box::new(move |writer: &mut dyn SqlWriter| {
        Box::pin(async move {
            // First write succeeds — mirrors event 1's session row in a
            // multi-event file pass.
            writer
                    .execute(SqlStatement {
                        sql: "INSERT INTO sessions \
                              (id, provider_session_id, source, message_count, first_seen_at, last_seen_at, namespace) \
                              VALUES('mid-tx-session', 'mid-tx-session', 'chatgpt_export', 0, 1, 1, 'local')"
                            .into(),
                        params: vec![],
                        label: None,
                    })
                    .await?;

            // Cursor advance succeeds too — mirrors `upsert_cursor_on_writer`
            // running near the end of `write_events_and_cursor_on_writer`.
            upsert_cursor_on_writer(writer, &path_owned, Some("mid-tx-session"), 999, 1, None)
                .await?;

            // Third write fails with a genuine (non-suppressed) SQL error —
            // mirrors a mid-loop DB failure on a later event in the same file.
            writer
                .execute(SqlStatement {
                    sql: "INSERT INTO no_such_table_mid_tx_probe(a) VALUES(1)".into(),
                    params: vec![],
                    label: None,
                })
                .await?;

            Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
        })
    });

    // `atomic_unit` itself must surface the error and roll back the
    // whole unit — no explicit `commit()`/`drop()` orchestration is the
    // caller's job anymore; the seam owns it.
    let result = sql.atomic_unit(op).await;
    assert!(
        result.is_err(),
        "atomic_unit must propagate the forced third-write failure"
    );

    assert_eq!(
        count_rows(&rt, "sessions").await,
        0,
        "session write must not survive a later error in the same atomic unit"
    );
    assert_eq!(
        cursor_offset(&rt, &path.to_string_lossy()).await,
        None,
        "cursor must not advance when a later write in the same atomic unit fails"
    );
}

/// Build a bare, file-backed, write-queue-enabled `SqlAccess` handle
/// (sidesteps the process-global `KHIVE_WRITE_QUEUE` env var race — see
/// docs guide ADR-099 D5 notes).
fn write_queue_pool(db_path: std::path::PathBuf) -> Arc<khive_db::ConnectionPool> {
    let pool_cfg = khive_db::PoolConfig {
        path: Some(db_path),
        write_queue_enabled: Some(true),
        ..khive_db::PoolConfig::for_test()
    };
    let pool = Arc::new(khive_db::ConnectionPool::new(pool_cfg).expect("pool"));
    {
        let w_conn = pool.writer().expect("writer");
        for stmt in &SESSION_SCHEMA_PLAN_STMTS {
            w_conn
                .conn()
                .execute_batch(stmt)
                .expect("session schema stmt");
        }
    }
    pool
}

/// ADR-099 D5 acceptance: `write_events_and_cursor_on_writer` is
/// suspension-free under `atomic_unit` on the real single-writer path
/// (see docs guide) — a suspending closure would fail `block_on_sync`
/// instead of returning `Ok`.
#[tokio::test]
async fn write_events_and_cursor_is_suspension_free_under_single_writer() {
    let dir = TempDir::new().expect("tempdir");
    let pool = write_queue_pool(dir.path().join("suspend_free.db"));
    let sql: Arc<dyn khive_storage::SqlAccess> =
        Arc::new(khive_db::SqlBridge::new(Arc::clone(&pool), true));

    pool.writer_task_handle()
        .unwrap()
        .expect("writer task must be spawned with the flag on for a file-backed pool");

    let events = vec![parse::parse_cc_line(
            r#"{"uuid":"evt-1","sessionId":"suspend-free-session","type":"user","message":{"role":"user","content":"hello"},"cwd":"/tmp","timestamp":"2026-01-01T00:00:00Z"}"#,
        )
        .expect("line must parse")];

    let path = std::path::Path::new("/synthetic/suspend-free.jsonl").to_path_buf();
    let now_us = Utc::now().timestamp_micros();
    let op: khive_storage::AtomicUnitOp = Box::new(move |writer: &mut dyn SqlWriter| {
        Box::pin(async move {
            write_events_and_cursor_on_writer(
                writer,
                &path,
                "claude_code",
                &[],
                &events,
                MirrorWriteProgress {
                    scanned: 1,
                    new_offset: 100,
                    now_us,
                    file_identity: "test-file",
                },
            )
            .await
            .map(|stats| Box::new(stats) as Box<dyn std::any::Any + Send>)
            .map_err(|e| {
                khive_storage::StorageError::driver(
                    khive_storage::StorageCapability::Sql,
                    "test_write_events_and_cursor",
                    e,
                )
            })
        })
    });

    let boxed = sql
        .atomic_unit(op)
        .await
        .expect("a suspension-free closure must not hit block_on_sync's Pending error");
    let stats = *boxed
        .downcast::<MirrorStats>()
        .expect("op must return MirrorStats");

    assert_eq!(stats.inserted, 1, "the one event must be inserted");

    let mut reader = sql.reader().await.expect("reader");
    let row = khive_storage::SqlReader::query_scalar(
        reader.as_mut(),
        SqlStatement {
            sql: "SELECT COUNT(*) FROM sessions".into(),
            params: vec![],
            label: None,
        },
    )
    .await
    .expect("count query")
    .expect("count row");
    match row {
        SqlValue::Integer(1) => {}
        other => panic!("the session row must be committed, got COUNT(*) = {other:?}"),
    }
}

/// ADR-099 D5 acceptance ("single-writer concurrency, mandatory"):
/// session-mirror ingest must route through the shared writer task, not
/// open its own standalone `BEGIN IMMEDIATE` (see docs guide for the
/// queue-depth + occupier-parked-on-oneshot technique).
#[tokio::test]
async fn session_ingest_routes_through_writer_task_when_flag_enabled() {
    let dir = TempDir::new().expect("tempdir");
    let pool = write_queue_pool(dir.path().join("concurrency.db"));
    let sql: Arc<dyn khive_storage::SqlAccess> =
        Arc::new(khive_db::SqlBridge::new(Arc::clone(&pool), true));

    let writer_task = pool
        .writer_task_handle()
        .unwrap()
        .expect("writer task must be spawned with the flag on for a file-backed pool");

    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let occupier = {
        let writer_task = writer_task.clone();
        tokio::spawn(async move {
            writer_task
                .send(move |_conn| {
                    let _ = started_tx.send(());
                    let _ = release_rx.blocking_recv();
                    Ok::<(), khive_storage::StorageError>(())
                })
                .await
        })
    };

    started_rx
        .await
        .expect("occupier must signal it has started running inside the writer task");
    assert_eq!(
        writer_task.queue_depth(),
        0,
        "channel must start empty once the occupier has been dequeued and is running"
    );

    let events = vec![parse::parse_cc_line(
            r#"{"uuid":"evt-concurrency-1","sessionId":"concurrency-session","type":"user","message":{"role":"user","content":"hello"},"cwd":"/tmp","timestamp":"2026-01-01T00:00:00Z"}"#,
        )
        .expect("line must parse")];
    let path = std::path::Path::new("/synthetic/concurrency.jsonl").to_path_buf();
    let now_us = Utc::now().timestamp_micros();
    let op: khive_storage::AtomicUnitOp = Box::new(move |writer: &mut dyn SqlWriter| {
        Box::pin(async move {
            write_events_and_cursor_on_writer(
                writer,
                &path,
                "claude_code",
                &[],
                &events,
                MirrorWriteProgress {
                    scanned: 1,
                    new_offset: 100,
                    now_us,
                    file_identity: "test-file",
                },
            )
            .await
            .map(|stats| Box::new(stats) as Box<dyn std::any::Any + Send>)
            .map_err(|e| {
                khive_storage::StorageError::driver(
                    khive_storage::StorageCapability::Sql,
                    "test_session_ingest_concurrency",
                    e,
                )
            })
        })
    });

    let sql_for_ingest = Arc::clone(&sql);
    let ingest_task = tokio::spawn(async move { sql_for_ingest.atomic_unit(op).await });

    let mut saw_enqueued = false;
    for _ in 0..100 {
        if writer_task.queue_depth() >= 1 {
            saw_enqueued = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        saw_enqueued,
        "session-ingest's atomic_unit request never appeared in the writer task's \
             channel while the occupier held the single drain slot — the converted ingest \
             path is not routing through the shared writer task (a standalone `begin_tx` \
             connection would never show up here at all)"
    );

    release_tx
        .send(())
        .expect("occupier must still be waiting on the release signal");
    occupier
        .await
        .expect("occupier task must not panic")
        .expect("occupier write must succeed");

    let boxed = ingest_task
        .await
        .expect("ingest task must not panic")
        .expect("ingest atomic_unit must succeed once the occupier releases the slot");
    let stats = *boxed
        .downcast::<MirrorStats>()
        .expect("op must return MirrorStats");
    assert_eq!(stats.inserted, 1, "the ingest event must be committed");
}

/// ADR-099 revert-companion test: the pre-conversion shape (a closure
/// issuing its own `BEGIN IMMEDIATE` inside `atomic_unit`) must fail
/// deterministically with a nested-transaction error — proves the
/// suspension-free assertions above are non-vacuous (see docs guide).
#[tokio::test]
async fn old_shape_manual_begin_immediate_inside_atomic_unit_fails() {
    let dir = TempDir::new().expect("tempdir");
    let pool = write_queue_pool(dir.path().join("old_shape_begin_immediate.db"));
    let sql: Arc<dyn khive_storage::SqlAccess> =
        Arc::new(khive_db::SqlBridge::new(Arc::clone(&pool), true));

    pool.writer_task_handle()
        .unwrap()
        .expect("writer task must be spawned with the flag on for a file-backed pool");

    let op: khive_storage::AtomicUnitOp = Box::new(move |writer: &mut dyn SqlWriter| {
        Box::pin(async move {
            // `atomic_unit` already has an open transaction around this
            // closure — issuing a second `BEGIN IMMEDIATE` here is
            // exactly the old `begin_tx`-shaped mistake this ADR
            // retires: a caller managing its own transaction control
            // inside a seam that already owns the transaction boundary.
            writer
                .execute(SqlStatement {
                    sql: "BEGIN IMMEDIATE".into(),
                    params: vec![],
                    label: None,
                })
                .await?;
            Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
        })
    });

    let err = sql.atomic_unit(op).await.expect_err(
        "a closure that issues its own BEGIN IMMEDIATE inside atomic_unit must fail with a \
             nested-transaction error, not silently succeed",
    );
    let msg = err.to_string();
    assert!(
        msg.contains("cannot start a transaction within a transaction"),
        "expected the deterministic nested-transaction failure (SQLite's own message for a \
             second BEGIN issued inside an already-open transaction), got: {msg}"
    );
}

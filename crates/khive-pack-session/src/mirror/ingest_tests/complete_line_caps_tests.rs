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

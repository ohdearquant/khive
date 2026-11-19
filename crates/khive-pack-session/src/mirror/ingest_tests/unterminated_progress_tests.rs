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

#[test]
fn bracketed_unit_first_leaf_refuses_prev_in_every_argument_shape() {
    for source in [
        "[get(id=$prev.id) | stats()]",
        r#"[get(id="$prev.id") | stats()]"#,
        "[get(ids=[$prev.id]) | stats()]",
        r#"[create(kind="concept", name="a", properties={"r": $prev.id}) | stats()]"#,
        "[stats(), get(id=$prev.id) | stats()]",
    ] {
        assert!(
            matches!(
                parse_request(source),
                Err(DslError::PrevRefOutsideChain { .. })
            ),
            "first leaf must not read a previous unit: {source}"
        );
    }

    let allowed = req("[stats() | get(id=$prev.id), stats()]");
    assert_eq!(allowed.mode, ExecutionMode::Parallel);
    assert_eq!(allowed.ops.len(), 3);
    assert_eq!(allowed.ranges, vec![0..2, 2..3]);
    assert_eq!(
        allowed.ops[1].args["id"],
        ArgValue::PrevRef { path: "id".into() }
    );

    let escaped = req(r#"[get(id="\\$prev.id") | stats()]"#);
    assert_eq!(escaped.mode, ExecutionMode::Parallel);
    assert_eq!(val(&escaped.ops[0].args["id"]), &json!("$prev.id"));
}

#[test]
fn bracketed_units_count_every_leaf_at_the_hundred_operation_boundary() {
    let chain = |count: usize| {
        (0..count)
            .map(|_| "stats()")
            .collect::<Vec<_>>()
            .join(" | ")
    };

    let one_unit = req(&format!("[{}]", chain(MAX_OPS)));
    assert_eq!(one_unit.mode, ExecutionMode::Parallel);
    assert_eq!(one_unit.ops.len(), MAX_OPS);
    assert_eq!(one_unit.ranges, vec![0..MAX_OPS]);
    assert!(matches!(
        parse_request(&format!("[{}]", chain(MAX_OPS + 1))),
        Err(DslError::TooManyOps { count, max }) if count == MAX_OPS + 1 && max == MAX_OPS
    ));

    let split = req(&format!("[{}, {}]", chain(50), chain(50)));
    assert_eq!(split.ops.len(), MAX_OPS);
    assert_eq!(split.ranges, vec![0..50, 50..100]);
    assert!(matches!(
        parse_request(&format!("[{}, {}]", chain(50), chain(51))),
        Err(DslError::TooManyOps { count, max }) if count == MAX_OPS + 1 && max == MAX_OPS
    ));
}

// ── MixedSeparators emitted at parse time ─────────────────────────────────────

#[test]
fn bracketed_batch_of_chains_is_parallel_with_unit_ranges() {
    // `[a() | b(), c()]` is a parallel batch of two units (ADR-016
    // Amendment 2): unit 0 is the chain `a() | b()`, unit 1 is the single
    // leaf `c()`. Only inside the outer `[...]` does `,` separate units
    // whose own leaves may themselves be `|`-chained; bare top-level mixing
    // without brackets is still rejected (see the two tests below).
    let r = parse_request("[a() | b(), c()]").unwrap();
    assert_eq!(r.mode, ExecutionMode::Parallel);
    assert_eq!(r.ops.len(), 3);
    assert_eq!(r.ranges, vec![0..2, 2..3]);
}

#[test]
fn unit_dangling_pipe_before_comma_rejected() {
    // A `|` inside one unit still requires a leaf on both sides; dangling
    // it before the unit separator is a parse error, not an empty step.
    let err = parse_request("[a() | , c()]").unwrap_err();
    assert!(
        matches!(err, DslError::InvalidIdentifier { .. }),
        "expected InvalidIdentifier, got {err:?}"
    );
}

#[test]
fn unit_trailing_pipe_before_close_bracket_rejected() {
    // Same dangling-pipe mistake, at the end of the last unit instead of
    // the middle of the batch.
    let err = parse_request("[a(), b() |]").unwrap_err();
    assert!(
        matches!(err, DslError::InvalidIdentifier { .. }),
        "expected InvalidIdentifier, got {err:?}"
    );
}

#[test]
fn nested_batch_inside_bracketed_units_rejected() {
    // No recursion beyond one bracket level: a unit's own leaf can never be
    // another `[...]` batch.
    let err = parse_request("[a(), [b(), c()]]").unwrap_err();
    assert!(
        matches!(err, DslError::InvalidIdentifier { .. }),
        "expected InvalidIdentifier, got {err:?}"
    );
}

#[test]
fn mixed_separator_after_chain_rejected() {
    // `a() | b(), c()` mixes `|` chain with trailing `,`.
    let err = parse_request("a() | b(), c()").unwrap_err();
    assert!(
        matches!(err, DslError::MixedSeparators),
        "expected MixedSeparators, got {err:?}"
    );
}

#[test]
fn comma_only_parallel_accepted() {
    // `[a(), b(), c()]` is valid comma-only parallel batch. An ordinary flat
    // batch is the case where every unit has exactly one leaf: `ranges` is
    // unchanged from before this amendment, one `i..i+1` range per op.
    let r = parse_request("[a(), b(), c()]").unwrap();
    assert_eq!(r.mode, ExecutionMode::Parallel);
    assert_eq!(r.ops.len(), 3);
    assert_eq!(r.ranges, vec![0..1, 1..2, 2..3]);
}

#[test]
fn mixed_separator_before_any_chain_pipe_reaches_the_same_message() {
    // `a(), b() | c()` hits the mixed-separator mistake on the very first
    // op, before a chain `|` has ever been seen, a second code path from
    // `mixed_separator_after_chain_rejected` above (`a() | b(), c()`). Both
    // must reach the same actionable message, not fall through to a generic
    // "expected '|' or end of input" report, and the message must point at
    // the bracketed form that now accepts chains as batch units.
    let err = parse_request("a(), b() | c()").unwrap_err();
    assert!(
        matches!(err, DslError::MixedSeparators),
        "expected MixedSeparators, got {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("cannot mix ',' (parallel) and '|' (chain) separators"),
        "message must name the mix mistake, got: {msg}"
    );
    assert!(
        msg.contains("[a() | b(arg=$prev.id), c()]"),
        "message must point at the bracketed batch of chains, got: {msg}"
    );
    assert!(
        !msg.contains("two `request` calls"),
        "a batch of chains needs no second request, got: {msg}"
    );
}

#[test]
fn trailing_comma_before_batch_close_bracket_named() {
    // `[gtd.next(),]` used to fail with a confusing "invalid identifier"
    // report pointing at the `]` — the real mistake is the trailing `,`.
    let err = parse_request("[gtd.next(),]").unwrap_err();
    assert!(
        matches!(err, DslError::TrailingComma { .. }),
        "expected TrailingComma, got {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("trailing comma"),
        "message must name the trailing comma, got: {msg}"
    );
}

#[test]
fn pipe_only_chain_accepted() {
    // `a() | b() | c()` is valid pipe-only chain.
    let r = parse_request("a() | b() | c()").unwrap();
    assert_eq!(r.mode, ExecutionMode::Chain);
    assert_eq!(r.ops.len(), 3);
}

// ── multi-segment dotted verb → clear error ───────────────────────────────────

#[test]
fn three_segment_verb_name_rejected() {
    // `brain.state.debug()` must produce UnsupportedVerbNesting,
    // not the misleading "expected '|' or end of input, found '.'".
    let err = parse_request("brain.state.debug()").unwrap_err();
    assert!(
        matches!(err, DslError::UnsupportedVerbNesting { .. }),
        "expected UnsupportedVerbNesting, got {err:?}"
    );
}

#!/bin/sh
# Validate titled ADR references against the authoritative ADR H1 headings.
set -eu

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# `--self-test`: exercise the parenthetical-prose-citation extraction (the
# `ADR-NNN: <title>` form embedded in plain prose, e.g. inside a crate's
# docs/design.md "ADR Compliance" section) against synthetic fixtures rather
# than the live repo, since the real corpus only carries a handful of these.
# Regression case 1 reproduces the bm25 design.md drift this was added for:
# a parenthetical citation that echoes a truncated ADR title must fail.
# Regression case 2 asserts a bare "(ADR-030)" reference
# with no restated title never false-positives.
self_test() {
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT

    mkdir -p "$tmp/case-fail/docs/adr" "$tmp/case-fail/crates/fixture-crate/docs"
    mkdir -p "$tmp/case-pass/docs/adr" "$tmp/case-pass/crates/fixture-crate/docs"
    mkdir -p "$tmp/case-uncataloged/docs/adr"

    for case in case-fail case-pass case-uncataloged; do
        cat > "$tmp/$case/docs/adr/ADR-030-retrieval-stack-port.md" <<'FIXTURE'
# ADR-030: Retrieval Stack Port — khive-retrieval

**Status**: accepted
FIXTURE
        cat > "$tmp/$case/docs/adr/README.md" <<'FIXTURE'
# ADR Index

<!-- BEGIN GENERATED ADR CATALOG -->

| ADR | Title |
| --- | --- |
| [ADR-030](ADR-030-retrieval-stack-port.md) | Retrieval Stack Port — khive-retrieval |

<!-- END GENERATED ADR CATALOG -->
FIXTURE
    done

    # Regression case 3 (must-FAIL control for the catalog-coverage arm): the
    # same tree with the ADR-030 row deleted from the index must go red. The
    # arm was added because a merged ADR landed with no catalog row and the
    # lint passed; an assertion that has never been observed failing is not
    # yet a check, so this case IS that observation, kept permanent.
    grep -v '^| \[ADR-030\]' "$tmp/case-uncataloged/docs/adr/README.md" > "$tmp/case-uncataloged/README.tmp"
    mv "$tmp/case-uncataloged/README.tmp" "$tmp/case-uncataloged/docs/adr/README.md"

    # Regression case 4 (must-FAIL control, letter-suffixed ids): a letter-suffixed ADR id
    # (the real "ADR-117a" shape) is invisible to the old \d{3}-only file
    # pattern on both sides of the join -- neither added to the authoritative
    # set nor checked against the index -- so it silently passed uncatalogued.
    # A header-only index with no row for it must now go red.
    mkdir -p "$tmp/case-letter-uncataloged/docs/adr"
    cat > "$tmp/case-letter-uncataloged/docs/adr/ADR-117a-fixture-letter-suffix.md" <<'FIXTURE'
# ADR-117a: Fixture Letter Suffix

**Status**: accepted
FIXTURE
    cat > "$tmp/case-letter-uncataloged/docs/adr/README.md" <<'FIXTURE'
# ADR Index

<!-- BEGIN GENERATED ADR CATALOG -->

| ADR | Title |
| --- | --- |

<!-- END GENERATED ADR CATALOG -->
FIXTURE

    # Regression case 5 (must-PASS control, letter-suffixed ids): the same letter-suffixed ADR
    # correctly catalogued must not be flagged -- proves the fix admits the
    # id rather than merely rejecting it everywhere.
    mkdir -p "$tmp/case-letter-cataloged/docs/adr"
    cat > "$tmp/case-letter-cataloged/docs/adr/ADR-117a-fixture-letter-suffix.md" <<'FIXTURE'
# ADR-117a: Fixture Letter Suffix

**Status**: accepted
FIXTURE
    cat > "$tmp/case-letter-cataloged/docs/adr/README.md" <<'FIXTURE'
# ADR Index

<!-- BEGIN GENERATED ADR CATALOG -->

| ADR | Title |
| --- | --- |
| [ADR-117a](ADR-117a-fixture-letter-suffix.md) | Fixture Letter Suffix |

<!-- END GENERATED ADR CATALOG -->
FIXTURE

    # Regression case 6 (must-FAIL control, inert catalog rows): the only index row for the
    # ADR sits inside an HTML comment spanning multiple lines -- a stale
    # example kept for reference, not a live catalog entry. Must go red.
    mkdir -p "$tmp/case-comment-row/docs/adr"
    cat > "$tmp/case-comment-row/docs/adr/ADR-201-fixture-comment-row.md" <<'FIXTURE'
# ADR-201: Fixture Comment Row

**Status**: accepted
FIXTURE
    cat > "$tmp/case-comment-row/docs/adr/README.md" <<'FIXTURE'
# ADR Index

<!-- BEGIN GENERATED ADR CATALOG -->

| ADR | Title |
| --- | --- |
<!--
| [ADR-201](ADR-201-fixture-comment-row.md) | Fixture Comment Row |
-->

<!-- END GENERATED ADR CATALOG -->
FIXTURE

    # Regression case 7 (must-FAIL control, inert catalog rows): the only index row for the
    # ADR sits inside a fenced code block. Must go red.
    mkdir -p "$tmp/case-fenced-row/docs/adr"
    cat > "$tmp/case-fenced-row/docs/adr/ADR-202-fixture-fenced-row.md" <<'FIXTURE'
# ADR-202: Fixture Fenced Row

**Status**: accepted
FIXTURE
    cat > "$tmp/case-fenced-row/docs/adr/README.md" <<'FIXTURE'
# ADR Index

<!-- BEGIN GENERATED ADR CATALOG -->

| ADR | Title |
| --- | --- |
```
| [ADR-202](ADR-202-fixture-fenced-row.md) | Fixture Fenced Row |
```

<!-- END GENERATED ADR CATALOG -->
FIXTURE

    # Regression case 8 (must-FAIL control, catalog boundary): the catalog
    # opens but never closes, and a valid-looking row sits in the trailing
    # prose past the intended boundary. Without an end-of-marker assertion the
    # scan runs to EOF and adopts that row as live coverage, so the file reads
    # as fully catalogued.
    mkdir -p "$tmp/case-unclosed-catalog/docs/adr"
    cat > "$tmp/case-unclosed-catalog/docs/adr/ADR-203-fixture-unclosed.md" <<'FIXTURE'
# ADR-203: Fixture Unclosed Catalog

**Status**: accepted
FIXTURE
    cat > "$tmp/case-unclosed-catalog/docs/adr/README.md" <<'FIXTURE'
# ADR Index

<!-- BEGIN GENERATED ADR CATALOG -->

| ADR | Title |
| --- | --- |

## Superseded entries kept for reference

| [ADR-203](ADR-203-fixture-unclosed.md) | Fixture Unclosed Catalog |
FIXTURE

    # Regression case 9 (must-FAIL control, titled reference): a titled prose
    # citation of a letter-suffixed ADR carrying the wrong title. The coverage
    # grammar admitted such ids before the reference recognizers did, so an
    # ADR-117a citation could restate any title at all and never be compared
    # against the authoritative H1.
    mkdir -p "$tmp/case-letter-ref/docs/adr" "$tmp/case-letter-ref/crates/fixture-crate/docs"
    cat > "$tmp/case-letter-ref/docs/adr/ADR-117a-fixture-letter-suffix.md" <<'FIXTURE'
# ADR-117a: Fixture Letter Suffix

**Status**: accepted
FIXTURE
    cat > "$tmp/case-letter-ref/docs/adr/README.md" <<'FIXTURE'
# ADR Index

<!-- BEGIN GENERATED ADR CATALOG -->

| ADR | Title |
| --- | --- |
| [ADR-117a](ADR-117a-fixture-letter-suffix.md) | Fixture Letter Suffix |

<!-- END GENERATED ADR CATALOG -->
FIXTURE
    cat > "$tmp/case-letter-ref/crates/fixture-crate/docs/design.md" <<'FIXTURE'
# fixture-crate Design

## ADR Compliance

- follows the suffix convention (ADR-117a: Deliberately Wrong Title).
FIXTURE

    # Regression case 10 (must-PASS control, titled reference): the same
    # citation restating the real title must stay green -- proves case 9 fails
    # on the title comparison rather than on the widened id shape itself.
    mkdir -p "$tmp/case-letter-ref-ok/docs/adr" "$tmp/case-letter-ref-ok/crates/fixture-crate/docs"
    cat > "$tmp/case-letter-ref-ok/docs/adr/ADR-117a-fixture-letter-suffix.md" <<'FIXTURE'
# ADR-117a: Fixture Letter Suffix

**Status**: accepted
FIXTURE
    cat > "$tmp/case-letter-ref-ok/docs/adr/README.md" <<'FIXTURE'
# ADR Index

<!-- BEGIN GENERATED ADR CATALOG -->

| ADR | Title |
| --- | --- |
| [ADR-117a](ADR-117a-fixture-letter-suffix.md) | Fixture Letter Suffix |

<!-- END GENERATED ADR CATALOG -->
FIXTURE
    cat > "$tmp/case-letter-ref-ok/crates/fixture-crate/docs/design.md" <<'FIXTURE'
# fixture-crate Design

## ADR Compliance

- follows the suffix convention (ADR-117a: Fixture Letter Suffix).
FIXTURE

    # Regression case 11 (must-FAIL control, inert END in a fence): the catalog
    # never really closes, but a quoted END marker sits inside a code fence.
    # Testing the delimiter before resolving fence state let that quoted text
    # satisfy the end-marker assertion, so an unclosed catalog passed again by
    # a different route than case 8.
    mkdir -p "$tmp/case-fenced-end/docs/adr"
    cat > "$tmp/case-fenced-end/docs/adr/ADR-204-fixture-fenced-end.md" <<'FIXTURE'
# ADR-204: Fixture Fenced End

**Status**: accepted
FIXTURE
    cat > "$tmp/case-fenced-end/docs/adr/README.md" <<'FIXTURE'
# ADR Index

<!-- BEGIN GENERATED ADR CATALOG -->

| ADR | Title |
| --- | --- |
| [ADR-204](ADR-204-fixture-fenced-end.md) | Fixture Fenced End |

The catalog is delimited like this:

```
<!-- END GENERATED ADR CATALOG -->
```
FIXTURE

    # Regression case 12 (must-FAIL control, inert END in a comment): the same
    # defect reached through the multi-line HTML-comment state instead of the
    # fence state.
    mkdir -p "$tmp/case-commented-end/docs/adr"
    cat > "$tmp/case-commented-end/docs/adr/ADR-205-fixture-commented-end.md" <<'FIXTURE'
# ADR-205: Fixture Commented End

**Status**: accepted
FIXTURE
    cat > "$tmp/case-commented-end/docs/adr/README.md" <<'FIXTURE'
# ADR Index

<!-- BEGIN GENERATED ADR CATALOG -->

| ADR | Title |
| --- | --- |
| [ADR-205](ADR-205-fixture-commented-end.md) | Fixture Commented End |

<!--
<!-- END GENERATED ADR CATALOG -->
-->
FIXTURE

    # Regression case 13 (must-FAIL control, inert BEGIN): the mirror of cases
    # 11 and 12 on the opening delimiter. A quoted BEGIN inside a multi-line
    # HTML comment, above the real catalog, opened the scan early, so a stale
    # row sitting between the quoted marker and the real one counted as live
    # coverage for an ADR the real catalog omits.
    #
    # The quoted marker is inside a COMMENT rather than a fence deliberately.
    # A fenced variant of this fixture goes red either way: the fence's own
    # closing line lands after the delimiter test and swallows the stale row,
    # so the arm would pass without the opening delimiter being fence-aware at
    # all. It would have been an arm satisfied for a reason other than the one
    # it names.
    mkdir -p "$tmp/case-quoted-begin/docs/adr"
    cat > "$tmp/case-quoted-begin/docs/adr/ADR-206-fixture-quoted-begin.md" <<'FIXTURE'
# ADR-206: Fixture Quoted Begin

**Status**: accepted
FIXTURE
    cat > "$tmp/case-quoted-begin/docs/adr/README.md" <<'FIXTURE'
# ADR Index

<!--
<!-- BEGIN GENERATED ADR CATALOG -->
-->

| [ADR-206](ADR-206-fixture-quoted-begin.md) | Fixture Quoted Begin |

<!-- BEGIN GENERATED ADR CATALOG -->

| ADR | Title |
| --- | --- |

<!-- END GENERATED ADR CATALOG -->
FIXTURE

    # Regression case 14 (must-FAIL control, mismatched fence characters): the
    # catalog never closes, and the only END sits inside a tilde fence that a
    # backtick line appears to close. In Markdown a backtick line is ordinary
    # text inside a tilde fence, so a single in-fence flag toggled off there
    # and exposed the quoted END as a real delimiter.
    mkdir -p "$tmp/case-mixed-fence/docs/adr"
    cat > "$tmp/case-mixed-fence/docs/adr/ADR-207-fixture-mixed-fence.md" <<'FIXTURE'
# ADR-207: Fixture Mixed Fence

**Status**: accepted
FIXTURE
    cat > "$tmp/case-mixed-fence/docs/adr/README.md" <<'FIXTURE'
# ADR Index

<!-- BEGIN GENERATED ADR CATALOG -->

| ADR | Title |
| --- | --- |
| [ADR-207](ADR-207-fixture-mixed-fence.md) | Fixture Mixed Fence |

~~~
```
<!-- END GENERATED ADR CATALOG -->
```
~~~
FIXTURE

    # Regression case 15 (must-FAIL control, short closing fence): a fence
    # closes only on a run at least as long as the one that opened it, so a
    # three-backtick line does not close a four-backtick fence and the END
    # quoted after it stays inert.
    mkdir -p "$tmp/case-short-fence/docs/adr"
    cat > "$tmp/case-short-fence/docs/adr/ADR-208-fixture-short-fence.md" <<'FIXTURE'
# ADR-208: Fixture Short Fence

**Status**: accepted
FIXTURE
    cat > "$tmp/case-short-fence/docs/adr/README.md" <<'FIXTURE'
# ADR Index

<!-- BEGIN GENERATED ADR CATALOG -->

| ADR | Title |
| --- | --- |
| [ADR-208](ADR-208-fixture-short-fence.md) | Fixture Short Fence |

````
```
<!-- END GENERATED ADR CATALOG -->
```
````
FIXTURE

    # Regression case 16 (must-FAIL control, adjacent comments): one line both
    # closes a comment and opens the next. A scanner that stops at the first
    # `-->` reads the rest of the file as live text, so the END quoted inside
    # the second comment closed the catalog.
    mkdir -p "$tmp/case-chained-comment/docs/adr"
    cat > "$tmp/case-chained-comment/docs/adr/ADR-209-fixture-chained-comment.md" <<'FIXTURE'
# ADR-209: Fixture Chained Comment

**Status**: accepted
FIXTURE
    cat > "$tmp/case-chained-comment/docs/adr/README.md" <<'FIXTURE'
# ADR Index

<!-- BEGIN GENERATED ADR CATALOG -->

| ADR | Title |
| --- | --- |
| [ADR-209](ADR-209-fixture-chained-comment.md) | Fixture Chained Comment |

<!-- first comment
--> <!-- second comment
<!-- END GENERATED ADR CATALOG -->
-->
FIXTURE

    # Regression case 17 (must-FAIL control, indented END): four spaces make a
    # line an indented code block in Markdown, so the marker is inert text. A
    # scanner that stripped the line before comparing accepted it as the real
    # delimiter; exact whole-line matching cannot.
    mkdir -p "$tmp/case-indented-end/docs/adr"
    cat > "$tmp/case-indented-end/docs/adr/ADR-210-fixture-indented-end.md" <<'FIXTURE'
# ADR-210: Fixture Indented End

**Status**: accepted
FIXTURE
    printf '%s\n' \
        '# ADR Index' \
        '' \
        '<!-- BEGIN GENERATED ADR CATALOG -->' \
        '' \
        '| ADR | Title |' \
        '| --- | --- |' \
        '| [ADR-210](ADR-210-fixture-indented-end.md) | Fixture Indented End |' \
        '' \
        '    <!-- END GENERATED ADR CATALOG -->' \
        > "$tmp/case-indented-end/docs/adr/README.md"

    # Regression case 18 (must-FAIL control, two-space indented END): the
    # continuation indentation a list container would give a nested marker,
    # isolated from the fence itself so that only the exact whole-line marker
    # comparison can reject it. Two spaces are neither an indented code block
    # nor a tab, so this arm is not a weaker restatement of case 17 or 19.
    mkdir -p "$tmp/case-continuation-indent-end/docs/adr"
    cat > "$tmp/case-continuation-indent-end/docs/adr/ADR-211-fixture-continuation-indent.md" <<'FIXTURE'
# ADR-211: Fixture Continuation Indent

**Status**: accepted
FIXTURE
    printf '%s\n' \
        '# ADR Index' \
        '' \
        '<!-- BEGIN GENERATED ADR CATALOG -->' \
        '' \
        '| ADR | Title |' \
        '| --- | --- |' \
        '| [ADR-211](ADR-211-fixture-continuation-indent.md) | Fixture Continuation Indent |' \
        '' \
        '  <!-- END GENERATED ADR CATALOG -->' \
        > "$tmp/case-continuation-indent-end/docs/adr/README.md"

    # Regression case 23 (must-PASS control, trailing whitespace on both markers):
    # the discriminator for the rstrip/strip asymmetry. Without it a suite of
    # leading-indentation must-FAIL arms is equally satisfied by a script that
    # rejects any marker line that is not byte-exact, which would go red on a
    # correct index over an invisible character. Markdown reads these two lines
    # identically to the clean ones.
    mkdir -p "$tmp/case-trailing-space-markers/docs/adr"
    cat > "$tmp/case-trailing-space-markers/docs/adr/ADR-216-fixture-trailing-space.md" <<'FIXTURE'
# ADR-216: Fixture Trailing Space

**Status**: accepted
FIXTURE
    printf '%s\n' \
        '# ADR Index' \
        '' \
        '<!-- BEGIN GENERATED ADR CATALOG -->  ' \
        '' \
        '| ADR | Title |' \
        '| --- | --- |' \
        '| [ADR-216](ADR-216-fixture-trailing-space.md) | Fixture Trailing Space |' \
        '' \
        '<!-- END GENERATED ADR CATALOG -->	' \
        > "$tmp/case-trailing-space-markers/docs/adr/README.md"

    # Regression case 22 (must-FAIL control, a fence nested in a list item,
    # kept verbatim as it was originally reported). Recorded because it is the
    # reported defect, NOT because it isolates one mechanism: it is rejected
    # twice over, by the indented END failing the marker comparison AND by the
    # fence line being an unexpected line inside the catalog region. Case 18
    # is the arm that isolates the marker comparison.
    mkdir -p "$tmp/case-list-fence-end/docs/adr"
    cat > "$tmp/case-list-fence-end/docs/adr/ADR-215-fixture-list-fence.md" <<'FIXTURE'
# ADR-215: Fixture List Fence

**Status**: accepted
FIXTURE
    printf '%s\n' \
        '# ADR Index' \
        '' \
        '<!-- BEGIN GENERATED ADR CATALOG -->' \
        '' \
        '| ADR | Title |' \
        '| --- | --- |' \
        '| [ADR-215](ADR-215-fixture-list-fence.md) | Fixture List Fence |' \
        '' \
        '- ```' \
        '  <!-- END GENERATED ADR CATALOG -->' \
        '  ```' \
        > "$tmp/case-list-fence-end/docs/adr/README.md"

    # Regression case 19 (must-FAIL control, tab-indented END): a tab expands to
    # an indented-code prefix in Markdown but counts as one character to a
    # `\s{0,3}` prefix test, which is how a tab could close a real fence early.
    mkdir -p "$tmp/case-tab-end/docs/adr"
    cat > "$tmp/case-tab-end/docs/adr/ADR-212-fixture-tab-end.md" <<'FIXTURE'
# ADR-212: Fixture Tab End

**Status**: accepted
FIXTURE
    printf '%s\n' \
        '# ADR Index' \
        '' \
        '<!-- BEGIN GENERATED ADR CATALOG -->' \
        '' \
        '| ADR | Title |' \
        '| --- | --- |' \
        '| [ADR-212](ADR-212-fixture-tab-end.md) | Fixture Tab End |' \
        '' \
        '	<!-- END GENERATED ADR CATALOG -->' \
        > "$tmp/case-tab-end/docs/adr/README.md"

    # Regression case 20 (must-FAIL control, duplicate BEGIN): marker cardinality
    # was previously unspecified -- the first BEGIN won and any later one was
    # ignored, so a second catalog region could exist unnoticed. Exactly one of
    # each is now required.
    mkdir -p "$tmp/case-duplicate-begin/docs/adr"
    cat > "$tmp/case-duplicate-begin/docs/adr/ADR-213-fixture-duplicate-begin.md" <<'FIXTURE'
# ADR-213: Fixture Duplicate Begin

**Status**: accepted
FIXTURE
    cat > "$tmp/case-duplicate-begin/docs/adr/README.md" <<'FIXTURE'
# ADR Index

<!-- BEGIN GENERATED ADR CATALOG -->

| ADR | Title |
| --- | --- |
| [ADR-213](ADR-213-fixture-duplicate-begin.md) | Fixture Duplicate Begin |

<!-- BEGIN GENERATED ADR CATALOG -->

<!-- END GENERATED ADR CATALOG -->
FIXTURE

    # Regression case 21 (must-FAIL control, markers out of order): an END above
    # the BEGIN previously left the scan looking for a later END that the file
    # did not have, which happened to fail but for the wrong reason. Order is
    # now checked directly.
    mkdir -p "$tmp/case-marker-order/docs/adr"
    cat > "$tmp/case-marker-order/docs/adr/ADR-214-fixture-marker-order.md" <<'FIXTURE'
# ADR-214: Fixture Marker Order

**Status**: accepted
FIXTURE
    cat > "$tmp/case-marker-order/docs/adr/README.md" <<'FIXTURE'
# ADR Index

<!-- END GENERATED ADR CATALOG -->

| ADR | Title |
| --- | --- |
| [ADR-214](ADR-214-fixture-marker-order.md) | Fixture Marker Order |

<!-- BEGIN GENERATED ADR CATALOG -->
FIXTURE

    cat > "$tmp/case-fail/crates/fixture-crate/docs/design.md" <<'FIXTURE'
# fixture-crate Design

## ADR Compliance

- ported as part of the retrieval stack (ADR-030: Retrieval Stack Port).
FIXTURE

    cat > "$tmp/case-pass/crates/fixture-crate/docs/design.md" <<'FIXTURE'
# fixture-crate Design

## ADR Compliance

- ported as part of the retrieval stack (ADR-030).
FIXTURE

    status=0

    if sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-fail" > "$tmp/fail.log" 2>&1; then
        echo "self-test FAILED: drifted parenthetical prose citation (ADR-030: Retrieval Stack Port, missing the '-- khive-retrieval' suffix) was not caught"
        cat "$tmp/fail.log"
        status=1
    elif ! grep -q "ADR-030 title mismatch" "$tmp/fail.log"; then
        echo "self-test FAILED: lint failed, but not for the expected reason:"
        cat "$tmp/fail.log"
        status=1
    else
        echo "self-test OK: drifted parenthetical prose citation caught"
    fi

    if ! sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-pass" > "$tmp/pass.log" 2>&1; then
        echo "self-test FAILED: bare ADR-030 reference (no restated title) should not trip the lint"
        cat "$tmp/pass.log"
        status=1
    else
        echo "self-test OK: bare ADR reference does not false-positive"
    fi

    if sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-uncataloged" > "$tmp/uncataloged.log" 2>&1; then
        echo "self-test FAILED: ADR file with no index catalog row was not caught"
        cat "$tmp/uncataloged.log"
        status=1
    elif ! grep -q "ADR-030 (ADR-030-retrieval-stack-port.md) has no index catalog row" "$tmp/uncataloged.log"; then
        echo "self-test FAILED: uncataloged lint failed, but not for the expected reason:"
        cat "$tmp/uncataloged.log"
        status=1
    else
        echo "self-test OK: uncataloged ADR caught"
    fi

    if sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-letter-uncataloged" > "$tmp/letter-uncataloged.log" 2>&1; then
        echo "self-test FAILED: uncatalogued letter-suffixed ADR-117a was not caught"
        cat "$tmp/letter-uncataloged.log"
        status=1
    elif ! grep -q "ADR-117a (ADR-117a-fixture-letter-suffix.md) has no index catalog row" "$tmp/letter-uncataloged.log"; then
        echo "self-test FAILED: letter-suffixed lint failed, but not for the expected reason:"
        cat "$tmp/letter-uncataloged.log"
        status=1
    else
        echo "self-test OK: uncatalogued letter-suffixed ADR caught"
    fi

    if ! sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-letter-cataloged" > "$tmp/letter-cataloged.log" 2>&1; then
        echo "self-test FAILED: correctly catalogued letter-suffixed ADR-117a should not trip the lint"
        cat "$tmp/letter-cataloged.log"
        status=1
    else
        echo "self-test OK: correctly catalogued letter-suffixed ADR does not false-positive"
    fi

    if sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-comment-row" > "$tmp/comment-row.log" 2>&1; then
        echo "self-test FAILED: index row hidden inside an HTML comment was counted as catalog coverage"
        cat "$tmp/comment-row.log"
        status=1
    elif ! grep -q "unexpected line inside the generated catalog" "$tmp/comment-row.log"; then
        echo "self-test FAILED: comment-row lint failed, but not for the expected reason:"
        cat "$tmp/comment-row.log"
        status=1
    else
        echo "self-test OK: index row hidden inside an HTML comment does not count as coverage"
    fi

    if sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-fenced-row" > "$tmp/fenced-row.log" 2>&1; then
        echo "self-test FAILED: index row hidden inside a fenced code block was counted as catalog coverage"
        cat "$tmp/fenced-row.log"
        status=1
    elif ! grep -q "unexpected line inside the generated catalog" "$tmp/fenced-row.log"; then
        echo "self-test FAILED: fenced-row lint failed, but not for the expected reason:"
        cat "$tmp/fenced-row.log"
        status=1
    else
        echo "self-test OK: index row hidden inside a fenced code block does not count as coverage"
    fi

    if sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-unclosed-catalog" > "$tmp/unclosed.log" 2>&1; then
        echo "self-test FAILED: catalog opened but never closed was accepted, and a row past the intended boundary counted as coverage"
        cat "$tmp/unclosed.log"
        status=1
    elif ! grep -qE 'expected exactly one "<!-- END GENERATED ADR CATALOG -->" marker|unexpected line inside the generated catalog' "$tmp/unclosed.log"; then
        echo "self-test FAILED: unclosed-catalog lint failed, but not for the expected reason:"
        cat "$tmp/unclosed.log"
        status=1
    else
        echo "self-test OK: catalog with no end marker caught"
    fi

    if sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-letter-ref" > "$tmp/letter-ref.log" 2>&1; then
        echo "self-test FAILED: titled citation of letter-suffixed ADR-117a restating a wrong title was not caught"
        cat "$tmp/letter-ref.log"
        status=1
    elif ! grep -q "ADR-117a title mismatch" "$tmp/letter-ref.log"; then
        echo "self-test FAILED: letter-suffixed reference lint failed, but not for the expected reason:"
        cat "$tmp/letter-ref.log"
        status=1
    else
        echo "self-test OK: wrong-titled citation of a letter-suffixed ADR caught"
    fi

    if ! sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-letter-ref-ok" > "$tmp/letter-ref-ok.log" 2>&1; then
        echo "self-test FAILED: correctly titled citation of letter-suffixed ADR-117a should not trip the lint"
        cat "$tmp/letter-ref-ok.log"
        status=1
    else
        echo "self-test OK: correctly titled citation of a letter-suffixed ADR does not false-positive"
    fi

    if sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-fenced-end" > "$tmp/fenced-end.log" 2>&1; then
        echo "self-test FAILED: an END marker quoted inside a code fence satisfied the end-marker assertion"
        cat "$tmp/fenced-end.log"
        status=1
    elif ! grep -qE 'expected exactly one "<!-- END GENERATED ADR CATALOG -->" marker|unexpected line inside the generated catalog' "$tmp/fenced-end.log"; then
        echo "self-test FAILED: fenced-end lint failed, but not for the expected reason:"
        cat "$tmp/fenced-end.log"
        status=1
    else
        echo "self-test OK: END marker quoted inside a code fence does not close the catalog"
    fi

    if sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-commented-end" > "$tmp/commented-end.log" 2>&1; then
        echo "self-test FAILED: an END marker inside a multi-line HTML comment satisfied the end-marker assertion"
        cat "$tmp/commented-end.log"
        status=1
    elif ! grep -qE 'expected exactly one "<!-- END GENERATED ADR CATALOG -->" marker|unexpected line inside the generated catalog' "$tmp/commented-end.log"; then
        echo "self-test FAILED: commented-end lint failed, but not for the expected reason:"
        cat "$tmp/commented-end.log"
        status=1
    else
        echo "self-test OK: END marker inside an HTML comment does not close the catalog"
    fi

    if sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-quoted-begin" > "$tmp/quoted-begin.log" 2>&1; then
        echo "self-test FAILED: a BEGIN marker quoted inside an HTML comment opened the scan early, so a row above the real catalog counted as coverage"
        cat "$tmp/quoted-begin.log"
        status=1
    elif ! grep -q "ADR-206 (ADR-206-fixture-quoted-begin.md) has no index catalog row" "$tmp/quoted-begin.log"; then
        echo "self-test FAILED: quoted-begin lint failed, but not for the expected reason:"
        cat "$tmp/quoted-begin.log"
        status=1
    else
        echo "self-test OK: BEGIN marker quoted inside an HTML comment does not open the catalog"
    fi

    for case in mixed-fence short-fence chained-comment; do
        case "$case" in
            mixed-fence) why="an END quoted inside a tilde fence that a backtick line appeared to close" ;;
            short-fence) why="an END quoted after a short closing fence that does not close the longer opening one" ;;
            chained-comment) why="an END quoted inside a comment reopened on the same line that closed the previous one" ;;
        esac
        if sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-$case" > "$tmp/$case.log" 2>&1; then
            echo "self-test FAILED: $why closed the catalog"
            cat "$tmp/$case.log"
            status=1
        elif ! grep -qE 'expected exactly one "<!-- END GENERATED ADR CATALOG -->" marker|unexpected line inside the generated catalog' "$tmp/$case.log"; then
            echo "self-test FAILED: $case lint failed, but not for the expected reason:"
            cat "$tmp/$case.log"
            status=1
        else
            echo "self-test OK: $why does not close the catalog"
        fi
    done

    if ! sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-trailing-space-markers" > "$tmp/trailing-space.log" 2>&1; then
        echo "self-test FAILED: markers carrying trailing whitespace should still delimit the catalog"
        cat "$tmp/trailing-space.log"
        status=1
    else
        echo "self-test OK: trailing whitespace on a marker does not hide it"
    fi

    # Cases 17-22: the catalog grammar rejects every one of these outright, so
    # each asserts a rejection rather than a particular repair. Their value is
    # that none of them needs Markdown state to be recognised as inert.
    for case in indented-end continuation-indent-end tab-end duplicate-begin marker-order list-fence-end; do
        case "$case" in
            indented-end) why="a four-space indented END" ;;
            continuation-indent-end) why="a two-space indented END" ;;
            list-fence-end) why="an END inside a fence in a list item" ;;
            tab-end) why="a tab-indented END" ;;
            duplicate-begin) why="a second BEGIN marker" ;;
            marker-order) why="an END marker above the BEGIN" ;;
        esac
        if sh "$SCRIPT_DIR/lint-adr-refs.sh" "$tmp/case-$case" > "$tmp/$case.log" 2>&1; then
            echo "self-test FAILED: $why was accepted"
            cat "$tmp/$case.log"
            status=1
        elif ! grep -qE 'expected exactly one|appears before|unexpected line inside the generated catalog|has no index catalog row' "$tmp/$case.log"; then
            echo "self-test FAILED: $case lint failed, but not for the expected reason:"
            cat "$tmp/$case.log"
            status=1
        else
            echo "self-test OK: $why is rejected"
        fi
    done

    return "$status"
}

if [ "${1:-}" = "--self-test" ]; then
    self_test
    exit $?
fi

ROOT="${1:-$SCRIPT_DIR/..}"

python3 - "$ROOT" <<'PY'
from __future__ import annotations

import re
import sys
import unicodedata
from pathlib import Path


root = Path(sys.argv[1]).resolve()
adr_dir = root / "docs" / "adr"
# Shared by the authoritative-file, H1-header, and index-row patterns so the
# three agree on what an ADR id looks like -- e.g. the letter-suffixed
# "ADR-117a" naming a follow-on ADR that shares its parent's number. Captured
# numbers are lowercased at every use site so "117A" and "117a" join.
ADR_NUMBER = r"[0-9]{3}[a-z]?"
adr_file_re = re.compile(rf"^ADR-({ADR_NUMBER})-.*\.md$", re.IGNORECASE)
h1_re = re.compile(
    rf"^#\s+ADR-(?P<number>{ADR_NUMBER})(?:\s+Rev\s+\d+)?\s*:\s*(?P<title>.+?)\s*#*\s*$",
    re.IGNORECASE,
)
colon_ref_re = re.compile(rf"\bADR-(?P<number>{ADR_NUMBER})\s*:\s*", re.IGNORECASE)
paren_ref_re = re.compile(rf"\bADR-(?P<number>{ADR_NUMBER})\s+\(", re.IGNORECASE)
dash_ref_re = re.compile(
    rf"\bADR-(?P<number>{ADR_NUMBER})\s+(?:--?|–|—)\s+", re.IGNORECASE
)
heading_re = re.compile(r"^\s{0,3}#{1,6}\s+(?P<body>.+?)\s*#*\s*$")
link_re = re.compile(r"\[(?P<label>[^]\n]+)\]\((?P<target>[^\n)]+)\)")
adr_led_re = re.compile(rf"^(?:\[)?ADR-{ADR_NUMBER}\b", re.IGNORECASE)
index_row_re = re.compile(
    rf"^\|\s*\[ADR-(?P<number>{ADR_NUMBER})\]\((?P<target>[^)]+)\)\s*"
    r"\|\s*(?P<title>.*?)\s*\|\s*$",
    re.IGNORECASE,
)
edge_punctuation = " \t\r\n`*_~\\\"'“”‘’[]{}<>:;,.!?()#-–—"


def normalize(title: str) -> str:
    title = unicodedata.normalize("NFKC", title)
    previous = None
    while title != previous:
        previous = title
        title = re.sub(r"\s+", " ", title).strip(edge_punctuation)
    return title.casefold()


def first_h1(path: Path) -> tuple[int, str] | None:
    with path.open(encoding="utf-8") as handle:
        for line_number, line in enumerate(handle, 1):
            if line.startswith("# "):
                return line_number, line.rstrip("\n")
    return None


def closing_delimiter(line: str, start: int, opening: str, closing: str) -> int | None:
    depth = 0
    for index in range(start, len(line)):
        if line[index] == opening:
            depth += 1
        elif line[index] == closing:
            depth -= 1
            if depth == 0:
                return index
    return None


def parenthesized_title(line: str, match: re.Match[str]) -> str | None:
    opening = match.end() - 1
    closing = closing_delimiter(line, opening, "(", ")")
    if closing is None:
        return None
    return line[opening + 1 : closing]


def top_level_parens(line: str) -> list[tuple[int, int]]:
    spans: list[tuple[int, int]] = []
    index = 0
    while index < len(line):
        if line[index] == "(":
            closing = closing_delimiter(line, index, "(", ")")
            if closing is None:
                index += 1
                continue
            spans.append((index, closing))
            index = closing + 1
        else:
            index += 1
    return spans


def prose_parenthetical_references(line: str) -> list[tuple[str, str]]:
    # Titled ADR references embedded in plain prose, e.g. "(ADR-030: Retrieval
    # Stack Port)" -- distinct from headings and Markdown link labels, which
    # are handled separately. Bounded to matching parens so trailing sentence
    # content never leaks into the captured title (unlike a naive end-of-line
    # capture). Only fires on the colon-titled form -- a bare "(ADR-030)" or a
    # descriptive gloss like "(ADR-030, hybrid retrieval)" never matches
    # colon_ref_re, so it is left alone.
    references: list[tuple[str, str]] = []
    for open_idx, close_idx in top_level_parens(line):
        inner = line[open_idx + 1 : close_idx]
        inner_matches = list(colon_ref_re.finditer(inner))
        for match_index, inner_match in enumerate(inner_matches):
            end = (
                inner_matches[match_index + 1].start()
                if match_index + 1 < len(inner_matches)
                else len(inner)
            )
            title = inner[inner_match.end() : end].split(";", 1)[0].strip()
            if not title or title[0].isdigit():
                # Empty capture, or a section/line locator like "ADR-017:451-480"
                # rather than a titled reference.
                continue
            references.append((inner_match.group("number").lower(), title))
    return references


def titled_references(label: str) -> list[tuple[str, str]]:
    references: list[tuple[str, str]] = []
    for match in colon_ref_re.finditer(label):
        references.append((match.group("number").lower(), label[match.end() :]))
    for match in paren_ref_re.finditer(label):
        title = parenthesized_title(label, match)
        if title is not None:
            references.append((match.group("number").lower(), title))
    for match in dash_ref_re.finditer(label):
        references.append((match.group("number").lower(), label[match.end() :]))
    return references


def is_local_adr_link(source: Path, target: str) -> bool:
    target = target.split("#", 1)[0]
    if re.match(r"^[a-z]+://", target, re.IGNORECASE):
        return bool(
            re.search(
                r"github\.com/ohdearquant/khive/(?:blob|tree)/[^/]+/docs/adr/ADR-",
                target,
                re.IGNORECASE,
            )
        )
    resolved = (source.parent / target).resolve()
    return resolved.parent == adr_dir.resolve() and resolved.name.upper().startswith("ADR-")


errors: list[str] = []
titles: dict[str, tuple[str, Path]] = {}

for path in sorted(adr_dir.glob("ADR-*.md")):
    file_match = adr_file_re.match(path.name)
    if file_match is None or re.match(r"^ADR-\d{3}-amendment-", path.name, re.IGNORECASE):
        continue
    h1 = first_h1(path)
    relative = path.relative_to(root)
    if h1 is None:
        errors.append(f"{relative}: missing ADR H1 heading")
        continue
    line_number, heading = h1
    heading_match = h1_re.match(heading)
    if heading_match is None:
        errors.append(f"{relative}:{line_number}: malformed ADR H1: {heading!r}")
        continue
    file_number = file_match.group(1).lower()
    heading_number = heading_match.group("number").lower()
    if file_number != heading_number:
        errors.append(
            f"{relative}:{line_number}: filename ADR-{file_number} does not match H1 ADR-{heading_number}"
        )
        continue
    if file_number in titles:
        other = titles[file_number][1].relative_to(root)
        errors.append(f"{relative}:{line_number}: duplicate ADR-{file_number}; also defined by {other}")
        continue
    titles[file_number] = (heading_match.group("title"), path)

index_path = adr_dir / "README.md"
relative = index_path.relative_to(root)
# The catalog is a GENERATED block, so it is validated against a closed grammar
# rather than by modelling the Markdown around it. A hand-rolled fence and
# comment scanner was tried first, and each repair closed one way a quoted
# marker could act as a real delimiter while leaving another -- mismatched fence
# characters, short closers, comments reopened on one line, four-space indented
# code, fences inside list items, tab expansion. Those are structural
# CommonMark concepts, and a line-at-a-time model in this script will keep
# missing them.
#
# The grammar removes the problem instead of chasing it. Markers are matched by
# exact equality on the whole raw line, so an indented or quoted marker is
# simply not a marker and needs no state to recognise as inert. Exactly one of
# each is required, in order. Between them only four line shapes are legal:
# blank, the header row, its separator, and an index row. A fence or a comment
# inside the region is not skipped -- it is rejected by name, because a
# generated block has no business containing either.
catalog_begin = "<!-- BEGIN GENERATED ADR CATALOG -->"
catalog_end = "<!-- END GENERATED ADR CATALOG -->"
catalog_header_re = re.compile(r"^\|\s*ADR\s*\|\s*Title\s*\|$")
catalog_separator_re = re.compile(r"^\|\s*-{3,}\s*\|\s*-{3,}\s*\|$")
cataloged_numbers: set[str] = set()

index_lines = index_path.read_text(encoding="utf-8").split("\n")
# rstrip, never strip: trailing whitespace is invisible to a reader and changes
# nothing about how Markdown reads the line, while LEADING whitespace is exactly
# what has to keep failing -- it is what turns a marker into indented code or a
# container child. The repository's trailing-whitespace hook excludes .md, so
# nothing upstream removes it.
begin_positions = [i for i, line in enumerate(index_lines) if line.rstrip() == catalog_begin]
end_positions = [i for i, line in enumerate(index_lines) if line.rstrip() == catalog_end]


def describe(count: int, marker: str) -> str:
    return f'{relative}: expected exactly one "{marker}" marker at the start of a line, found {count}'


if len(begin_positions) != 1:
    errors.append(describe(len(begin_positions), catalog_begin))
if len(end_positions) != 1:
    errors.append(describe(len(end_positions), catalog_end))
if len(begin_positions) == 1 and len(end_positions) == 1:
    if end_positions[0] < begin_positions[0]:
        errors.append(
            f'{relative}:{end_positions[0] + 1}: "{catalog_end}" appears before '
            f'"{catalog_begin}"'
        )
    else:
        for offset, line in enumerate(
            index_lines[begin_positions[0] + 1 : end_positions[0]]
        ):
            line_number = begin_positions[0] + offset + 2
            if not line.strip():
                continue
            if catalog_header_re.match(line) or catalog_separator_re.match(line):
                continue
            match = index_row_re.match(line)
            if match is None:
                errors.append(
                    f"{relative}:{line_number}: unexpected line inside the generated "
                    f"catalog; only the header, its separator, and index rows are "
                    f"allowed: {line.strip()!r}"
                )
                continue
            number = match.group("number").lower()
            if number in cataloged_numbers:
                errors.append(
                    f"{relative}:{line_number}: duplicate catalog row for ADR-{number}"
                )
                continue
            cataloged_numbers.add(number)
            canonical = titles.get(number)
            if canonical is None:
                errors.append(
                    f"{relative}:{line_number}: ADR-{number} index entry has no authoritative file"
                )
                continue
            expected, expected_path = canonical
            target_path = (adr_dir / match.group("target")).resolve()
            if target_path != expected_path.resolve():
                errors.append(
                    f"{relative}:{line_number}: ADR-{number} index target mismatch; "
                    f'expected "{expected_path.name}", found "{match.group("target")}"'
                )
            found = match.group("title")
            if normalize(found) != normalize(expected):
                errors.append(
                    f'{relative}:{line_number}: ADR-{number} index title mismatch; '
                    f'expected "{expected}", found "{found}"'
                )

# Catalog coverage: every authoritative ADR file must have an index row.
# The index and the tree can otherwise drift silently -- a merged ADR with no
# catalog row passes every per-reference check above because no reference to
# it exists to check.
index_relative = index_path.relative_to(root)
for number in sorted(titles):
    if number not in cataloged_numbers:
        missing_name = titles[number][1].name
        errors.append(
            f"{index_relative}: ADR-{number} ({missing_name}) has no index catalog row"
        )

scan_paths = set((root / "docs").glob("**/*.md"))
scan_paths.update((root / "crates").glob("**/docs/**/*.md"))
scan_paths.update((root / "crates").glob("**/design*.md"))

adr_dir_resolved = adr_dir.resolve()

reference_count = 0
for path in sorted(scan_paths):
    relative = path.relative_to(root)
    # docs/adr/**/*.md itself is excluded from prose-citation scanning: ADR
    # bodies routinely cross-reference sibling ADRs with a deliberately
    # abbreviated gloss ("(ADR-002: Edge Ontology governs the endpoint
    # contract)", "(ADR-001: Artifact entities)") rather than a literal title
    # restatement -- an established, reviewed convention, not drift. Headings
    # and links are still checked everywhere, including docs/adr/.
    prose_eligible = adr_dir_resolved not in path.resolve().parents
    in_fence = False
    with path.open(encoding="utf-8") as handle:
        for line_number, raw_line in enumerate(handle, 1):
            line = raw_line.rstrip("\n")
            if re.match(r"^\s*(```|~~~)", line):
                in_fence = not in_fence
                continue
            if in_fence:
                continue

            labels: list[str] = []
            heading_match = heading_re.match(line)
            if heading_match is not None:
                body = heading_match.group("body")
                if not body.startswith("[") and adr_led_re.match(body):
                    labels.append(body)
            labels.extend(
                match.group("label")
                for match in link_re.finditer(line)
                if adr_led_re.match(match.group("label"))
                and is_local_adr_link(path, match.group("target"))
            )

            references: list[tuple[str, str]] = []
            for label in labels:
                for reference in titled_references(label):
                    if reference not in references:
                        references.append(reference)

            if heading_match is None and prose_eligible:
                for reference in prose_parenthetical_references(line):
                    if reference not in references:
                        references.append(reference)

            for number, found in references:
                reference_count += 1
                canonical = titles.get(number)
                if canonical is None:
                    errors.append(
                        f'{relative}:{line_number}: ADR-{number} has no authoritative file; found title "{found.strip()}"'
                    )
                    continue
                expected = canonical[0]
                if normalize(found) != normalize(expected):
                    errors.append(
                        f'{relative}:{line_number}: ADR-{number} title mismatch; expected "{expected}", found "{found.strip()}"'
                    )

if errors:
    for error in errors:
        print(error)
    print(f"\nADR reference lint: {len(errors)} issue(s)")
    raise SystemExit(1)

print(
    f"ADR reference lint: {len(scan_paths)} file(s), "
    f"{reference_count} titled reference(s) OK"
)
PY

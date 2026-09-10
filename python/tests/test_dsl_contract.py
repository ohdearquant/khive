"""Pins `docs/DSL_WIRE_CONTRACT.md` to the request-parser source it describes.

Two independent checks: every quoted parser line the doc cites must still
exist in the file it names (a parser change that removes or rewrites a rule
fails this test until the doc is updated), and every rule id in the doc has
a concrete case here that exercises `render_dsl` (or the offline fake parser
when the renderer itself does not reach the rule) against the obligation the
doc states.
"""

from __future__ import annotations

import re
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import pytest
from _dsl_fake import DslParseError, PrevRef, parse_dsl, parse_dsl_with_mode

from khive.dsl import MAX_OPS, MAX_OPS_INPUT_LEN, NESTING_DEPTH_LIMIT, render_dsl
from khive.errors import TransportError
from khive.ops import op

REPO_ROOT = Path(__file__).resolve().parents[2]
DOC_PATH = Path(__file__).resolve().parents[1] / "docs" / "DSL_WIRE_CONTRACT.md"
DOC_TEXT = DOC_PATH.read_text()

_ID_RE = re.compile(r"^P\d+$")
# A citation is `path — "quoted line"`, optionally followed immediately by
# `{shared=N}` when the exact quoted text legitimately recurs at N sites in
# that file (the same guard or helper call reused unchanged across modes —
# e.g. the two `MAX_OPS` cap checks) — see
# `test_every_cited_line_exists_in_the_parser_source`.
_CITE_RE = re.compile(r'([\w./\-]+) — "((?:\\.|[^"\\])*)"(?:\{shared=(\d+)\})?')


def _unescape(text: str) -> str:
    """Reverses this doc's table escaping: a backslash escapes the character
    that follows it (covers `\\"`, `\\|`, and `\\\\`), and `&#124;` is the
    HTML-entity form used for a literal pipe outside a quoted span."""
    out: list[str] = []
    i = 0
    n = len(text)
    while i < n:
        if text[i] == "\\" and i + 1 < n:
            out.append(text[i + 1])
            i += 2
            continue
        out.append(text[i])
        i += 1
    return "".join(out).replace("&#124;", "|")


@dataclass(frozen=True)
class Rule:
    id: str
    site_cell: str
    # (path, quoted-line-text, expected-occurrence-count-or-None-for-"exactly one")
    citations: tuple[tuple[str, str, int | None], ...]


def _parse_rules(doc_text: str) -> list[Rule]:
    rules = []
    for line in doc_text.splitlines():
        if not line.startswith("| P"):
            continue
        cells = line.split(" | ")
        if len(cells) != 5:
            continue
        rid = cells[0][2:].strip()
        if not _ID_RE.match(rid):
            continue
        site_cell = cells[1]
        citations = tuple(
            (m.group(1), _unescape(m.group(2)), int(m.group(3)) if m.group(3) else None)
            for m in _CITE_RE.finditer(site_cell)
        )
        rules.append(Rule(id=rid, site_cell=site_cell, citations=citations))
    return rules


RULES = _parse_rules(DOC_TEXT)


def test_rule_ids_are_exactly_p1_through_pn_with_no_gap_or_duplicate():
    ids = [r.id for r in RULES]
    assert len(ids) == len(set(ids)), "duplicate rule id in the doc"
    n = len(ids)
    assert set(ids) == {f"P{i}" for i in range(1, n + 1)}


def test_rule_count_is_pinned():
    # A silent row loss (or gain) fails here even if the id sequence still
    # happens to be contiguous. 111 original rules + P112-P118: six
    # previously-uncited parser sites, plus P118's integer-range rule.
    assert len(RULES) == 118


@pytest.mark.skipif(
    not (REPO_ROOT / "crates" / "khive-request").exists(),
    reason="crates/khive-request is not present in this checkout",
)
def test_every_cited_line_exists_in_the_parser_source():
    """Two checks per citation: the quoted text exists in the file it names
    (a parser change that removes or rewrites a rule fails this test until
    the doc is updated), and it matches at the declared occurrence count.
    This proves presence and count, not branch identity: existence alone
    does not prove a citation names the *decision* it claims to, only that
    the substring is present somewhere in the file (see
    `docs/DSL_WIRE_CONTRACT.md`'s "Citation" note) — naming the deciding
    branch a row claims is a reading obligation on that row's author, not
    something this test can check. A citation whose exact text legitimately
    recurs at N sites (the same guard or helper call, reused unchanged —
    e.g. the two `MAX_OPS` cap checks) must be marked `{shared=N}` and match
    exactly N lines; every other citation must match exactly one line."""
    failures: list[str] = []
    file_cache: dict[str, str | None] = {}
    for rule in RULES:
        for path, quoted, shared in rule.citations:
            if path not in file_cache:
                try:
                    file_cache[path] = (REPO_ROOT / path).read_text()
                except FileNotFoundError:
                    file_cache[path] = None
            content = file_cache[path]
            if content is None:
                failures.append(f"{rule.id}: file not found: {path}")
                continue
            matches = sum(1 for line in content.splitlines() if quoted in line)
            expected = shared if shared is not None else 1
            if matches == 0:
                failures.append(f"{rule.id}: {path!r} has no line containing {quoted!r}")
            elif matches != expected:
                marker = f"{{shared={shared}}}" if shared is not None else "(no shared marker)"
                failures.append(
                    f"{rule.id}: {path!r} citation {quoted!r} matches {matches} lines "
                    f"{marker}, expected {expected}"
                )
    assert not failures, "\n".join(failures)


# ---------------------------------------------------------------------------
# CASES: one entry per rule id, exercising `render_dsl` (or the fake parser,
# for rules the renderer cannot reach) against the doc's stated obligation.
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Case:
    kind: str  # "renders" | "refuses" | "fake" | "not_emitted"
    check: Callable[[], None]
    # Required for kind == "not_emitted": why this rule's obligation has no
    # renderer- or fake-observable behavior to assert.
    reason: str | None = None


CASES: dict[str, Case] = {}


def _add(rule_id: str, kind: str, check: Callable[[], None], *, reason: str | None = None) -> None:
    assert kind in ("renders", "refuses", "fake", "not_emitted")
    if kind == "not_emitted":
        assert reason, f"{rule_id}: a not_emitted case must state its reason"
    assert rule_id not in CASES
    CASES[rule_id] = Case(kind=kind, check=check, reason=reason)


def _rt(tool: str = "verb", **kwargs: Any) -> tuple[str, dict[str, Any]]:
    """Renders one op from `kwargs`, asserts the fake round-trips it back to
    the same (None-pruned, matching `op()`) args, and returns both."""
    ops = [op(tool, **kwargs)]
    rendered = render_dsl(ops)
    [(verb, parsed_args)] = parse_dsl(rendered)
    expected = {k: v for k, v in kwargs.items() if v is not None}
    assert verb == tool
    assert parsed_args == expected
    return rendered, parsed_args


def _refuses(ops: Any, *, match: str | None = None) -> None:
    with pytest.raises(TransportError, match=match):
        render_dsl(ops)


# -- P1: an empty/whitespace-only raw request is refused, never rendered ----
def _check_p1():
    # A structured render always emits at least "tool()" — non-empty. Only
    # raw DSL text a caller passes through verbatim can be empty; the
    # renderer refuses it there instead of forwarding it (see the module
    # docstring's "Raw DSL text" paragraph).
    _refuses("")
    _refuses("   \t\n  ")
    assert render_dsl("whoami()") == "whoami()"


_add("P1", "refuses", _check_p1)


# -- P2: InputTooLarge (1 MiB) — enforced on the rendered byte length --------
_OP_TEXT_OVERHEAD = len('verb(note="")')  # bytes around the note value itself


def _check_p2():
    boundary_content = "x" * (MAX_OPS_INPUT_LEN - _OP_TEXT_OVERHEAD)
    rendered_ok = render_dsl([op("verb", note=boundary_content)])
    assert len(rendered_ok.encode("utf-8")) == MAX_OPS_INPUT_LEN
    assert parse_dsl(rendered_ok) == [("verb", {"note": boundary_content})]

    over_content = boundary_content + "x"
    with pytest.raises(TransportError, match=str(MAX_OPS_INPUT_LEN)):
        render_dsl([op("verb", note=over_content)])

    # Hand-built without the renderer, to prove the fake enforces the same
    # cap independently of whether render_dsl ever produces this text.
    hand_built = 'verb(note="' + over_content + '")'
    assert len(hand_built.encode("utf-8")) == MAX_OPS_INPUT_LEN + 1
    with pytest.raises(DslParseError):
        parse_dsl(hand_built)


_add("P2", "refuses", _check_p2)


def _generic_not_emitted_check():
    rendered = render_dsl([op("stats"), op("whoami")])
    assert not rendered.startswith("{")
    if rendered.startswith("["):
        inner = rendered[1:].lstrip(" \t\n\r")
        assert not inner.startswith("{")


_JSON_FORM_NOT_EMITTED_REASON = (
    "render_dsl never emits JSON-object request syntax (JSON-form batches, typed "
    "args) — it always emits function-call syntax (see the module docstring), so "
    "this rule's JSON-form obligation has no renderer-observable behavior beyond "
    "'the renderer's own output never begins with {'."
)

for _pid in (
    "P3",
    "P4",
    *[f"P{i}" for i in range(13, 20)],
    *[f"P{i}" for i in range(27, 40)],
):
    _add(_pid, "not_emitted", _generic_not_emitted_check, reason=_JSON_FORM_NOT_EMITTED_REASON)


# -- P5: a valid batch element is a complete function call, never a bare value
def _check_p5():
    rendered = render_dsl([op("stats"), op("whoami")])
    parsed = parse_dsl(rendered)
    assert parsed == [("stats", {}), ("whoami", {})]
    assert len(parsed) == 2


_add("P5", "renders", _check_p5)


# -- P6: non-JSON requests always choose function-call syntax ----------------
def _check_p6():
    rendered = render_dsl([op("whoami")])
    assert re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*\(.*\)", rendered)
    assert parse_dsl(rendered) == [("whoami", {})]


_add("P6", "renders", _check_p6)


# -- P7: a single call ends after its closing ) ------------------------------
def _check_p7():
    rendered = render_dsl([op("stats")])
    assert rendered == "stats()"
    _ops, mode = parse_dsl_with_mode(rendered)
    assert mode == "single"


_add("P7", "renders", _check_p7)


# -- P8: single mode must never carry a dynamic $prev reference --------------
def _check_p8():
    # A raw passthrough DSL string is rendered verbatim (see the module
    # docstring: the renderer has no way to validate text it did not itself
    # construct) — this element happens to carry a $prev reference outside a
    # chain, which the real parser rejects with PrevRefOutsideChain; the
    # renderer forwards it unchanged and only the daemon parser catches it.
    rendered = render_dsl(["update(id=$prev.id)"])
    assert rendered == "update(id=$prev.id)"
    with pytest.raises(DslParseError) as exc_info:
        parse_dsl(rendered)
    assert exc_info.value.variant == "PrevRefOutsideChain"


_add("P8", "fake", _check_p8)


# -- P9: reserved arg refused on a single operation --------------------------
def _check_p9():
    _refuses([op("stats", presentation="table")], match="presentation")


_add("P9", "refuses", _check_p9)


# -- P10: a top-level pipe enters chain parsing ------------------------------
def _check_p10():
    rendered = render_dsl([op("stats"), op("whoami")], chained=True)
    assert rendered == "stats() | whoami()"
    _ops, mode = parse_dsl_with_mode(rendered)
    assert mode == "chain"


_add("P10", "renders", _check_p10)


# -- P11: parallel calls are always wrapped in [...], never bare-comma'd ----
def _check_p11():
    rendered = render_dsl([op("stats"), op("whoami")])
    assert rendered.startswith("[") and rendered.endswith("]")
    _ops, mode = parse_dsl_with_mode(rendered)
    assert mode == "parallel"


_add("P11", "renders", _check_p11)


# -- P12: nothing trails a complete bare single operation --------------------
def _check_p12():
    rendered = render_dsl([op("stats")])
    assert rendered[-1] == ")"
    assert rendered == rendered.strip()


_add("P12", "renders", _check_p12)


# -- P20: reserved arg refused on the first operation of a chain -------------
def _check_p20():
    _refuses(
        [op("stats", presentation="table"), op("whoami")],
        match="presentation",
    )


_add("P20", "refuses", _check_p20)


# -- P21: chains longer than 100 are refused, at both the exact boundary
#         and one past it, including the fake's own independent check ------
def _check_p21():
    ops100 = [op("v", i=i) for i in range(100)]
    rendered_ok = render_dsl(ops100, chained=True)
    assert rendered_ok.count(" | ") == 99
    assert len(parse_dsl(rendered_ok)) == 100

    ops101 = [op("v", i=i) for i in range(101)]
    with pytest.raises(TransportError, match=str(MAX_OPS)):
        render_dsl(ops101, chained=True)

    hand_built = " | ".join(f"v(i={i})" for i in range(101))
    with pytest.raises(DslParseError):
        parse_dsl(hand_built)


_add("P21", "refuses", _check_p21)


# -- P22: a complete operation follows every chain separator -----------------
def _check_p22():
    rendered = render_dsl([op("get", id="x"), op("update", note="y")], chained=True)
    assert rendered == 'get(id="x") | update(note="y")'
    assert parse_dsl(rendered) == [("get", {"id": "x"}), ("update", {"note": "y"})]


_add("P22", "renders", _check_p22)


# -- P23: reserved arg refused on a later chain operation --------------------
def _check_p23():
    _refuses(
        [op("get", id="x"), op("update", presentation="table")],
        match="presentation",
    )


_add("P23", "refuses", _check_p23)


# -- P24: a chain terminates cleanly at EOF ----------------------------------
def _check_p24():
    rendered = render_dsl([op("stats"), op("whoami")], chained=True)
    _ops, mode = parse_dsl_with_mode(rendered)
    assert mode == "chain"
    assert rendered == rendered.strip()


_add("P24", "renders", _check_p24)


def _segment_has_top_level_comma(segment: str) -> bool:
    """Quote/bracket-aware scan for a comma outside every nested `()`, `[]`,
    `{}` — checks every chain segment, not just one, so a smuggled
    top-level comma in *any* segment (not only the last) would be caught."""
    depth = 0
    in_string = False
    i = 0
    n = len(segment)
    while i < n:
        ch = segment[i]
        if in_string:
            if ch == "\\" and i + 1 < n:
                i += 2
                continue
            if ch == '"':
                in_string = False
            i += 1
            continue
        if ch == '"':
            in_string = True
        elif ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth -= 1
        elif ch == "," and depth == 0:
            return True
        i += 1
    return False


# -- P25: a chain never mixes in a top-level comma, in ANY segment ----------
def _check_p25():
    rendered = render_dsl(
        [
            op("v", tags=["a", "b"], properties={"x": 1, "y": 2}),
            op("w", note="a,b(c)[d]{e}"),
        ],
        chained=True,
    )
    top_level = rendered.split(" | ")
    assert len(top_level) == 2
    for segment in top_level:
        assert not _segment_has_top_level_comma(segment)


_add("P25", "renders", _check_p25)


# -- P26: only a pipe or end-of-input follows a chain call -------------------
def _check_p26():
    rendered = render_dsl([op("stats"), op("whoami")], chained=True)
    assert rendered == rendered.strip()
    assert not rendered.endswith("|")


_add("P26", "renders", _check_p26)


# -- P40: a batch opens with [ before its first call -------------------------
def _check_p40():
    rendered = render_dsl([op("stats"), op("whoami")])
    assert rendered[0] == "["
    assert rendered[1] != " "


_add("P40", "renders", _check_p40)


# -- P41: an empty batch is refused, never rendered as [] --------------------
def _check_p41():
    _refuses([], match="empty")


_add("P41", "refuses", _check_p41)


# -- P42: batches over 100 ops are refused, at both the exact boundary and
#         one past it, including the fake's own independent check ---------
def _check_p42():
    ops100 = [op("v", i=i) for i in range(100)]
    rendered_ok = render_dsl(ops100)
    assert rendered_ok.count(",") == 99
    assert len(parse_dsl(rendered_ok)) == 100

    ops101 = [op("v", i=i) for i in range(101)]
    with pytest.raises(TransportError, match=str(MAX_OPS)):
        render_dsl(ops101)

    hand_built = "[" + ", ".join(f"v(i={i})" for i in range(101)) + "]"
    with pytest.raises(DslParseError):
        parse_dsl(hand_built)


_add("P42", "refuses", _check_p42)


# -- P43: batch elements are complete function calls, never bare values -----
def _check_p43():
    rendered = render_dsl([op("stats"), op("whoami")])
    inner = rendered[1:-1]
    parts = [p.strip() for p in inner.split(",")]
    assert all(re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*\(.*\)", p) for p in parts)


_add("P43", "renders", _check_p43)


# -- P44: a comma (with optional following ws) joins batch operations -------
def _check_p44():
    rendered = render_dsl([op("stats"), op("whoami")])
    assert ", " in rendered
    assert parse_dsl(rendered) == [("stats", {}), ("whoami", {})]


_add("P44", "renders", _check_p44)


# -- P45: never a trailing comma before the closing ] ------------------------
def _check_p45():
    rendered = render_dsl([op("stats"), op("whoami")])
    assert ",]" not in rendered
    with pytest.raises(DslParseError):
        parse_dsl("[stats(),]")


_add("P45", "renders", _check_p45)


# -- P46: the batch closes with exactly one ] --------------------------------
def _check_p46():
    rendered = render_dsl([op("stats"), op("whoami")])
    assert rendered.endswith("]")
    assert not rendered.endswith("]]")


_add("P46", "renders", _check_p46)


# -- P47: no chain pipe appears inside a function batch ----------------------
def _check_p47():
    rendered = render_dsl([op("stats"), op("whoami")])
    assert "|" not in rendered


_add("P47", "renders", _check_p47)


# -- P48: only , or ] follows each batch operation ---------------------------
def _check_p48():
    rendered = render_dsl([op("stats"), op("whoami"), op("get", id="x")])
    inner = rendered[1:-1]
    assert re.fullmatch(r"[A-Za-z_]+\([^()]*\)(, [A-Za-z_]+\([^()]*\))*", inner)


_add("P48", "renders", _check_p48)


# -- P49: the batch closes before end-of-input -------------------------------
def _check_p49():
    rendered = render_dsl([op("stats"), op("whoami")])
    assert rendered.count("[") == rendered.count("]") == 1
    assert rendered.endswith("]")


_add("P49", "renders", _check_p49)


# -- P50: nothing follows the batch's closing ] ------------------------------
def _check_p50():
    rendered = render_dsl([op("stats"), op("whoami")])
    assert rendered == rendered.strip()
    assert rendered[-1] == "]"


_add("P50", "renders", _check_p50)


# -- P51: no dynamic $prev nested anywhere in a parallel batch ---------------
def _check_p51():
    # A raw passthrough element is rendered verbatim (see the module
    # docstring) inside a parallel batch; this one carries a $prev reference
    # outside a chain, which the real parser rejects with
    # PrevRefOutsideChain — the renderer forwards it unchanged.
    rendered = render_dsl(["update(id=$prev.id)", "other()"])
    assert rendered == "[update(id=$prev.id), other()]"
    with pytest.raises(DslParseError) as exc_info:
        parse_dsl(rendered)
    assert exc_info.value.variant == "PrevRefOutsideChain"


_add("P51", "fake", _check_p51)


# -- P52: reserved arg refused inside a parallel batch -----------------------
def _check_p52():
    _refuses(
        [op("stats"), op("whoami", presentation="table")],
        match="presentation",
    )


_add("P52", "refuses", _check_p52)


# -- P53: a valid nonempty function batch is parallel ------------------------
def _check_p53():
    rendered = render_dsl([op("stats"), op("whoami")])
    _ops, mode = parse_dsl_with_mode(rendered)
    assert mode == "parallel"


_add("P53", "renders", _check_p53)


# -- P54: only ASCII whitespace separates tokens -----------------------------
def _check_p54():
    rendered = render_dsl([op("stats"), op("whoami")], chained=True)
    for ch in rendered:
        if ch.isspace():
            assert ch == " "


_add("P54", "renders", _check_p54)


# -- P55: required delimiters are emitted exactly where expected ------------
def _check_p55():
    rendered, _ = _rt("verb", count=1)
    assert rendered.startswith("verb(") and rendered.endswith(")")


_add("P55", "renders", _check_p55)


# -- P56: tool/argument identifiers start with [A-Za-z_] ---------------------
def _check_p56():
    _refuses([{"tool": "1x", "args": {}}], match="1x")


_add("P56", "refuses", _check_p56)


# -- P57: no non-ASCII or punctuation inside an identifier -------------------
def _check_p57():
    _refuses([{"tool": "verb", "args": {"bad-name": 1}}], match="bad-name")


_add("P57", "refuses", _check_p57)


# -- P58: exactly one namespace dot, valid identifiers both sides ------------
def _check_p58():
    rendered, _ = _rt("blob.put", bytes="x")
    assert rendered.startswith("blob.put(")
    assert parse_dsl(rendered) == [("blob.put", {"bytes": "x"})]


_add("P58", "renders", _check_p58)


# -- P59: never a second namespace dot ---------------------------------------
def _check_p59():
    _refuses([{"tool": "a.b.c", "args": {}}], match=r"a\.b\.c")


_add("P59", "refuses", _check_p59)


# -- P60: ( immediately follows the tool name --------------------------------
def _check_p60():
    rendered, _ = _rt("verb", count=1)
    assert re.match(r"verb\(", rendered)


_add("P60", "renders", _check_p60)


# -- P61: an empty call is allowed but still closes with ) -------------------
def _check_p61():
    rendered = render_dsl([op("stats")])
    assert rendered == "stats()"


_add("P61", "renders", _check_p61)


# -- P62: a call never ends after ( without ) --------------------------------
def _check_p62():
    rendered = render_dsl([op("stats")])
    assert rendered.count("(") == rendered.count(")") == 1


_add("P62", "renders", _check_p62)


# -- P63: a valid identifier precedes every argument = -----------------------
def _check_p63():
    _refuses([{"tool": "verb", "args": {"1x": 2}}], match="1x")


_add("P63", "refuses", _check_p63)


# -- P64: exactly = joins an argument name and its value ---------------------
def _check_p64():
    rendered, _ = _rt("verb", count=1)
    assert "count=1" in rendered
    assert ":" not in rendered.split("(", 1)[1]


_add("P64", "renders", _check_p64)


# -- P65: every argument is one of the parser's value forms ------------------
def _check_p65():
    _rendered, parsed = _rt(
        "verb",
        s="x",
        n=1,
        f=0.5,
        b=True,
        z=None,
        lst=[1, 2],
        obj={"a": 1},
    )
    assert parsed == {"s": "x", "n": 1, "f": 0.5, "b": True, "lst": [1, 2], "obj": {"a": 1}}


_add("P65", "renders", _check_p65)


# -- P66: each function argument name appears at most once ------------------
def _check_p66():
    rendered, _ = _rt("verb", a=1, b=2, c=3)
    assert rendered.count("a=1") == 1
    assert rendered.count("b=2") == 1
    assert rendered.count("c=3") == 1


_add("P66", "renders", _check_p66)


# -- P67: commas between arguments, never a trailing comma ------------------
def _check_p67():
    rendered, _ = _rt("verb", a=1, b=2)
    assert ",)" not in rendered
    assert "a=1, b=2" in rendered


_add("P67", "renders", _check_p67)


# -- P68: ) follows the final argument ---------------------------------------
def _check_p68():
    rendered, _ = _rt("verb", a=1)
    assert rendered.endswith(")")


_add("P68", "renders", _check_p68)


# -- P69: function-arg list nesting is refused past depth 64, at both the
#         exact boundary and one past it, including the fake's own
#         independent check ---------------------------------------------
def _make_nested_list(depth: int) -> Any:
    value: Any = 0
    for _ in range(depth):
        value = [value]
    return value


def _check_p69():
    ok_deep = _make_nested_list(NESTING_DEPTH_LIMIT)
    rendered_ok = render_dsl([op("verb", data=ok_deep)])
    [(verb, parsed_args)] = parse_dsl(rendered_ok)
    assert verb == "verb"
    assert parsed_args == {"data": ok_deep}

    too_deep = _make_nested_list(NESTING_DEPTH_LIMIT + 1)
    with pytest.raises(TransportError, match=str(NESTING_DEPTH_LIMIT)):
        render_dsl([op("verb", data=too_deep)])

    hand_built = (
        "verb(data=" + "[" * (NESTING_DEPTH_LIMIT + 1) + "0" + "]" * (NESTING_DEPTH_LIMIT + 1) + ")"
    )
    with pytest.raises(DslParseError):
        parse_dsl(hand_built)


_add("P69", "refuses", _check_p69)


# -- P70: an empty array literal is a valid argument value -------------------
def _check_p70():
    rendered, parsed = _rt("verb", tags=[])
    assert "tags=[]" in rendered
    assert parsed["tags"] == []


_add("P70", "renders", _check_p70)


# -- P71: array elements are themselves recursively valid values -------------
def _check_p71():
    _rendered, parsed = _rt("verb", data=[1, [2, 3], {"a": "b"}])
    assert parsed["data"] == [1, [2, 3], {"a": "b"}]


_add("P71", "renders", _check_p71)


# -- P72: commas between array elements, ] closes, no trailing comma --------
def _check_p72():
    rendered, _ = _rt("verb", tags=["x", "y"])
    assert ",]" not in rendered
    assert '["x", "y"]' in rendered


_add("P72", "renders", _check_p72)


# -- P73: an array is always closed, never a trailing comma ------------------
def _check_p73():
    rendered, _ = _rt("verb", tags=["x"])
    assert rendered.count("[") == rendered.count("]")


_add("P73", "renders", _check_p73)


# -- P74: array contents (dynamic or not) survive the round trip ------------
def _check_p74():
    _rendered, parsed = _rt("verb", data=[1, {"a": [2, 3]}, "s"])
    assert parsed["data"] == [1, {"a": [2, 3]}, "s"]


_add("P74", "renders", _check_p74)


# -- P75: object nesting is refused past depth 64, at both the exact
#         boundary and one past it, including the fake's own independent
#         check --------------------------------------------------------
def _make_nested_obj(depth: int) -> Any:
    value: Any = 0
    for _ in range(depth):
        value = {"a": value}
    return value


def _check_p75():
    ok_deep = _make_nested_obj(NESTING_DEPTH_LIMIT)
    rendered_ok = render_dsl([op("verb", data=ok_deep)])
    [(verb, parsed_args)] = parse_dsl(rendered_ok)
    assert verb == "verb"
    assert parsed_args == {"data": ok_deep}

    too_deep = _make_nested_obj(NESTING_DEPTH_LIMIT + 1)
    with pytest.raises(TransportError, match=str(NESTING_DEPTH_LIMIT)):
        render_dsl([op("verb", data=too_deep)])

    hand_built = (
        "verb(data="
        + '{"a": ' * (NESTING_DEPTH_LIMIT + 1)
        + "0"
        + "}" * (NESTING_DEPTH_LIMIT + 1)
        + ")"
    )
    with pytest.raises(DslParseError):
        parse_dsl(hand_built)


_add("P75", "refuses", _check_p75)


# -- P76: an empty object literal is a valid argument value ------------------
def _check_p76():
    rendered, parsed = _rt("verb", properties={})
    assert "properties={}" in rendered
    assert parsed["properties"] == {}


_add("P76", "renders", _check_p76)


# -- P77: object keys are always double-quoted -------------------------------
def _check_p77():
    rendered, _ = _rt("verb", properties={"a": 1})
    assert '"a":1' in rendered


_add("P77", "renders", _check_p77)


# -- P78: object-key controls use JSON string escapes ------------------------
def _check_p78():
    rendered, parsed = _rt("verb", properties={"a\nb": 1})
    assert "\\n" in rendered
    assert parsed["properties"] == {"a\nb": 1}


_add("P78", "renders", _check_p78)


# -- P79: : joins every object key and its value -----------------------------
def _check_p79():
    rendered, _ = _rt("verb", properties={"a": 1})
    assert '"a":1' in rendered


_add("P79", "renders", _check_p79)


# -- P80: object values are themselves recursively valid ---------------------
def _check_p80():
    _rendered, parsed = _rt("verb", properties={"a": [1, {"b": 2}]})
    assert parsed["properties"] == {"a": [1, {"b": 2}]}


_add("P80", "renders", _check_p80)


# -- P81: commas between object pairs, } closes, no trailing comma ----------
def _check_p81():
    rendered, _ = _rt("verb", properties={"a": 1, "b": 2})
    assert ",}" not in rendered


_add("P81", "renders", _check_p81)


# -- P82: an object is always closed, never a trailing comma -----------------
def _check_p82():
    rendered, _ = _rt("verb", properties={"a": 1})
    assert rendered.count("{") == rendered.count("}")


_add("P82", "renders", _check_p82)


# -- P83: object keys stay unique through a Python dict ----------------------
def _check_p83():
    # A Python dict cannot itself carry a duplicate key, so the renderer can
    # never emit one from structured data.
    _rendered, parsed = _rt("verb", properties={"a": 1, "b": 2})
    assert list(parsed["properties"]) == ["a", "b"]


_add("P83", "renders", _check_p83)


# -- P84: nested object values survive the round trip ------------------------
def _check_p84():
    _rendered, parsed = _rt("verb", properties={"a": {"b": [1, 2]}})
    assert parsed["properties"] == {"a": {"b": [1, 2]}}


_add("P84", "renders", _check_p84)


# -- P85: a bare-$-looking string is only ever emitted quoted ----------------
def _check_p85():
    rendered, parsed = _rt("verb", query="$foo")
    assert '"$foo"' in rendered
    assert parsed["query"] == "$foo"


_add("P85", "renders", _check_p85)


# -- P86: exactly $prev is a whole-result reference in a chain ---------------
def _check_p86():
    text = 'get(id="x") | update(id=$prev)'
    rendered = render_dsl(text)
    assert rendered == text
    parsed = parse_dsl(rendered)
    assert parsed[1] == ("update", {"id": PrevRef("")})


_add("P86", "renders", _check_p86)


# -- P87: dotted bare paths resolve field segments ---------------------------
def _check_p87():
    text = 'get(id="x") | update(id=$prev.a.b)'
    rendered = render_dsl(text)
    parsed = parse_dsl(rendered)
    assert parsed[1] == ("update", {"id": PrevRef("a.b")})


_add("P87", "renders", _check_p87)


# -- P88: bracket indices are ASCII digits -----------------------------------
def _check_p88():
    text = 'get(id="x") | update(id=$prev[0])'
    rendered = render_dsl(text)
    parsed = parse_dsl(rendered)
    assert parsed[1] == ("update", {"id": PrevRef("[0]")})


_add("P88", "renders", _check_p88)


# -- P89: an empty/signed/quoted bracket index must never be emitted --------
def _check_p89():
    # A raw passthrough chain is not validated by the renderer; the fake
    # (mirroring the real parser) rejects the malformed index it forwards
    # unchanged.
    rendered = render_dsl("first() | second(x=$prev[])")
    assert rendered == "first() | second(x=$prev[])"
    with pytest.raises(DslParseError) as exc_info:
        parse_dsl(rendered)
    assert exc_info.value.variant == "InvalidValue"


_add("P89", "fake", _check_p89)


# -- P90: an index must close immediately after its digits -------------------
def _check_p90():
    # Same passthrough gap as P89, for a malformed index terminator.
    rendered = render_dsl("first() | second(x=$prev[0x])")
    with pytest.raises(DslParseError) as exc_info:
        parse_dsl(rendered)
    assert exc_info.value.variant == "UnexpectedChar"


_add("P90", "fake", _check_p90)


# -- P91: no unparsed punctuation trails a bare path -------------------------
def _check_p91():
    # Same passthrough gap as P89/P90; the fake rejects the trailing
    # punctuation the renderer forwarded unchanged.
    rendered = render_dsl("first() | second(x=$prev.foo-bar)")
    with pytest.raises(DslParseError):
        parse_dsl(rendered)


_add("P91", "fake", _check_p91)


# -- P92: a quoted $prev. path resolves inside a chain -----------------------
def _check_p92():
    text = 'first() | second(x="$prev.id")'
    rendered = render_dsl(text)
    parsed = parse_dsl(rendered)
    assert parsed[1] == ("second", {"x": PrevRef("id")})


_add("P92", "renders", _check_p92)


# -- P93: a quoted $prev[N] path resolves inside a chain ---------------------
def _check_p93():
    text = 'first() | second(x="$prev[0].id")'
    rendered = render_dsl(text)
    parsed = parse_dsl(rendered)
    assert parsed[1] == ("second", {"x": PrevRef("[0].id")})


_add("P93", "renders", _check_p93)


# -- P94: a malformed quoted $prev-shaped value round-trips as a literal ----
def _check_p94():
    _rendered, parsed = _rt("verb", query="$prev.")
    assert parsed["query"] == "$prev."


_add("P94", "renders", _check_p94)


# -- P95: one leading backslash before $prev-shaped text has no wire form --
def _check_p95():
    _refuses([op("verb", query="\\$prev")], match="no representation")


_add("P95", "refuses", _check_p95)


# -- P96: delimiter-containing scalars are always quoted ---------------------
def _check_p96():
    _rendered, parsed = _rt("verb", query="a,b(c)[d]{e}|f")
    assert parsed["query"] == "a,b(c)[d]{e}|f"


_add("P96", "renders", _check_p96)


# -- P97: only double quotes and JSON escapes are ever emitted --------------
def _check_p97():
    value = 'a "quoted" c\\d'
    rendered, parsed = _rt("verb", query=value)
    assert '\\"quoted\\"' in rendered
    assert parsed["query"] == value


_add("P97", "renders", _check_p97)


# -- P98: raw LF/CR/TAB in a value are emitted as their JSON escape ---------
def _check_p98():
    value = "a\nb\tc\rd"
    rendered, parsed = _rt("verb", note=value)
    assert "\\n" in rendered and "\\t" in rendered and "\\r" in rendered
    assert parsed["note"] == value


_add("P98", "renders", _check_p98)


# -- P99: other raw controls are JSON-escaped in nested values ---------------
def _check_p99():
    # At the argument's own top level `_render_string` refuses any other raw
    # control byte outright (see `test_control_character_raises_transport_error`
    # in test_dsl.py); nested inside an object, `json.dumps` escapes it, so
    # the obligation is satisfied there.
    rendered, parsed = _rt("verb", properties={"a": "x\x00y"})
    assert "\\u0000" in rendered
    assert parsed["properties"] == {"a": "x\x00y"}


_add("P99", "renders", _check_p99)


def test_nested_object_backspace_and_form_feed_use_short_json_escapes():
    """`\\b` (backspace) and `\\f` (form feed) are standard JSON short
    escapes, distinct from the `\\uXXXX` fallback P99 covers above for a
    control with no short form (NUL)."""
    rendered, parsed = _rt("verb", properties={"a": "x\bY\fz"})
    assert "\\b" in rendered and "\\f" in rendered
    assert parsed["properties"] == {"a": "x\bY\fz"}


# -- P100: the renderer never emits a backslash immediately before a raw
#          control byte (it always names the escape) ------------------------
def _check_p100():
    rendered, parsed = _rt("verb", note="a\\b\nc")
    assert "\\\\" in rendered
    assert parsed["note"] == "a\\b\nc"


_add("P100", "renders", _check_p100)


# -- P101: only valid JSON number syntax is ever emitted ---------------------
def _check_p101():
    rendered, parsed = _rt("verb", weight=-2.5e-3, count=42)
    assert re.search(r"weight=-?\d[\d.eE+-]*", rendered)
    assert not re.search(r"=0\d", rendered)
    assert parsed == {"weight": -2.5e-3, "count": 42}


_add("P101", "renders", _check_p101)


# -- P102: true/false/null are always lowercase ------------------------------
def _check_p102():
    rendered, parsed = _rt("verb", hard=True, soft=False, kind="x")
    assert "true" in rendered and "false" in rendered
    assert parsed == {"hard": True, "soft": False, "kind": "x"}

    # `op()` prunes `None` args before render_dsl ever sees them (see
    # `khive.ops.op`), so exercising the `null` obligation itself needs a
    # raw op dict built past that helper.
    raw_op = {"tool": "verb", "args": {"missing": None}}
    rendered_null = render_dsl([raw_op])
    assert "null" in rendered_null
    assert parse_dsl(rendered_null) == [("verb", {"missing": None})]


_add("P102", "renders", _check_p102)


# -- P103: every string value is double-quoted, even identifier-looking ----
def _check_p103():
    rendered, parsed = _rt("verb", note="abc")
    assert '"abc"' in rendered
    assert "=abc" not in rendered
    assert parsed["note"] == "abc"


_add("P103", "renders", _check_p103)


# -- P104: exactly one complete scalar value per argument --------------------
def _check_p104():
    rendered, _ = _rt("verb", a=1, b="x")
    assert re.fullmatch(r"verb\(a=1, b=\"x\"\)", rendered)


_add("P104", "renders", _check_p104)


# -- P105: every quoted string/key is closed with a double quote ------------
def _check_p105():
    rendered, _ = _rt("verb", note="x")
    assert rendered.count('"') % 2 == 0


_add("P105", "renders", _check_p105)


# -- P106: nested [ and { are always balanced --------------------------------
def _check_p106():
    rendered, _ = _rt("verb", data=[{"a": 1}])
    assert rendered.count("[") == rendered.count("]")
    assert rendered.count("{") == rendered.count("}")


_add("P106", "renders", _check_p106)


# -- P107: nested containers close in order before the call's ) ------------
def _check_p107():
    rendered, _ = _rt("verb", data=[1, [2]])
    # A balanced, well-formed close: stripping the outer call leaves nothing
    # but balanced containers.
    assert rendered.endswith("])")


_add("P107", "renders", _check_p107)


# -- P108: an index that fits usize resolves; parser acceptance alone does
#          not guarantee a hit -----------------------------------------------
def _check_p108():
    """Reference resolution (turning `$prev[0]` into an actual lookup) is a
    runtime concern that starts only after the parser has accepted the
    reference. This pins that the renderer emits plain ASCII-digit indices
    for the resolver to consume, and that an index too large to fit a
    platform usize is still grammar-accepted syntax passed through
    unchanged — it only fails later, at runtime resolution in
    `path.rs::apply_path_segment`. This offline fake has no mirror of that
    runtime resolution step (it only mirrors the parser grammar), so only
    parser acceptance is asserted here; the runtime miss on an oversized
    index is instead pinned by
    `path::tests::oversized_bracket_index_is_malformed_and_always_misses` in
    `crates/khive-request/src/parser/path.rs`.
    """
    text = "first() | second(x=$prev[12])"
    rendered = render_dsl(text)
    m = re.search(r"\$prev\[(\d+)\]", rendered)
    assert m and m.group(1).isascii() and m.group(1).isdigit()
    parsed = parse_dsl(rendered)
    assert parsed[1] == ("second", {"x": PrevRef("[12]")})

    oversized_text = "first() | second(x=$prev[99999999999999999999])"
    rendered_oversized = render_dsl(oversized_text)
    assert rendered_oversized == oversized_text
    parsed_oversized = parse_dsl(rendered_oversized)
    assert parsed_oversized[1] == ("second", {"x": PrevRef("[99999999999999999999]")})


_add("P108", "renders", _check_p108)


# -- P109: the renderer never emits a \\uXXXX escape (raw unicode instead),
#          so it can never emit an incomplete one --------------------------
def _check_p109():
    value = "café ☕"
    rendered, parsed = _rt("verb", note=value)
    assert "\\u" not in rendered
    assert value in rendered
    assert parsed["note"] == value


_add("P109", "renders", _check_p109)


# -- P110: the no-$prev rule is applied recursively for structured data ----
def _check_p110():
    _rendered, parsed = _rt("verb", tags=["$prev"], properties={"note": "$prev.id"})
    assert parsed["tags"] == ["$prev"]
    assert parsed["properties"] == {"note": "$prev.id"}
    # Neither nested value became a reference: both decode back to the exact
    # literal text the caller supplied.


_add("P110", "renders", _check_p110)


# -- P111: only ASCII whitespace separates DSL tokens ------------------------
def _check_p111():
    rendered = render_dsl([op("a"), op("b"), op("c")], chained=True)
    for ch in rendered:
        if ch in (" ", "\t", "\n", "\r"):
            assert ch == " "


_add("P111", "renders", _check_p111)


# -- P112: reject_reserved_args runs identically at every call site ---------
def _check_p112():
    _refuses([op("verb", presentation="table")], match="presentation")
    _refuses([op("verb", presentation_per_op="table")], match="presentation_per_op")
    _rendered, parsed = _rt("verb", note="x")
    assert parsed == {"note": "x"}


_add("P112", "refuses", _check_p112)


# -- P113: an object literal reaching EOF while a key is expected is refused,
#          distinct from the empty-object accept at P76 ---------------------
def _check_p113():
    # The request must end right after the `{` to reach the `None` arm this
    # rule describes; the row names its variant, so the fake pins the class.
    with pytest.raises(DslParseError) as exc_info:
        parse_dsl("v(a={")
    assert exc_info.value.variant == "UnexpectedEof"
    # The near cases: a present byte after `{` — the call's own `)`, or a
    # `,` — takes the `Some(c)` arm the row sets this rule apart from, so
    # neither may report the end-of-input class. The row does not name that
    # arm's variant, so nothing more is pinned here; the real parser's own
    # suite (`crates/khive-request/tests/parser.rs`) pins both shapes.
    for near in ("v(a={)", "v(a={, b=1)"):
        with pytest.raises(DslParseError) as exc_info:
            parse_dsl(near)
        assert exc_info.value.variant != "UnexpectedEof", near


_add("P113", "fake", _check_p113)


# -- P114: past NESTING_DEPTH_LIMIT, json_value_contains_prev_ref_at treats a
#          nested JSON value as conservatively containing a $prev reference —
#          a json-mode-only concern the renderer never reaches ------------
_add("P114", "not_emitted", _generic_not_emitted_check, reason=_JSON_FORM_NOT_EMITTED_REASON)


# -- P115: value_nesting_within_limit applies the same depth-exceeded
#          rejection to both Array and Object — a json-mode-only concern --
_add("P115", "not_emitted", _generic_not_emitted_check, reason=_JSON_FORM_NOT_EMITTED_REASON)


# -- P116: an unmatched closing brace inside an unquoted scalar/value slice,
#          while a different local bracket/paren is still open, is refused
#          immediately, distinct from P106's trailing check ----------------
def _check_p116():
    # `v(a=1})` alone reaches `UnexpectedChar` instead (no local container is
    # open when the top-level `}` is hit, so `scan_value_end` treats it as an
    # ordinary value boundary and the arg-list loop rejects the leftover `}`)
    # — the `[` here is what keeps a local container open through the `}`,
    # reaching this rule's `UnclosedBracket` outcome.
    with pytest.raises(DslParseError) as exc_info:
        parse_dsl("v(a=1[})")
    assert exc_info.value.variant == "UnclosedBracket"
    # The row also names the no-container case: the same `}` ends the value
    # and the arg-list loop rejects it as an unexpected character.
    with pytest.raises(DslParseError) as exc_info:
        parse_dsl("v(a=1})")
    assert exc_info.value.variant == "UnexpectedChar"
    # `v(a=1["x}"])` looks similar at a glance, but the `}` sits inside a
    # quoted span, which `scan_value_end` consumes whole before ever
    # inspecting bracket depth — the `'}'` arm this rule describes is never
    # reached, so the fake must not report its class for this shape. The
    # row names no variant for it; the real parser's own suite pins one.
    with pytest.raises(DslParseError) as exc_info:
        parse_dsl('v(a=1["x}"])')
    assert exc_info.value.variant != "UnclosedBracket"


_add("P116", "fake", _check_p116)


# -- P117: split_path resolves dotted text as Field lookups and non-usize
#          bracket text as an always-miss Malformed segment, at chain
#          runtime resolution, after the parser has already accepted the
#          reference (same boundary as P108) -------------------------------
def _check_p117():
    """`split_path`'s Field/Malformed classification runs after parsing, at
    resolution time; this offline fake has no mirror of `split_path` or
    `apply_path_segment` (no Python port exists), so only the
    renderer/parser-observable half of the obligation is assertable here:
    dot-separated field text and bracket index text both pass through the
    renderer and the fake's grammar-level parse unchanged. The Field/
    Malformed resolution split itself is instead pinned by
    `path::tests::dotted_field_segments_resolve_by_key_or_miss` and
    `path::tests::oversized_bracket_index_is_malformed_and_always_misses` in
    `crates/khive-request/src/parser/path.rs`.
    """
    text = 'first() | second(x="$prev.a.b")'
    rendered = render_dsl(text)
    parsed = parse_dsl(rendered)
    assert parsed[1] == ("second", {"x": PrevRef("a.b")})

    oversized_text = "first() | second(x=$prev[99999999999999999999])"
    rendered_oversized = render_dsl(oversized_text)
    parsed_oversized = parse_dsl(rendered_oversized)
    assert parsed_oversized[1] == ("second", {"x": PrevRef("[99999999999999999999]")})


_add("P117", "renders", _check_p117)


# -- P118: an integer literal outside [-2**63, 2**64 - 1] is not a
#          parser-time rejection — it silently decodes as f64 on the wire;
#          render_dsl refuses it instead of changing its type -------------
def _check_p118():
    max_u64 = 2**64 - 1
    min_i64 = -(2**63)

    _rendered_ok, parsed_ok = _rt("verb", big=max_u64, small=min_i64)
    assert parsed_ok == {"big": max_u64, "small": min_i64}

    with pytest.raises(TransportError, match="outside"):
        render_dsl([op("verb", big=max_u64 + 1)])
    with pytest.raises(TransportError, match="outside"):
        render_dsl([op("verb", small=min_i64 - 1)])

    # Hand-built without the renderer, to prove the fake mirrors serde_json's
    # default-feature decode: an out-of-range integer literal is not
    # rejected, it is silently converted to a float on the wire.
    hand_built = f"verb(big={max_u64 + 1})"
    [(_verb, parsed)] = parse_dsl(hand_built)
    assert parsed["big"] == float(max_u64 + 1)
    assert isinstance(parsed["big"], float)


_add("P118", "refuses", _check_p118)


def test_every_rule_id_has_exactly_one_case():
    assert set(CASES) == {r.id for r in RULES}


def _rule_sort_key(rule_id: str) -> int:
    return int(rule_id[1:])


@pytest.mark.parametrize("rule_id", sorted(CASES, key=_rule_sort_key))
def test_contract_case(rule_id: str):
    CASES[rule_id].check()

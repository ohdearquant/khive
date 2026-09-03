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

from khive.dsl import render_dsl
from khive.errors import TransportError
from khive.ops import op

REPO_ROOT = Path(__file__).resolve().parents[2]
DOC_PATH = Path(__file__).resolve().parents[1] / "docs" / "DSL_WIRE_CONTRACT.md"
DOC_TEXT = DOC_PATH.read_text()

_ID_RE = re.compile(r"^P\d+$")
_CITE_RE = re.compile(r'([\w./\-]+) — "((?:\\.|[^"\\])*)"')


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
    citations: tuple[tuple[str, str], ...]  # (path, quoted-line-text)


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
        citations = tuple((m.group(1), _unescape(m.group(2))) for m in _CITE_RE.finditer(site_cell))
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
    # happens to be contiguous.
    assert len(RULES) == 111


@pytest.mark.skipif(
    not (REPO_ROOT / "crates" / "khive-request").exists(),
    reason="crates/khive-request is not present in this checkout",
)
def test_every_cited_line_exists_in_the_parser_source():
    failures: list[str] = []
    file_cache: dict[str, str | None] = {}
    for rule in RULES:
        for path, quoted in rule.citations:
            if path not in file_cache:
                try:
                    file_cache[path] = (REPO_ROOT / path).read_text()
                except FileNotFoundError:
                    file_cache[path] = None
            content = file_cache[path]
            if content is None:
                failures.append(f"{rule.id}: file not found: {path}")
                continue
            if not any(quoted in line for line in content.splitlines()):
                failures.append(f"{rule.id}: {path!r} has no line containing {quoted!r}")
    assert not failures, "\n".join(failures)


# ---------------------------------------------------------------------------
# CASES: one entry per rule id, exercising `render_dsl` (or the fake parser,
# for rules the renderer cannot reach) against the doc's stated obligation.
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Case:
    kind: str  # "renders" | "refuses" | "fake" | "not_emitted"
    check: Callable[[], None]


CASES: dict[str, Case] = {}


def _add(rule_id: str, kind: str, check: Callable[[], None]) -> None:
    assert kind in ("renders", "refuses", "fake", "not_emitted")
    assert rule_id not in CASES
    CASES[rule_id] = Case(kind=kind, check=check)


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


# -- P1: input must not render to nothing -----------------------------------
def _check_p1():
    rendered = render_dsl([op("whoami")])
    assert rendered.strip() != ""
    assert parse_dsl(rendered) == [("whoami", {})]


_add("P1", "renders", _check_p1)


# -- P2: InputTooLarge (1 MiB) — the renderer has no size cap ----------------
def _check_p2():
    # Finding: rule P2 — a single huge string argument — render_dsl emits it
    # in full with no 1 MiB (or any) length check; the cloud parser would
    # reject the resulting request text once it exceeds MAX_OPS_INPUT_LEN.
    huge = "x" * (1024 * 1024 + 1)
    rendered = render_dsl([op("verb", note=huge)])
    assert len(rendered) > 1024 * 1024


_add("P2", "fake", _check_p2)


def _generic_not_emitted_check():
    rendered = render_dsl([op("stats"), op("whoami")])
    assert not rendered.startswith("{")
    if rendered.startswith("["):
        inner = rendered[1:].lstrip(" \t\n\r")
        assert not inner.startswith("{")


for _pid in (
    "P3",
    "P4",
    "P5",
    *[f"P{i}" for i in range(13, 20)],
    *[f"P{i}" for i in range(27, 40)],
):
    _add(_pid, "not_emitted", _generic_not_emitted_check)


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
    # Finding: rule P8 — render_dsl(["update(id=$prev.id)"]) — a raw
    # passthrough DSL string is rendered verbatim in single mode with no
    # check that it lacks a $prev reference; the real parser rejects this
    # exact text with PrevRefOutsideChain.
    rendered = render_dsl(["update(id=$prev.id)"])
    assert rendered == "update(id=$prev.id)"
    with pytest.raises(DslParseError):
        parse_dsl(rendered)


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


# -- P21: chains longer than 100 — the renderer has no cap -------------------
def _check_p21():
    # Finding: rule P21 — a 101-op chain — render_dsl joins all 101 calls
    # with " | " with no TooManyOps-style cap; the parser rejects a chain
    # this long.
    ops = [op("v", i=i) for i in range(101)]
    rendered = render_dsl(ops, chained=True)
    assert rendered.count(" | ") == 100


_add("P21", "fake", _check_p21)


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


# -- P25: a chain never mixes in a top-level comma ---------------------------
def _check_p25():
    rendered = render_dsl([op("v", tags=["a", "b"]), op("w")], chained=True)
    # The only commas in a chain must be inside a balanced container.
    top_level = rendered.split(" | ")
    assert len(top_level) == 2
    assert "," not in top_level[1]


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


# -- P42: batches over 100 ops — the renderer has no cap ---------------------
def _check_p42():
    # Finding: rule P42 — a 101-op parallel batch — render_dsl emits all 101
    # calls with no TooManyOps-style cap; the parser rejects a batch this
    # long.
    ops = [op("v", i=i) for i in range(101)]
    rendered = render_dsl(ops)
    assert rendered.count(",") == 100


_add("P42", "fake", _check_p42)


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
    # Finding: rule P51 — render_dsl(["update(id=$prev.id)", "other()"]) — a
    # raw passthrough element is rendered verbatim inside a parallel batch
    # with no check that it lacks a $prev reference; the real parser rejects
    # this exact text with PrevRefOutsideChain.
    rendered = render_dsl(["update(id=$prev.id)", "other()"])
    assert rendered == "[update(id=$prev.id), other()]"
    with pytest.raises(DslParseError):
        parse_dsl(rendered)


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


# -- P69: function-arg array/object nesting past depth 64 — no renderer cap -
def _make_nested_list(depth: int) -> Any:
    value: Any = 0
    for _ in range(depth):
        value = [value]
    return value


def _check_p69():
    # Finding: rule P69 — a 70-level-deep nested list argument — render_dsl
    # emits it in full with no NestingTooDeep-style cap; the parser rejects
    # function-form argument nesting past depth 64.
    deep = _make_nested_list(70)
    rendered = render_dsl([op("verb", data=deep)])
    assert rendered.count("[") >= 70


_add("P69", "fake", _check_p69)


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


# -- P75: object nesting past depth 64 — no renderer cap ---------------------
def _make_nested_obj(depth: int) -> Any:
    value: Any = 0
    for _ in range(depth):
        value = {"a": value}
    return value


def _check_p75():
    # Finding: rule P75 — a 70-level-deep nested object argument —
    # render_dsl emits it in full with no NestingTooDeep-style cap.
    deep = _make_nested_obj(70)
    rendered = render_dsl([op("verb", data=deep)])
    assert rendered.count("{") >= 70


_add("P75", "fake", _check_p75)


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
    # Finding: rule P89 — render_dsl("first() | second(x=$prev[])") — a
    # passthrough chain is not validated by the renderer; the fake (mirroring
    # the real parser) rejects the malformed index it forwards unchanged.
    rendered = render_dsl("first() | second(x=$prev[])")
    assert rendered == "first() | second(x=$prev[])"
    with pytest.raises(DslParseError):
        parse_dsl(rendered)


_add("P89", "fake", _check_p89)


# -- P90: an index must close immediately after its digits -------------------
def _check_p90():
    # Finding: rule P90 — render_dsl("first() | second(x=$prev[0x])") — same
    # passthrough gap as P89, for a malformed index terminator.
    rendered = render_dsl("first() | second(x=$prev[0x])")
    with pytest.raises(DslParseError):
        parse_dsl(rendered)


_add("P90", "fake", _check_p90)


# -- P91: no unparsed punctuation trails a bare path -------------------------
def _check_p91():
    # Finding: rule P91 — render_dsl("first() | second(x=$prev.foo-bar)") —
    # same passthrough gap; the fake rejects the trailing punctuation the
    # renderer forwarded unchanged.
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
    rendered, parsed = _rt("verb", hard=True, soft=False, kind="x", missing=None)
    assert "true" in rendered and "false" in rendered
    assert "null" not in rendered  # `kind=None` never reaches _rt's kwargs as None here
    assert parsed == {"hard": True, "soft": False, "kind": "x"}


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


# -- P108: runtime index resolution is beyond the parser ---------------------
def _check_p108():
    """Reference resolution (turning `$prev[0]` into an actual lookup) is a
    runtime concern that starts only after the parser has accepted the
    reference; this case only pins that the renderer emits plain ASCII-digit
    indices for the resolver to consume."""
    text = "first() | second(x=$prev[12])"
    rendered = render_dsl(text)
    m = re.search(r"\$prev\[(\d+)\]", rendered)
    assert m and m.group(1).isascii() and m.group(1).isdigit()


_add("P108", "not_emitted", _check_p108)


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


def test_every_rule_id_has_exactly_one_case():
    assert set(CASES) == {r.id for r in RULES}


def _rule_sort_key(rule_id: str) -> int:
    return int(rule_id[1:])


@pytest.mark.parametrize("rule_id", sorted(CASES, key=_rule_sort_key))
def test_contract_case(rule_id: str):
    CASES[rule_id].check()

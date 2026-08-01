#!/usr/bin/env python3
"""Validate published verb/pack count claims against ``verbs()`` output.

Merged ADRs are historical records and intentionally excluded. The scanner
covers the living documentation surfaces shipped from this repository,
including CLI help/goldens, pack rustdoc comments, and the Pages-generated
``llms.txt`` summary.
"""

from __future__ import annotations

from collections import Counter
from dataclasses import dataclass
from pathlib import Path
import re
from typing import Iterable


NUMBER_WORDS = {
    "zero": 0,
    "one": 1,
    "two": 2,
    "three": 3,
    "four": 4,
    "five": 5,
    "six": 6,
    "seven": 7,
    "eight": 8,
    "nine": 9,
    "ten": 10,
    "eleven": 11,
    "twelve": 12,
    "thirteen": 13,
    "fourteen": 14,
    "fifteen": 15,
    "sixteen": 16,
    "seventeen": 17,
    "eighteen": 18,
    "nineteen": 19,
    "twenty": 20,
}
COUNT_TOKEN = rf"(?:\d+|{'|'.join(NUMBER_WORDS)})"
VERB_COUNT_MODIFIERS = r"(?:(?:public|agent-facing|production|mcp-callable)\s+)*"
PACK_COUNT_MODIFIERS = r"(?:(?:built-in|default(?:-loaded)?|loaded|production)\s+)*"
VERB_COUNT_RE = re.compile(
    rf"\b(?P<count>{COUNT_TOKEN})\s*(?:-\s*|\s+)"
    rf"{VERB_COUNT_MODIFIERS}verbs?(?:\s+handlers?)?\b",
    re.IGNORECASE,
)
INVERTED_VERB_COUNT_RE = re.compile(
    rf"\bverbs?\s*(?::|at)\s*(?P<count>{COUNT_TOKEN})\b",
    re.IGNORECASE,
)
PACK_COUNT_RE = re.compile(
    rf"\b(?P<count>{COUNT_TOKEN})\s*(?:-\s*|\s+){PACK_COUNT_MODIFIERS}packs?\b",
    re.IGNORECASE,
)
CLAUSE_BOUNDARY_RE = re.compile(r"(?:;|[.!?](?=\s|$))")
INVERTED_PACK_COUNT_RE = re.compile(
    rf"\bpacks\s*:\s*(?P<count>{COUNT_TOKEN})\b",
    re.IGNORECASE,
)
LOADS_ALL_RE = re.compile(
    rf"\b(?:loads?|loading|includes?)\s+all\s+(?P<count>{COUNT_TOKEN})\b",
    re.IGNORECASE,
)
BEYOND_LOADED_RE = re.compile(
    rf"\bpacks?\s+beyond\s+the\s+(?P<count>{COUNT_TOKEN})\s+loaded\b",
    re.IGNORECASE,
)
HANDLER_ENTRY_RE = re.compile(
    rf"\b(?P<count>{COUNT_TOKEN})\s+(?:public\s+verbs?\s+)?entries\b",
    re.IGNORECASE,
)
HANDLER_COUNT_RE = re.compile(
    rf"\b(?P<count>{COUNT_TOKEN})\s+(?:public\s+)?handlers?(?![-\w])",
    re.IGNORECASE,
)


@dataclass(frozen=True)
class RegistryCounts:
    total_verbs: int
    pack_verbs: dict[str, int]

    @property
    def total_packs(self) -> int:
        return len(self.pack_verbs)


@dataclass(frozen=True)
class CountClaim:
    path: str
    line: int
    kind: str
    value: int
    text: str
    form: str
    pack: str | None = None


def registry_counts(verbs_result: object) -> RegistryCounts:
    if not isinstance(verbs_result, dict):
        raise ValueError("verbs() result must be an object")
    verbs = verbs_result.get("verbs")
    total = verbs_result.get("total")
    pack_counts = verbs_result.get("pack_counts")
    if not isinstance(verbs, list) or not isinstance(total, int):
        raise ValueError("verbs() result must contain list 'verbs' and integer 'total'")
    if not isinstance(pack_counts, dict):
        raise ValueError("verbs() result must contain object 'pack_counts'")
    if len(verbs) != total:
        raise ValueError(f"verbs().total={total} but the catalog has {len(verbs)} entries")

    parsed_pack_counts: dict[str, int] = {}
    for pack, count in pack_counts.items():
        if not isinstance(pack, str) or not isinstance(count, int) or count < 0:
            raise ValueError(f"invalid verbs().pack_counts entry: {pack!r}: {count!r}")
        parsed_pack_counts[pack.lower()] = count

    observed = Counter()
    for entry in verbs:
        if not isinstance(entry, dict) or not isinstance(entry.get("pack"), str):
            raise ValueError(f"verbs() catalog entry lacks a string pack: {entry!r}")
        observed[entry["pack"].lower()] += 1
    for pack, count in parsed_pack_counts.items():
        if observed[pack] != count:
            raise ValueError(
                f"verbs().pack_counts[{pack!r}]={count}, catalog contains {observed[pack]}"
            )
    unknown = sorted(set(observed) - set(parsed_pack_counts))
    if unknown:
        raise ValueError(f"verbs().pack_counts omits catalog packs: {unknown}")
    if sum(parsed_pack_counts.values()) != total:
        raise ValueError(
            "verbs().pack_counts sum "
            f"{sum(parsed_pack_counts.values())} does not equal total {total}"
        )
    return RegistryCounts(total_verbs=total, pack_verbs=parsed_pack_counts)


def _number(token: str) -> int:
    lowered = token.lower()
    return int(lowered) if lowered.isdigit() else NUMBER_WORDS[lowered]


def _plain(line: str) -> str:
    return line.replace("—", "-").replace("–", "-").translate(
        str.maketrans("", "", "`*_")
    )


def _pack_from_path(path: str, pack_names: set[str]) -> str | None:
    match = re.search(r"(?:^|/)crates/khive-pack-([^/]+)(?:/|$)", path)
    if match and match.group(1).lower() in pack_names:
        return match.group(1).lower()
    match = re.search(r"(?:^|/)marketplace/khive/skills/([^/]+)/SKILL\.md$", path)
    if match and match.group(1).lower() in pack_names:
        return match.group(1).lower()
    return None


def _context_pack(
    context: str,
    pack_names: set[str],
    count_span: tuple[int, int],
    *,
    local_clause_only: bool = False,
) -> str | None:
    plain = _plain(context)
    count_start, count_end = count_span
    scope_start, scope_end = 0, len(plain)
    if local_clause_only:
        for boundary in CLAUSE_BOUNDARY_RE.finditer(plain):
            if boundary.end() <= count_start:
                scope_start = boundary.end()
            elif boundary.start() >= count_end:
                scope_end = boundary.start()
                break
    candidates: list[tuple[int, bool, int, str]] = []
    for pack in pack_names:
        marker = re.compile(
            rf"(?:khive-pack-{re.escape(pack)}\b|"
            rf"(?<![\w.-]){re.escape(pack)}(?![\w.-])"
            rf"(?:\s*\([^)]*\))?\s+pack\b|"
            rf"(?<![\w.-]){re.escape(pack)}(?![\w.-])"
            rf"(?:\s*\([^)]*\))?\s+"
            rf"(?:adds?|contributes?|dispatches?|exposes?|implements?|provides?|registers?|ships?)|"
            rf"\bpack\s*=\s*\\?[\"']?{re.escape(pack)}\\?[\"']?)",
            re.IGNORECASE,
        )
        for match in marker.finditer(plain):
            if match.start() < scope_start or match.end() > scope_end:
                continue
            if match.end() <= count_start:
                distance = count_start - match.end()
                follows_count = False
            elif match.start() >= count_end:
                distance = match.start() - count_end
                follows_count = True
            else:
                distance = 0
                follows_count = False
            candidates.append((distance, follows_count, match.start(), pack))
    if not candidates:
        return None
    return min(candidates)[3]


def _is_reference_count(plain: str, match: re.Match[str]) -> bool:
    prefix = plain[max(0, match.start() - 12) : match.start()]
    return re.search(r"(?:ADR|PR|issue|#)[ -]?$", prefix, re.IGNORECASE) is not None


def _is_pack_total_context(plain: str, match: re.Match[str]) -> bool:
    lowered = plain.lower()
    if "amendment" in lowered and re.search(r"\badds?(?:\s+exactly)?\b", lowered):
        return False
    if re.search(r"\b(?:surface|pack|handlers?|catalog|descriptors?|all)\b", lowered):
        return True
    if re.search(
        r"\b(?:add|contribut|provid|implement|dispatch|register|ship|expos)\w*\b",
        lowered,
    ):
        return True
    suffix = plain[match.end() : match.end() + 4]
    return ":" in suffix or "-" in suffix


def _has_explicit_aggregate_verb_context(plain: str) -> bool:
    lowered = plain.lower()
    if any(
        cue in lowered
        for cue in (
            "across",
            "runtime",
            "catalog",
            "mcp tool",
            "out of the box",
            "aggregate",
        )
    ):
        return True
    return PACK_COUNT_RE.search(plain) is not None and any(
        cue in lowered
        for cue in ("default", "load by default", "loaded by default", "production")
    )


def _is_aggregate_verb_context(plain: str, match: re.Match[str]) -> bool:
    if match.re is INVERTED_VERB_COUNT_RE:
        return True
    return _has_explicit_aggregate_verb_context(plain)


def _is_aggregate_pack_context(plain: str, match: re.Match[str]) -> bool:
    if match.re is INVERTED_PACK_COUNT_RE:
        return True
    lowered = plain.lower()
    across = lowered.find("across")
    if 0 <= across < match.start():
        return True
    verb_match = VERB_COUNT_RE.search(plain)
    if verb_match is not None and _is_aggregate_verb_context(plain, verb_match):
        return True
    return any(
        cue in lowered
        for cue in (
            "default",
            "production",
            "loaded",
            "loads",
            "load ",
            "force-linked",
        )
    )


def _claim(
    path: str,
    line_number: int,
    kind: str,
    value: int,
    text: str,
    form: str,
    pack: str | None = None,
) -> CountClaim:
    return CountClaim(path, line_number, kind, value, text.strip(), form, pack)


def scan_document(path: str, text: str, pack_names: Iterable[str]) -> list[CountClaim]:
    if path.startswith("docs/adr/") or "/docs/adr/" in path:
        return []
    names = {name.lower() for name in pack_names}
    path_pack = _pack_from_path(path, names)
    claims: list[CountClaim] = []
    pack_table_columns: tuple[int, int] | None = None

    lines = text.splitlines()
    for line_number, raw in enumerate(lines, 1):
        stripped = raw.strip()
        if path.endswith(".rs") and not stripped.startswith(("//!", "///", "#[doc")):
            continue
        next_raw = lines[line_number] if line_number < len(lines) else ""
        if path.endswith(".rs") and not next_raw.strip().startswith(("//!", "///", "#[doc")):
            next_raw = ""
        raw_window = f"{raw} {next_raw}" if next_raw else raw
        plain = _plain(raw_window)
        current_line_end = len(_plain(raw))
        preceding_context = "\n".join(
            lines[max(0, line_number - 3) : line_number - 1]
        )
        context = f"{preceding_context}\n{raw}" if preceding_context else raw
        context_offset = len(_plain(preceding_context)) + (1 if preceding_context else 0)

        def starts_on_current_line(match: re.Match[str]) -> bool:
            return match.start() <= current_line_end

        def claim_text(match: re.Match[str]) -> str:
            return raw_window if match.end() > current_line_end else raw

        if stripped.startswith("|"):
            cells = [
                re.sub(r"[`*_]", "", cell).strip()
                for cell in stripped.strip("|").split("|")
            ]
            lowered_cells = [cell.lower() for cell in cells]
            pack_column = next(
                (index for index, cell in enumerate(lowered_cells) if cell == "pack"),
                None,
            )
            verbs_column = next(
                (
                    index
                    for index, cell in enumerate(lowered_cells)
                    if re.fullmatch(r"(?:public\s+)?verbs?", cell)
                ),
                None,
            )
            if pack_column is not None and verbs_column is not None:
                pack_table_columns = (pack_column, verbs_column)
                continue
            if pack_table_columns is not None:
                pack_column, verbs_column = pack_table_columns
                if max(pack_column, verbs_column) >= len(cells):
                    continue
                pack = lowered_cells[pack_column]
                count = lowered_cells[verbs_column]
                if pack in names and re.fullmatch(COUNT_TOKEN, count, re.IGNORECASE):
                    claims.append(
                        _claim(
                            path,
                            line_number,
                            "pack_verbs",
                            _number(count),
                            raw,
                            "per-pack-table",
                            pack,
                        )
                    )
                continue
        elif pack_table_columns is not None:
            pack_table_columns = None

        matched_spans: set[tuple[int, int]] = set()
        verb_matches = list(VERB_COUNT_RE.finditer(plain)) + list(
            INVERTED_VERB_COUNT_RE.finditer(plain)
        )
        if names:
            named_count_re = re.compile(
                rf"\b(?P<count>{COUNT_TOKEN})\s+{VERB_COUNT_MODIFIERS}"
                rf"(?:{'|'.join(map(re.escape, sorted(names)))})(?:\.)?\s+"
                rf"{VERB_COUNT_MODIFIERS}verbs?\b",
                re.IGNORECASE,
            )
            verb_matches.extend(named_count_re.finditer(plain))
        for match in verb_matches:
            if not starts_on_current_line(match):
                continue
            if match.span() in matched_spans:
                continue
            if _is_reference_count(plain, match):
                continue
            matched_spans.add(match.span())
            value = _number(match.group("count"))
            count_span = (
                context_offset + match.start(),
                context_offset + match.end(),
            )
            clause_pack = _context_pack(
                context,
                names,
                count_span,
                local_clause_only=True,
            )
            pack = clause_pack or path_pack or _context_pack(context, names, count_span)
            claim_plain = _plain(claim_text(match))
            pack_context = (
                claim_plain
                if path_pack is not None and clause_pack is None
                else _plain(context)
            )
            unqualified_path_inverted = (
                path_pack is not None
                and match.re is INVERTED_VERB_COUNT_RE
                and not _has_explicit_aggregate_verb_context(claim_plain)
            )
            if pack is not None and (
                unqualified_path_inverted or _is_pack_total_context(pack_context, match)
            ):
                claims.append(
                    _claim(
                        path,
                        line_number,
                        "pack_verbs",
                        value,
                        claim_text(match),
                        "per-pack-window",
                        pack,
                    )
                )
            elif _is_aggregate_verb_context(plain, match):
                form = "inverted" if match.re is INVERTED_VERB_COUNT_RE else (
                    "hyphenated" if "-" in match.group(0) else "spaced"
                )
                claims.append(
                    _claim(
                        path,
                        line_number,
                        "total_verbs",
                        value,
                        claim_text(match),
                        form,
                    )
                )

        if path_pack is not None and "handlers" in plain.lower():
            handler_matches = list(HANDLER_ENTRY_RE.finditer(plain)) + list(
                HANDLER_COUNT_RE.finditer(plain)
            )
            for match in handler_matches:
                if not starts_on_current_line(match):
                    continue
                if _is_reference_count(plain, match):
                    continue
                if match.re is HANDLER_COUNT_RE and not _is_pack_total_context(
                    _plain(context), match
                ):
                    continue
                claims.append(
                    _claim(
                        path,
                        line_number,
                        "pack_verbs",
                        _number(match.group("count")),
                        claim_text(match),
                        "per-pack-window",
                        path_pack,
                    )
                )

        pack_matches = list(PACK_COUNT_RE.finditer(plain)) + list(
            INVERTED_PACK_COUNT_RE.finditer(plain)
        ) + list(BEYOND_LOADED_RE.finditer(plain))
        for match in pack_matches:
            if not starts_on_current_line(match):
                continue
            if _is_reference_count(plain, match) or not _is_aggregate_pack_context(plain, match):
                continue
            if "--packs" in plain and "production" not in plain.lower():
                continue
            form = "inverted" if match.re is INVERTED_PACK_COUNT_RE else (
                "hyphenated" if "-" in match.group(0) else "spaced"
            )
            claims.append(
                _claim(
                    path,
                    line_number,
                    "total_packs",
                    _number(match.group("count")),
                    claim_text(match),
                    form,
                )
            )

        for match in LOADS_ALL_RE.finditer(plain):
            if not starts_on_current_line(match):
                continue
            if any(existing.line == line_number and existing.kind == "total_packs" for existing in claims):
                continue
            context = plain.lower()
            if any(word in context for word in ("default", "server", "config", "install", "production")):
                claims.append(
                    _claim(
                        path,
                        line_number,
                        "total_packs",
                        _number(match.group("count")),
                        claim_text(match),
                        "spelled",
                    )
                )

    deduped: list[CountClaim] = []
    seen: set[tuple[str, int, str, int, str | None]] = set()
    for claim in claims:
        key = (claim.path, claim.line, claim.kind, claim.value, claim.pack)
        if key not in seen:
            seen.add(key)
            deduped.append(claim)
    return deduped


def _published_files(repo_root: Path, pack_names: Iterable[str]) -> list[Path]:
    paths: set[Path] = set()
    for name in (
        "README.md",
        "AGENTS.md",
        "CLAUDE.md",
        "npm/README.md",
        "cli/main.ts",
        "cli/tests/golden/help_toplevel.txt",
    ):
        candidate = repo_root / name
        if candidate.is_file():
            paths.add(candidate)
    for pattern in (
        "docs/**/*.md",
        "marketplace/**/*.md",
        "scripts/**/*.md",
        "crates/**/README.md",
        "crates/**/docs/**/*.md",
    ):
        paths.update(path for path in repo_root.glob(pattern) if path.is_file())
    pages_workflow = repo_root / ".github/workflows/pages.yml"
    if pages_workflow.is_file():
        paths.add(pages_workflow)
    for pack in pack_names:
        source_root = repo_root / f"crates/khive-pack-{pack}/src"
        if source_root.is_dir():
            paths.update(source_root.rglob("*.rs"))
    return sorted(
        path
        for path in paths
        if not path.relative_to(repo_root).as_posix().startswith("docs/adr/")
    )


def scan_repository(repo_root: Path, counts: RegistryCounts) -> list[CountClaim]:
    claims: list[CountClaim] = []
    for path in _published_files(repo_root, counts.pack_verbs):
        relative = path.relative_to(repo_root).as_posix()
        claims.extend(
            scan_document(relative, path.read_text(encoding="utf-8"), counts.pack_verbs)
        )
    return claims


def validate_documented_counts(repo_root: Path, verbs_result: object) -> list[str]:
    counts = registry_counts(verbs_result)
    claims = scan_repository(repo_root, counts)
    errors: list[str] = []
    for claim in claims:
        if claim.kind == "total_verbs":
            actual = counts.total_verbs
        elif claim.kind == "total_packs":
            actual = counts.total_packs
        else:
            actual = counts.pack_verbs[claim.pack or ""]
        if claim.value != actual:
            subject = f"{claim.pack} verbs" if claim.pack else claim.kind.replace("_", " ")
            errors.append(
                f"{claim.path}:{claim.line}: claims {subject}={claim.value}, "
                f"registry says {actual}: {claim.text}"
            )
    kinds = {claim.kind for claim in claims}
    if "total_verbs" not in kinds:
        errors.append("published docs contain no aggregate verb-count claim")
    if "total_packs" not in kinds:
        errors.append("published docs contain no aggregate pack-count claim")
    missing_packs = sorted(set(counts.pack_verbs) - {c.pack for c in claims if c.kind == "pack_verbs"})
    if missing_packs:
        errors.append(f"published docs contain no per-pack verb claim for: {missing_packs}")
    return errors

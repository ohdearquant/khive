"""Shared row tokenizer for the published Markdown documentation checks."""

from __future__ import annotations


def split_row(line: str) -> tuple[str, ...]:
    """Split one Markdown table row on its unescaped pipes; ``\\|`` stays in its cell."""
    body = line.strip()
    if not (body.startswith("|") and body.endswith("|")) or len(body) < 2:
        raise ValueError(f"not a table row: {line!r}")
    cells: list[str] = []
    current: list[str] = []
    index = 1
    while index < len(body) - 1:
        char = body[index]
        if char == "\\" and body[index + 1] == "|":
            current.append("|")
            index += 2
            continue
        if char == "|":
            cells.append("".join(current).strip())
            current = []
        else:
            current.append(char)
        index += 1
    cells.append("".join(current).strip())
    return tuple(cells)

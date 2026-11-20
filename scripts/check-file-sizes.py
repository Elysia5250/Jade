#!/usr/bin/env python3
"""Fail when a Rust source file grows past the harness limits.

Test modules, `tests*.rs`, and `tests/` trees are not counted. Generated
output and `target/` are skipped. The hard limit is 700 lines; the soft
limit of 400 is a warning and does not fail the check.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SOFT_LIMIT = 400
HARD_LIMIT = 700
CFG_TEST = re.compile(
    r"#\[\s*cfg\(\s*(?:test\b|all\(\s*test\b|any\(\s*test\b)"
)


def rust_files() -> list[Path]:
    files: list[Path] = []
    base = ROOT / "crates"
    if not base.exists():
        return files
    for path in base.rglob("*.rs"):
        parts = set(path.parts)
        if "target" in parts or "generated" in parts or "tests" in parts:
            continue
        if path.name.startswith("tests"):
            continue
        files.append(path)
    return sorted(files)


def is_cfg_test(line: str) -> bool:
    stripped = line.strip()
    if not stripped.startswith("#["):
        return False
    if "not(test)" in stripped:
        return False
    return CFG_TEST.search(stripped) is not None


def skip_cfg_test_item(lines: list[str], start: int) -> int:
    """Return the index just after a `#[cfg(test)]` item that starts at `start`."""
    i = start
    depth = 0
    seen_brace = False
    in_string = False
    escaped = False
    in_block = False
    while i < len(lines):
        line = lines[i]
        j = 0
        in_line = False
        while j < len(line):
            ch = line[j]
            nxt = line[j + 1] if j + 1 < len(line) else ""
            if in_line:
                break
            if in_block:
                if ch == "*" and nxt == "/":
                    in_block = False
                    j += 2
                    continue
                j += 1
                continue
            if in_string:
                if escaped:
                    escaped = False
                elif ch == "\\":
                    escaped = True
                elif ch == '"':
                    in_string = False
                j += 1
                continue
            if ch == "/" and nxt == "/":
                in_line = True
                break
            if ch == "/" and nxt == "*":
                in_block = True
                j += 2
                continue
            if ch == '"':
                in_string = True
                escaped = False
                j += 1
                continue
            if ch == "{":
                depth += 1
                seen_brace = True
            elif ch == "}" and depth:
                depth -= 1
            j += 1
        i += 1
        if i == start + 1 and not seen_brace:
            # Attribute line itself. Keep scanning into the item.
            continue
        if seen_brace and depth == 0 and not in_string and not in_block:
            return i
        if (
            not seen_brace
            and not in_string
            and not in_block
            and i > start + 1
            and line.strip().endswith(";")
        ):
            return i
    return i


def counted_lines(text: str) -> int:
    lines = text.splitlines()
    count = 0
    index = 0
    while index < len(lines):
        if is_cfg_test(lines[index]):
            index = skip_cfg_test_item(lines, index)
            continue
        count += 1
        index += 1
    return count


def line_count(path: Path) -> int:
    return counted_lines(path.read_text(encoding="utf-8", errors="replace"))


def main() -> int:
    errors: list[str] = []
    warnings: list[str] = []
    for path in rust_files():
        rel = path.relative_to(ROOT).as_posix()
        count = line_count(path)
        if count > HARD_LIMIT:
            errors.append(
                f"{rel}: {count} lines, hard limit is {HARD_LIMIT}; split the module"
            )
        elif count > SOFT_LIMIT:
            warnings.append(f"{rel}: {count} lines (soft limit {SOFT_LIMIT})")

    for warning in warnings:
        print(f"warning: {warning}", file=sys.stderr)
    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        print(
            "\nSplit by responsibility, not by line count alone.",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

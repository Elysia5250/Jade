#!/usr/bin/env python3
"""Fail when a Rust source file grows past the harness limits.

The console is intentionally small; a 2,000-line file is a review and merge
bottleneck. Existing oversized files are recorded in file-size-baseline.txt so
they can shrink without blocking unrelated work. New files are held to the hard
limit immediately, and a baseline entry may never grow.
"""

from __future__ import annotations

import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BASELINE = Path(__file__).with_name("file-size-baseline.txt")
SOFT_LIMIT = 400
HARD_LIMIT = 700


def read_baseline() -> dict[str, int]:
    baseline: dict[str, int] = {}
    if not BASELINE.exists():
        return baseline
    for raw in BASELINE.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        path, _, limit = line.partition("\t")
        if not path or not limit.isdigit():
            print(f"bad baseline line: {raw!r}", file=sys.stderr)
            raise SystemExit(2)
        baseline[path] = int(limit)
    return baseline


def rust_files() -> list[Path]:
    files: list[Path] = []
    for base in (ROOT / "crates",):
        if not base.exists():
            continue
        for path in base.rglob("*.rs"):
            parts = set(path.parts)
            if "target" in parts or "generated" in parts:
                continue
            files.append(path)
    return sorted(files)


def line_count(path: Path) -> int:
    with path.open("rb") as handle:
        return sum(1 for _ in handle)


def main() -> int:
    baseline = read_baseline()
    errors: list[str] = []
    warnings: list[str] = []
    seen: set[str] = set()

    for path in rust_files():
        rel = path.relative_to(ROOT).as_posix()
        count = line_count(path)
        seen.add(rel)
        limit = baseline.get(rel)
        if limit is not None:
            if count > limit:
                errors.append(
                    f"{rel}: {count} lines, baseline allows {limit}; split it instead of growing it"
                )
            elif count > HARD_LIMIT:
                warnings.append(f"{rel}: {count} lines (baseline; target <= {HARD_LIMIT})")
        elif count > HARD_LIMIT:
            errors.append(
                f"{rel}: {count} lines, hard limit is {HARD_LIMIT}; split the module"
            )
        elif count > SOFT_LIMIT:
            warnings.append(f"{rel}: {count} lines (soft limit {SOFT_LIMIT})")

    for rel, limit in baseline.items():
        if rel not in seen:
            warnings.append(f"{rel}: baseline entry is stale (file removed or renamed)")

    for warning in warnings:
        print(f"warning: {warning}", file=sys.stderr)
    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        print(
            "\nSplit by responsibility, not by line count alone. "
            "Update file-size-baseline.txt only when a file genuinely shrank.",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

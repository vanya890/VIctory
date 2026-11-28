#!/usr/bin/env python3
"""Check rustfmt only where a pull request or push changed Rust source lines.

The repository has legacy formatting debt in untouched lines. This check runs
rustfmt over each changed file, then rejects only formatter changes that overlap
the current change set. It therefore keeps new/modified Rust code formatted
without forcing unrelated workspace-wide reformatting.
"""
from __future__ import annotations

import difflib
import os
import re
import subprocess
import sys
from pathlib import Path


HUNK = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@")


def command(args: list[str], *, capture: bool = False) -> str:
    result = subprocess.run(
        args,
        check=True,
        text=True,
        encoding="utf-8",
        stdout=subprocess.PIPE if capture else None,
    )
    return result.stdout if capture else ""


def changed_line_ranges(base: str, path: str) -> list[tuple[int, int]]:
    patch = command(
        ["git", "diff", "--unified=0", f"{base}...HEAD", "--", path],
        capture=True,
    )
    ranges: list[tuple[int, int]] = []
    for line in patch.splitlines():
        match = HUNK.match(line)
        if not match:
            continue
        start = int(match.group(1))
        count = int(match.group(2) or "1")
        if count:
            ranges.append((start - 1, start - 1 + count))
    return ranges


def overlaps_changed_lines(
    original_start: int,
    original_end: int,
    ranges: list[tuple[int, int]],
) -> bool:
    for start, end in ranges:
        if original_start < end and original_end > start:
            return True
        if original_start == original_end and start <= original_start <= end:
            return True
    return False


def main() -> int:
    base_ref = os.environ.get("BASE_REF", "dev").strip()
    if not base_ref or re.fullmatch(r"0{40}", base_ref):
        base_ref = "dev"

    command(["git", "fetch", "--no-tags", "origin", base_ref])
    base = "FETCH_HEAD"
    files = command(
        [
            "git", "diff", "--name-only", "--diff-filter=ACMRT",
            f"{base}...HEAD", "--", "*.rs",
        ],
        capture=True,
    ).splitlines()

    checked = 0
    failures: list[str] = []
    for path in files:
        source_path = Path(path)
        if not source_path.is_file():
            continue

        ranges = changed_line_ranges(base, path)
        if not ranges:
            continue

        original = source_path.read_text(encoding="utf-8").splitlines(keepends=True)
        formatted_text = command(
            ["rustfmt", "--edition", "2024", "--emit", "stdout", path],
            capture=True,
        )
        formatted = formatted_text.splitlines(keepends=True)
        checked += 1

        matcher = difflib.SequenceMatcher(
            a=original,
            b=formatted,
            autojunk=False,
        )
        for tag, i1, i2, _j1, _j2 in matcher.get_opcodes():
            if tag != "equal" and overlaps_changed_lines(i1, i2, ranges):
                failures.append(path)
                break

    if failures:
        for path in sorted(set(failures)):
            print(f"rustfmt changes overlap modified lines in: {path}", file=sys.stderr)
        return 1

    print(f"rustfmt checked changed lines in {checked} changed Rust file(s); no violations.")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except subprocess.CalledProcessError as error:
        print(f"format check command failed: {error}", file=sys.stderr)
        raise SystemExit(error.returncode or 1)
    except Exception as error:
        print(f"format check failed: {error}", file=sys.stderr)
        raise SystemExit(1)

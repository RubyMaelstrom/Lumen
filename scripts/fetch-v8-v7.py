#!/usr/bin/env python3
"""Fetch the exact classic-v8 fixtures pinned by the benchmark manifest.

This is deliberately separate from bench-matrix.py: measured runs are network-free and fail if
their already-provisioned inputs do not match the checked-in hashes.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import sys
import tempfile
import urllib.error
import urllib.request


REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_MANIFEST = REPO_ROOT / "benchmarks" / "engine-matrix.json"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def within(root: Path, relative: str) -> Path:
    candidate = (root / relative).resolve()
    try:
        candidate.relative_to(root.resolve())
    except ValueError as error:
        raise ValueError(f"fixture path escapes repository: {relative}") from error
    return candidate


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    parser.add_argument(
        "--force",
        action="store_true",
        help="replace a mismatching existing fixture after its replacement verifies",
    )
    args = parser.parse_args()

    root = args.repo_root.resolve()
    manifest = json.loads(args.manifest.read_text(encoding="utf-8"))
    suite = manifest["suite"]
    destination = within(root, suite["fixture_root"])
    destination.mkdir(parents=True, exist_ok=True)
    raw_base = suite["source"]["raw_base"].rstrip("/")

    for entry in suite["files"]:
        target = within(destination, entry["path"])
        expected = entry["sha256"]
        if target.is_file():
            actual = sha256(target)
            if actual == expected:
                print(f"verified {target.relative_to(root)}")
                continue
            if not args.force:
                print(
                    f"error: {target.relative_to(root)} has sha256 {actual}, expected {expected}; "
                    "use --force to replace it",
                    file=sys.stderr,
                )
                return 1

        target.parent.mkdir(parents=True, exist_ok=True)
        url = f"{raw_base}/{entry['path']}"
        print(f"fetching {url}")
        temporary_name: str | None = None
        try:
            with urllib.request.urlopen(url, timeout=60) as response:
                with tempfile.NamedTemporaryFile(
                    dir=target.parent, prefix=f".{target.name}.", delete=False
                ) as temporary:
                    temporary_name = temporary.name
                    while chunk := response.read(1024 * 1024):
                        temporary.write(chunk)
                    temporary.flush()
                    os.fsync(temporary.fileno())
            temporary_path = Path(temporary_name)
            actual = sha256(temporary_path)
            if actual != expected:
                temporary_path.unlink(missing_ok=True)
                print(
                    f"error: downloaded {entry['path']} has sha256 {actual}, expected {expected}",
                    file=sys.stderr,
                )
                return 1
            os.replace(temporary_path, target)
        except (OSError, urllib.error.URLError) as error:
            if temporary_name is not None:
                Path(temporary_name).unlink(missing_ok=True)
            print(f"error: could not fetch {url}: {error}", file=sys.stderr)
            return 1

    print(f"all {len(suite['files'])} pinned fixtures are ready in {destination}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

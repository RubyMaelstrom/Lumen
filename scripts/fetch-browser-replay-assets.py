#!/usr/bin/env python3
"""Provision or verify the exact third-party assets pinned for browser replays.

Measured replay runs invoke only ``--verify-only``, which never accesses the network. A local
upstream checkout can be used as the provisioning source, which is useful for auditing the exact
bytes before they enter the fixture cache.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import urllib.error
import urllib.request


REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_MANIFEST = REPO_ROOT / "benchmarks" / "browser-replay-assets.json"


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
        raise ValueError(f"asset path escapes its root: {relative}") from error
    return candidate


def copy_or_download(source: Path | None, url: str, temporary: Path) -> None:
    if source is not None:
        with source.open("rb") as input_file, temporary.open("wb") as output_file:
            shutil.copyfileobj(input_file, output_file, length=1024 * 1024)
            output_file.flush()
            os.fsync(output_file.fileno())
        return
    with urllib.request.urlopen(url, timeout=60) as response, temporary.open("wb") as output_file:
        while chunk := response.read(1024 * 1024):
            output_file.write(chunk)
        output_file.flush()
        os.fsync(output_file.fileno())


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    parser.add_argument(
        "--source-checkout",
        type=Path,
        help="copy from this checkout of the pinned repository instead of downloading",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="replace a mismatching cached asset after its replacement verifies",
    )
    parser.add_argument(
        "--verify-only",
        action="store_true",
        help="require every cached asset to match; never copy, replace, or access the network",
    )
    args = parser.parse_args()

    root = args.repo_root.resolve()
    manifest = json.loads(args.manifest.read_text(encoding="utf-8"))
    suite = manifest["suite"]
    destination = within(root, suite["fixture_root"])
    destination.mkdir(parents=True, exist_ok=True)
    raw_base = suite["source"]["raw_base"].rstrip("/")
    source_root = args.source_checkout.resolve() if args.source_checkout else None

    for entry in suite["files"]:
        relative = entry["path"]
        target = within(destination, relative)
        expected = entry["sha256"]
        if target.is_file():
            actual = sha256(target)
            if actual == expected:
                print(f"verified {target.relative_to(root)}")
                continue
            if args.verify_only or not args.force:
                print(
                    f"error: {target.relative_to(root)} has sha256 {actual}, expected {expected}; "
                    "use --force to replace it",
                    file=sys.stderr,
                )
                return 1

        if args.verify_only:
            print(
                f"error: missing pinned asset {target.relative_to(root)}; "
                "run scripts/fetch-browser-replay-assets.py first",
                file=sys.stderr,
            )
            return 1

        source = within(source_root, relative) if source_root is not None else None
        if source is not None and not source.is_file():
            print(f"error: source checkout is missing {relative}", file=sys.stderr)
            return 1

        target.parent.mkdir(parents=True, exist_ok=True)
        url = f"{raw_base}/{relative}"
        temporary_name: str | None = None
        try:
            descriptor, temporary_name = tempfile.mkstemp(
                dir=target.parent, prefix=f".{target.name}."
            )
            os.close(descriptor)
            temporary = Path(temporary_name)
            if source is not None:
                print(f"copying {source}")
            else:
                print(f"fetching {url}")
            copy_or_download(source, url, temporary)
            actual = sha256(temporary)
            if actual != expected:
                temporary.unlink(missing_ok=True)
                print(
                    f"error: provisioned {relative} has sha256 {actual}, expected {expected}",
                    file=sys.stderr,
                )
                return 1
            os.replace(temporary, target)
        except (OSError, urllib.error.URLError) as error:
            if temporary_name is not None:
                Path(temporary_name).unlink(missing_ok=True)
            print(f"error: could not provision {relative}: {error}", file=sys.stderr)
            return 1

    print(f"all {len(suite['files'])} pinned assets are ready in {destination}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

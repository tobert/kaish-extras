#!/usr/bin/env python3
"""Build site/seed.json — the sample tree the try-kaish playground mounts.

Usage: make_seed.py OUT.json NAME=REPO_PATH [NAME=REPO_PATH ...]

Each repo's git-tracked text files land under /src/NAME/ in the playground's
in-memory VFS. Binary files (by sniff) and non-UTF-8 files are skipped; the
playground is for reading source, not shipping assets.
"""

import json
import subprocess
import sys
from pathlib import Path

MAX_FILE_BYTES = 512 * 1024  # skip pathological single files, keep the bundle honest


def tracked_files(repo: Path) -> list[str]:
    out = subprocess.run(
        ["git", "-C", str(repo), "ls-files", "-z"],
        capture_output=True, check=True,
    ).stdout
    return [p for p in out.decode().split("\0") if p]


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__, file=sys.stderr)
        return 2

    out_path = Path(sys.argv[1])
    seed: dict[str, str] = {}
    skipped = 0

    for spec in sys.argv[2:]:
        name, _, repo_str = spec.partition("=")
        repo = Path(repo_str).expanduser()
        for rel in tracked_files(repo):
            src = repo / rel
            try:
                raw = src.read_bytes()
            except OSError:
                skipped += 1
                continue
            if len(raw) > MAX_FILE_BYTES or b"\0" in raw[:8192]:
                skipped += 1
                continue
            try:
                text = raw.decode("utf-8")
            except UnicodeDecodeError:
                skipped += 1
                continue
            seed[f"/src/{name}/{rel}"] = text

    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(seed, ensure_ascii=False, separators=(",", ":")))
    total = sum(len(v.encode()) for v in seed.values())
    print(f"{out_path}: {len(seed)} files, {total / 1e6:.1f} MB text, {skipped} skipped")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

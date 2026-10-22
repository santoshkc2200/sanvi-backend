"""Fail closed on unclassified migrations and maintenance work in rolling mode.

Policy ships beside migrations in the image. Historical SQL is never rewritten.
Input status is emitted by `sanvi-cli migrate status`, never a guessed version.
"""
import argparse
import json
from pathlib import Path
import re
import sys


def check(migrations, policy_path, applied_path, mode):
    policy = json.loads(policy_path.read_text())
    status = applied_path.read_text().strip()
    applied = set()
    if status != "no migrations applied":
        if not status:
            raise ValueError("empty migration status")
        for line in status.splitlines():
            match = re.fullmatch(r"\s*(\d+)\s+\S.*", line)
            if not match:
                raise ValueError("invalid migration status; refusing to infer applied versions")
            applied.add(int(match[1]))
    pending = []
    available = set()
    for path in sorted(migrations.glob("*.sql")):
        if path.name.endswith(".down.sql"):
            continue
        if policy.get(path.name) not in ("rolling", "maintenance"):
            raise ValueError(f"unclassified migration: {path.name}")
        version = int(path.name.split("_", 1)[0])
        available.add(version)
        if version not in applied:
            pending.append(path.name)
    missing = applied - available
    if missing:
        raise ValueError("unsupported schema: candidate image lacks applied migration version(s) "
                         + ", ".join(map(str, sorted(missing)))
                         + "; use a schema-compatible image or perform the documented backup restore")
    blocked = [name for name in pending if policy[name] == "maintenance"]
    if blocked and mode != "maintenance":
        raise ValueError("maintenance mode required for: " + ", ".join(blocked))
    print(f"migration policy: {len(pending)} pending, mode={mode}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--migrations-dir", type=Path, required=True)
    parser.add_argument("--policy", type=Path, required=True)
    parser.add_argument("--applied-file", type=Path, required=True)
    parser.add_argument("--mode", choices=("rolling", "maintenance"), required=True)
    args = parser.parse_args()
    try:
        check(args.migrations_dir, args.policy, args.applied_file, args.mode)
    except (OSError, ValueError, KeyError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)

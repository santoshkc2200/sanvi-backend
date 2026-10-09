#!/usr/bin/env python3
"""Validate the v1 registry and generate/check its public changelog."""
import argparse
import calendar
import datetime as dt
import json
from pathlib import Path


def changelog(registry):
    if set(registry) != {"entries"} or not isinstance(registry["entries"], list):
        raise ValueError("registry must contain only an entries array")
    seen = set()
    lines = ["# API v1 deprecations", ""]
    for entry in registry["entries"]:
        if set(entry) != {"element", "method", "path", "deprecated_at", "sunset_at", "replacement"}:
            raise ValueError("deprecation entry has missing or unknown fields")
        element = entry["element"]
        if not element.strip() or element in seen:
            raise ValueError("deprecation elements must be nonempty and unique")
        seen.add(element)
        if entry["method"] not in {"GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"} or not entry["path"].startswith("/api/v1/"):
            raise ValueError("invalid operation method or v1 path")
        if not entry["replacement"].strip() or any(c in entry["replacement"] for c in "\r\n"):
            raise ValueError("replacement is required and must occupy one line")
        deprecated = dt.datetime.fromisoformat(entry["deprecated_at"].replace("Z", "+00:00"))
        sunset = dt.datetime.fromisoformat(entry["sunset_at"].replace("Z", "+00:00"))
        if deprecated.tzinfo is None or sunset.tzinfo is None:
            raise ValueError("dates need an explicit timezone")
        deprecated, sunset = deprecated.astimezone(dt.timezone.utc), sunset.astimezone(dt.timezone.utc)
        month = deprecated.month + 6
        year, month = deprecated.year + (month - 1) // 12, (month - 1) % 12 + 1
        earliest = deprecated.replace(year=year, month=month, day=min(deprecated.day, calendar.monthrange(year, month)[1]))
        if sunset < earliest:
            raise ValueError(f"{element} needs six calendar months before sunset")
        lines.append(f"- **{element}** (`{entry['method']} {entry['path']}`): deprecated {deprecated:%Y-%m-%d}; sunset {sunset:%Y-%m-%d}. Replacement: {entry['replacement']}.")
    if not registry["entries"]:
        lines.append("No API v1 elements are deprecated.")
    return "\n".join(lines) + "\n"


def main():
    root = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--registry", type=Path, default=root / "api/deprecations.json")
    parser.add_argument("--output", type=Path, default=root / "docs/api/deprecations.md")
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--write", action="store_true")
    group.add_argument("--check", action="store_true")
    args = parser.parse_args()
    try:
        content = changelog(json.loads(args.registry.read_text(encoding="utf-8")))
        if args.write:
            args.output.write_text(content, encoding="utf-8")
        elif args.check:
            if not args.output.exists() or args.output.read_text(encoding="utf-8") != content:
                raise ValueError("deprecation changelog is stale; run --write")
            print("deprecation registry and changelog: OK")
        else:
            print(content, end="")
    except (ValueError, KeyError, TypeError, OSError) as error:
        parser.exit(1, f"deprecation-policy: {error}\n")


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Compute the next automatic fork release version for the current commit."""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parent.parent
WORKSPACE_MANIFEST = REPO_ROOT / "codex-rs" / "Cargo.toml"
DEFAULT_RELEASE_CONFIG_PATH = REPO_ROOT / ".github" / "fork-release-config.json"
SEMVER_BASE_PATTERN = re.compile(r"(\d+\.\d+\.\d+)")
FORK_TAG_PATTERN = re.compile(r"^(?P<prefix>.+?)(?P<base>\d+\.\d+\.\d+)-rick\.(?P<counter>\d+)$")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--release-config",
        type=Path,
        default=DEFAULT_RELEASE_CONFIG_PATH,
        help="Path to the fork release JSON config.",
    )
    parser.add_argument(
        "--write-github-output",
        action="store_true",
        help="Write derived outputs to $GITHUB_OUTPUT.",
    )
    return parser.parse_args()


def load_release_config(path: Path) -> dict:
    with open(path, "r", encoding="utf-8") as fh:
        return json.load(fh)


def run_git(*args: str) -> str:
    completed = subprocess.run(
        ["git", *args],
        cwd=REPO_ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    return completed.stdout


def workspace_version() -> str:
    contents = WORKSPACE_MANIFEST.read_text(encoding="utf-8")
    match = re.search(
        r'(?ms)^\[workspace\.package\]\n(?:.+\n)*?^version = "([^"]+)"',
        contents,
    )
    if not match:
        raise RuntimeError(f"Could not resolve [workspace.package] version from {WORKSPACE_MANIFEST}.")
    return match.group(1)


def semver_base(version: str) -> str:
    match = SEMVER_BASE_PATTERN.search(version)
    if not match:
        raise RuntimeError(f"Could not extract semver base from version '{version}'.")
    return match.group(1)


def head_release_tag(prefix: str) -> str | None:
    tags = run_git("tag", "--points-at", "HEAD").splitlines()
    matching = [tag.strip() for tag in tags if tag.strip().startswith(prefix)]
    if not matching:
        return None
    return sorted(matching)[-1]


def next_counter(prefix: str, base_version: str) -> int:
    pattern = f"{prefix}{base_version}-rick.*"
    tags = run_git("tag", "--list", pattern).splitlines()
    highest = 0
    for tag in tags:
        match = FORK_TAG_PATTERN.match(tag.strip())
        if not match:
            continue
        if match.group("base") != base_version:
            continue
        highest = max(highest, int(match.group("counter")))
    return highest + 1


def parse_existing_release(prefix: str, tag: str) -> dict:
    if not tag.startswith(prefix):
        raise RuntimeError(f"Existing tag '{tag}' does not start with expected prefix '{prefix}'.")

    version = tag.removeprefix(prefix)
    match = re.match(rf"^{re.escape(semver_base(version))}-rick\.(\d+)$", version)
    if not match:
        raise RuntimeError(f"Existing tag '{tag}' does not match expected fork release version format.")

    return {
        "base_version": semver_base(version),
        "fork_counter": match.group(1),
        "version": version,
        "tag": tag,
        "tag_exists": "true",
    }


def compute_release(config: dict) -> dict:
    tag_prefix = config["tag_prefix"]
    current_version = workspace_version()
    base_version = semver_base(current_version)

    existing_tag = head_release_tag(tag_prefix)
    if existing_tag is not None:
        return parse_existing_release(tag_prefix, existing_tag)

    fork_counter = next_counter(tag_prefix, base_version)
    version = f"{base_version}-rick.{fork_counter}"
    return {
        "base_version": base_version,
        "fork_counter": str(fork_counter),
        "version": version,
        "tag": f"{tag_prefix}{version}",
        "tag_exists": "false",
    }


def write_github_output(values: dict) -> None:
    output_path = os.environ.get("GITHUB_OUTPUT")
    if not output_path:
        raise RuntimeError("GITHUB_OUTPUT is not set.")

    with open(output_path, "a", encoding="utf-8") as fh:
        for key, value in values.items():
            fh.write(f"{key}={value}\n")


def main() -> int:
    args = parse_args()
    config = load_release_config(args.release_config)
    release = compute_release(config)

    if args.write_github_output:
        write_github_output(release)

    print(json.dumps(release, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

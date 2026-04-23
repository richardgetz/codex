#!/usr/bin/env python3
"""Generate GitHub release notes for this fork's automatic releases."""

from __future__ import annotations

import argparse
import re
import subprocess
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parent.parent
WORKSPACE_MANIFEST = "codex-rs/Cargo.toml"
TAG_URL_BASE = "https://github.com/richardgetz/codex/releases/tag"
UPSTREAM_RELEASE_URL_BASE = "https://github.com/openai/codex/releases/tag"
FORK_TAG_PATTERN = re.compile(r"^rick-v\d+\.\d+\.\d+-rick\.\d+$")
MERGE_PR_PATTERN = re.compile(r"^Merge pull request #(?P<number>\d+) from (?P<branch>.+)$")
VERSION_PATTERN = re.compile(
    r'(?ms)^\[workspace\.package\]\n(?:.+\n)*?^version = "([^"]+)"'
)
SEMVER_BASE_PATTERN = re.compile(r"(\d+\.\d+\.\d+)")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--release-version", required=True)
    parser.add_argument("--release-tag", required=True)
    parser.add_argument("--base-version", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--head",
        default="HEAD",
        help="Commit to generate notes for. Defaults to HEAD.",
    )
    return parser.parse_args()


def run_git(*args: str, check: bool = True) -> str:
    completed = subprocess.run(
        ["git", *args],
        cwd=REPO_ROOT,
        check=check,
        capture_output=True,
        text=True,
    )
    if not check and completed.returncode != 0:
        return ""
    return completed.stdout


def commit_for_ref(ref: str) -> str | None:
    commit = run_git("rev-parse", "--verify", f"{ref}^{{commit}}", check=False).strip()
    return commit or None


def previous_fork_tag(release_tag: str, head: str) -> str | None:
    merged_tags = run_git(
        "tag",
        "--merged",
        head,
        "--sort=-creatordate",
        "--list",
        "rick-v*",
    ).splitlines()
    for tag in (tag.strip() for tag in merged_tags):
        if tag == release_tag or not FORK_TAG_PATTERN.match(tag):
            continue
        return tag
    return None


def workspace_version_at(ref: str) -> str | None:
    manifest = run_git("show", f"{ref}:{WORKSPACE_MANIFEST}", check=False)
    if not manifest:
        return None
    match = VERSION_PATTERN.search(manifest)
    if not match:
        return None
    return match.group(1)


def semver_base(version: str | None) -> str | None:
    if version is None:
        return None
    match = SEMVER_BASE_PATTERN.search(version)
    return match.group(1) if match else None


def first_parent_commits(previous_tag: str | None, head: str) -> list[dict[str, str]]:
    range_spec = f"{previous_tag}..{head}" if previous_tag else head
    output = run_git(
        "log",
        "--first-parent",
        "--reverse",
        "--format=%H%x1f%s%x1f%b%x1e",
        range_spec,
    )
    commits = []
    for entry in output.strip("\x1e\n").split("\x1e"):
        if not entry:
            continue
        sha, subject, body = (entry.strip("\n").split("\x1f", 2) + [""])[:3]
        commits.append({"sha": sha, "subject": subject, "body": body.strip()})
    return commits


def pr_label(commit: dict[str, str]) -> str:
    match = MERGE_PR_PATTERN.match(commit["subject"])
    if not match:
        return commit["subject"]

    body_subject = next(
        (line.strip() for line in commit["body"].splitlines() if line.strip()),
        "",
    )
    number = match.group("number")
    branch = match.group("branch")
    suffix = f" - {body_subject}" if body_subject else ""
    return f"#{number} {branch}{suffix}"


def is_mainline_refresh(commit: dict[str, str]) -> bool:
    text = f"{commit['subject']}\n{commit['body']}"
    return "stable-refresh/" in text


def bullet_list(items: list[str], empty: str) -> list[str]:
    if not items:
        return [f"- {empty}"]
    return [f"- {item}" for item in items]


def release_notes(
    release_version: str,
    release_tag: str,
    base_version: str,
    previous_tag: str | None,
    previous_base_version: str | None,
    commits: list[dict[str, str]],
) -> str:
    fork_changes = [pr_label(commit) for commit in commits if not is_mainline_refresh(commit)]
    mainline_changes = [pr_label(commit) for commit in commits if is_mainline_refresh(commit)]

    lines = [
        f"# {release_version}",
        "",
    ]
    if previous_tag:
        lines.extend(
            [
                f"Changes since [{previous_tag}]({TAG_URL_BASE}/{previous_tag}).",
                "",
            ]
        )
    else:
        lines.extend(["Initial fork release notes for this release line.", ""])

    lines.extend(["## Fork changes", ""])
    lines.extend(bullet_list(fork_changes, "No fork-specific changes in this release."))
    lines.append("")

    lines.extend(["## Mainline Codex", ""])
    if previous_base_version and previous_base_version != base_version:
        lines.append(f"- Upstream base changed from `{previous_base_version}` to `{base_version}`.")
    else:
        lines.append(f"- Upstream base remains `{base_version}`.")
    lines.append(f"- Upstream release notes: {UPSTREAM_RELEASE_URL_BASE}/rust-v{base_version}")
    lines.extend(
        bullet_list(
            mainline_changes,
            "No stable-refresh merge was detected in this release.",
        )
    )
    lines.append("")

    lines.extend(["## Install", "", "```bash", "npm install -g @rickgetz/codex", "```", ""])
    lines.append(f"Tag: `{release_tag}`")
    lines.append("")

    return "\n".join(lines)


def main() -> int:
    args = parse_args()
    previous_tag = previous_fork_tag(args.release_tag, args.head)
    previous_base_version = semver_base(workspace_version_at(previous_tag)) if previous_tag else None
    notes = release_notes(
        args.release_version,
        args.release_tag,
        args.base_version,
        previous_tag,
        previous_base_version,
        first_parent_commits(previous_tag, args.head),
    )
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(notes, encoding="utf-8")
    print(args.output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

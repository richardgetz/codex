#!/usr/bin/env python3
"""Helpers for fork-specific Codex release configuration."""

from __future__ import annotations

import json
from pathlib import Path

DEFAULT_SUPPORTED_TARGETS = [
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
]

DEFAULT_RELEASE_CONFIG = {
    "npm_package_name": "@openai/codex",
    "github_repo": "openai/codex",
    "repository_url": "git+https://github.com/openai/codex.git",
    "repository_directory": "codex-cli",
    "tag_prefix": "rust-v",
    "supported_targets": DEFAULT_SUPPORTED_TARGETS,
}


def load_release_config(path: Path | None) -> dict:
    config = dict(DEFAULT_RELEASE_CONFIG)
    if path is None:
        return config

    with open(path, "r", encoding="utf-8") as fh:
        loaded = json.load(fh)

    for key, value in loaded.items():
        config[key] = value

    return config


def package_alias_name(base_package_name: str, suffix: str) -> str:
    if base_package_name.startswith("@"):
        scope, package_name = base_package_name.split("/", 1)
        return f"{scope}/{package_name}-{suffix}"
    return f"{base_package_name}-{suffix}"

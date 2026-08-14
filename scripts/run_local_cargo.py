"""Run a local Cargo command with Codex's matching sandboxed V8 artifacts."""

import os
import re
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from codex_package.targets import REPO_ROOT
from codex_package.targets import TARGET_SPECS
from codex_package.v8 import resolve_codex_v8_cargo_env


def configured_cargo_target(cwd: Path, environment: dict[str, str]) -> str | None:
    target = environment.get("CARGO_BUILD_TARGET")
    if target:
        return target

    config_dirs = []
    current = cwd.resolve()
    while True:
        config_dirs.append(current / ".cargo")
        if current.parent == current:
            break
        current = current.parent

    cargo_home = Path(environment.get("CARGO_HOME", Path.home() / ".cargo"))
    config_dirs.append(cargo_home)

    configured_target: str | tuple[str, ...] | None = None
    for config_dir in reversed(config_dirs):
        for config_path in (config_dir / "config.toml", config_dir / "config"):
            target = target_from_config(config_path)
            if target is not None:
                configured_target = target
    if isinstance(configured_target, tuple):
        raise RuntimeError(
            "local Cargo builds cannot select one Codex V8 artifact for multiple "
            "build targets; pass --target explicitly"
        )
    return configured_target


def target_from_config_value(value: str) -> str | tuple[str, ...] | None:
    value = value.split("#", maxsplit=1)[0].strip()
    if value.startswith(('"', "'")):
        quote = value[0]
        end = value.find(quote, 1)
        return value[1:end] if end > 1 else None
    if value.startswith("["):
        targets = re.findall(r"['\"]([^'\"]+)['\"]", value)
        if len(targets) == 1:
            return targets[0]
        return tuple(targets)
    return None


def target_from_config(path: Path) -> str | tuple[str, ...] | None:
    if not path.is_file():
        return None

    section = None
    lines = path.read_text(encoding="utf-8").splitlines()
    for index, raw_line in enumerate(lines):
        line = raw_line.split("#", maxsplit=1)[0].strip()
        if not line:
            continue
        if line.startswith("["):
            section = line
            continue
        is_dotted_target = section is None and line.startswith("build.target")
        if section != "[build]" and not is_dotted_target:
            continue

        key_pattern = (
            r"target\s*=\s*(.+)$"
            if section == "[build]"
            else r"build\.target\s*=\s*(.+)$"
        )
        match = re.match(key_pattern, line)
        if match is None:
            continue
        value = match.group(1).strip()
        if value.startswith("["):
            while "]" not in value and index + 1 < len(lines):
                index += 1
                value += lines[index].split("#", maxsplit=1)[0]
        return target_from_config_value(value)
    return None


def command_line_cargo_target(
    args: list[str], cwd: Path
) -> str | tuple[str, ...] | None:
    configured_target = None
    for index, arg in enumerate(args):
        if arg == "--":
            break
        if arg == "--config" and index + 1 < len(args):
            value = args[index + 1]
        elif arg.startswith("--config="):
            value = arg.removeprefix("--config=")
        else:
            continue

        assignment = re.match(r"build\.target\s*=\s*(.+)$", value)
        if assignment is not None:
            configured_target = target_from_config_value(assignment.group(1))
            continue

        config_path = Path(value)
        if not config_path.is_absolute():
            config_path = cwd / config_path
        target = target_from_config(config_path)
        if target is not None:
            configured_target = target
    return configured_target


def cargo_target(args: list[str], environment: dict[str, str], cwd: Path) -> str | None:
    for index, arg in enumerate(args):
        if arg == "--":
            break
        if arg == "--target" and index + 1 < len(args):
            return args[index + 1]
        if arg.startswith("--target="):
            return arg.removeprefix("--target=")

    target = command_line_cargo_target(args, cwd)
    if isinstance(target, tuple):
        raise RuntimeError(
            "local Cargo builds cannot select one Codex V8 artifact for multiple "
            "build targets; pass --target explicitly"
        )
    if target:
        return target

    target = configured_cargo_target(cwd, environment)
    if target:
        return target

    result = subprocess.run(
        ["rustc", "-vV"],
        check=True,
        capture_output=True,
        text=True,
    )
    match = re.search(r"^host: (.+)$", result.stdout, re.MULTILINE)
    return match.group(1) if match else None


def main() -> int:
    args = sys.argv[1:]
    if not args:
        raise SystemExit("usage: run_local_cargo.py <cargo arguments>")

    environment = os.environ.copy()
    target = cargo_target(args, environment, REPO_ROOT / "codex-rs")
    if target in TARGET_SPECS:
        codex_v8_env = resolve_codex_v8_cargo_env(TARGET_SPECS[target])
        environment.update(codex_v8_env)

    return subprocess.run(
        ["cargo", *args],
        check=False,
        cwd=REPO_ROOT / "codex-rs",
        env=environment,
    ).returncode


if __name__ == "__main__":
    raise SystemExit(main())

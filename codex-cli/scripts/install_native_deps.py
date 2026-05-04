#!/usr/bin/env python3
"""Install Codex native binaries (Rust CLI plus ripgrep helpers)."""

import argparse
from contextlib import contextmanager
import hashlib
import json
import os
import shutil
import subprocess
import tarfile
import tempfile
import zipfile
from dataclasses import dataclass
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path
import platform
import sys
from typing import Iterable, Sequence
from urllib.parse import urlparse
from urllib.request import urlopen

SCRIPT_DIR = Path(__file__).resolve().parent
CODEX_CLI_ROOT = SCRIPT_DIR.parent
DEFAULT_WORKFLOW_URL = "https://github.com/openai/codex/actions/runs/17952349351"  # rust-v0.40.0
DEFAULT_GITHUB_REPO = "openai/codex"
VENDOR_DIR_NAME = "vendor"
RG_MANIFEST = CODEX_CLI_ROOT / "bin" / "rg"
BINARY_TARGETS = (
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
)


@dataclass(frozen=True)
class BinaryComponent:
    artifact_prefix: str  # matches the artifact filename prefix (e.g. codex-<target>.zst)
    dest_dir: str  # directory under vendor/<target>/ where the binary is installed
    binary_basename: str  # executable name inside dest_dir (before optional .exe)
    targets: tuple[str, ...] | None = None  # limit installation to specific targets


@dataclass(frozen=True)
class ObscuraAsset:
    target: str
    archive_name: str
    archive_format: str
    archive_member: str
    digest: str
    size: int


WINDOWS_TARGETS = tuple(target for target in BINARY_TARGETS if "windows" in target)
WRY_TARGETS = tuple(target for target in BINARY_TARGETS if "apple-darwin" in target)

BINARY_COMPONENTS = {
    "codex": BinaryComponent(
        artifact_prefix="codex",
        dest_dir="codex",
        binary_basename="codex",
    ),
    "codex-responses-api-proxy": BinaryComponent(
        artifact_prefix="codex-responses-api-proxy",
        dest_dir="codex-responses-api-proxy",
        binary_basename="codex-responses-api-proxy",
    ),
    "codex-windows-sandbox-setup": BinaryComponent(
        artifact_prefix="codex-windows-sandbox-setup",
        dest_dir="codex",
        binary_basename="codex-windows-sandbox-setup",
        targets=WINDOWS_TARGETS,
    ),
    "codex-command-runner": BinaryComponent(
        artifact_prefix="codex-command-runner",
        dest_dir="codex",
        binary_basename="codex-command-runner",
        targets=WINDOWS_TARGETS,
    ),
    "codex-agent-browser-wry": BinaryComponent(
        artifact_prefix="codex-agent-browser-wry",
        dest_dir="browser",
        binary_basename="codex-agent-browser-wry",
        targets=WRY_TARGETS,
    ),
}

RG_TARGET_PLATFORM_PAIRS: list[tuple[str, str]] = [
    ("x86_64-unknown-linux-musl", "linux-x86_64"),
    ("aarch64-unknown-linux-musl", "linux-aarch64"),
    ("x86_64-apple-darwin", "macos-x86_64"),
    ("aarch64-apple-darwin", "macos-aarch64"),
    ("x86_64-pc-windows-msvc", "windows-x86_64"),
    ("aarch64-pc-windows-msvc", "windows-aarch64"),
]
RG_TARGET_TO_PLATFORM = {target: platform for target, platform in RG_TARGET_PLATFORM_PAIRS}
DEFAULT_RG_TARGETS = [target for target, _ in RG_TARGET_PLATFORM_PAIRS]
DEFAULT_OBSCURA_VERSION = "v0.1.2"
OBSCURA_RELEASES: dict[str, dict[str, ObscuraAsset]] = {
    DEFAULT_OBSCURA_VERSION: {
        "aarch64-apple-darwin": ObscuraAsset(
            target="aarch64-apple-darwin",
            archive_name="obscura-aarch64-macos.tar.gz",
            archive_format="tar.gz",
            archive_member="obscura",
            digest="sha256:ba105ef19ceeb36db0bfa0386c3db5251737fd816cac322cba4f4b85f60749bd",
            size=38_267_564,
        ),
        "x86_64-unknown-linux-musl": ObscuraAsset(
            target="x86_64-unknown-linux-musl",
            archive_name="obscura-x86_64-linux.tar.gz",
            archive_format="tar.gz",
            archive_member="obscura",
            digest="sha256:307ce58affea3ff39223bf573cf391c8883e0811d36e9cd7185f8c7c54942802",
            size=51_687_685,
        ),
        "x86_64-pc-windows-msvc": ObscuraAsset(
            target="x86_64-pc-windows-msvc",
            archive_name="obscura-x86_64-windows.zip",
            archive_format="zip",
            archive_member="obscura.exe",
            digest="sha256:bb342ea818f3f0208017ccf56314f709c591d0b9e14041f27ff760600490458a",
            size=35_322_430,
        ),
    }
}

# urllib.request.urlopen() defaults to no timeout (can hang indefinitely), which is painful in CI.
DOWNLOAD_TIMEOUT_SECS = 60


def _gha_enabled() -> bool:
    # GitHub Actions supports "workflow commands" (e.g. ::group:: / ::error::) that make logs
    # much easier to scan: groups collapse noisy sections and error annotations surface the
    # failure in the UI without changing the actual exception/traceback output.
    return os.environ.get("GITHUB_ACTIONS") == "true"


def _gha_escape(value: str) -> str:
    # Workflow commands require percent/newline escaping.
    return value.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def _gha_error(*, title: str, message: str) -> None:
    # Emit a GitHub Actions error annotation. This does not replace stdout/stderr logs; it just
    # adds a prominent summary line to the job UI so the root cause is easier to spot.
    if not _gha_enabled():
        return
    print(
        f"::error title={_gha_escape(title)}::{_gha_escape(message)}",
        flush=True,
    )


@contextmanager
def _gha_group(title: str):
    # Wrap a block in a collapsible log group on GitHub Actions. Outside of GHA this is a no-op
    # so local output remains unchanged.
    if _gha_enabled():
        print(f"::group::{_gha_escape(title)}", flush=True)
    try:
        yield
    finally:
        if _gha_enabled():
            print("::endgroup::", flush=True)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Install native Codex binaries.")
    parser.add_argument(
        "--workflow-url",
        help=(
            "GitHub Actions workflow URL that produced the artifacts. Defaults to a "
            "known good run when omitted."
        ),
    )
    parser.add_argument(
        "--repo",
        default=DEFAULT_GITHUB_REPO,
        help="GitHub repository that owns the workflow run artifacts (default: openai/codex).",
    )
    parser.add_argument(
        "--component",
        dest="components",
        action="append",
        choices=tuple(list(BINARY_COMPONENTS) + ["rg", "obscura"]),
        help=(
            "Limit installation to the specified components."
            " May be repeated. Defaults to codex, codex-windows-sandbox-setup,"
            " codex-command-runner, and rg."
        ),
    )
    parser.add_argument(
        "--target",
        dest="targets",
        action="append",
        choices=BINARY_TARGETS,
        help="Restrict installation to the specified target triple(s). May be repeated.",
    )
    parser.add_argument(
        "root",
        nargs="?",
        type=Path,
        help=(
            "Directory containing package.json for the staged package. If omitted, the "
            "repository checkout is used."
        ),
    )
    parser.add_argument(
        "--obscura-version",
        default=DEFAULT_OBSCURA_VERSION,
        choices=tuple(OBSCURA_RELEASES),
        help=(
            "Obscura release version to fetch when --component obscura is selected "
            f"(default: {DEFAULT_OBSCURA_VERSION})."
        ),
    )
    parser.add_argument(
        "--obscura-binary",
        type=Path,
        help=(
            "Install a local Obscura binary instead of downloading a pinned release asset. "
            "Use with --component obscura and exactly one --target, or pass "
            "--obscura-binary-target."
        ),
    )
    parser.add_argument(
        "--obscura-binary-target",
        choices=BINARY_TARGETS,
        help="Target triple for --obscura-binary when selected targets are ambiguous.",
    )
    parser.add_argument(
        "--obscura-source-dir",
        type=Path,
        help=(
            "Build Obscura from a local source checkout, then install the produced binary. "
            "Use with --component obscura and exactly one --target, or pass "
            "--obscura-binary-target."
        ),
    )
    parser.add_argument(
        "--obscura-source-patch",
        type=Path,
        default=SCRIPT_DIR / "obscura-runtime-dom-render.patch",
        help=(
            "Patch to apply when --obscura-source-dir is used. Defaults to the "
            "tracked Codex Obscura runtime patch."
        ),
    )
    parser.add_argument(
        "--obscura-source-release",
        action="store_true",
        help="Build --obscura-source-dir with cargo --release before staging it.",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()

    codex_cli_root = (args.root or CODEX_CLI_ROOT).resolve()
    vendor_dir = codex_cli_root / VENDOR_DIR_NAME
    vendor_dir.mkdir(parents=True, exist_ok=True)

    if args.obscura_binary and args.obscura_source_dir:
        raise RuntimeError("--obscura-binary and --obscura-source-dir are mutually exclusive")
    if (args.obscura_binary or args.obscura_source_dir) and args.components is None:
        components = ["obscura"]
    else:
        components = args.components or [
            "codex",
            "codex-windows-sandbox-setup",
            "codex-command-runner",
            "rg",
        ]
    if (args.obscura_binary or args.obscura_source_dir) and "obscura" not in components:
        raise RuntimeError("--obscura-binary/--obscura-source-dir requires --component obscura")
    selected_targets = args.targets or list(BINARY_TARGETS)
    selected_binary_components = [
        BINARY_COMPONENTS[name] for name in components if name in BINARY_COMPONENTS
    ]

    workflow_url = (args.workflow_url or DEFAULT_WORKFLOW_URL).strip()
    if not workflow_url:
        workflow_url = DEFAULT_WORKFLOW_URL

    workflow_id = workflow_url.rstrip("/").split("/")[-1]
    if selected_binary_components:
        print(f"Downloading native artifacts from workflow {workflow_id} in {args.repo}...")

        with _gha_group(f"Download native artifacts from workflow {workflow_id}"):
            with tempfile.TemporaryDirectory(
                prefix="codex-native-artifacts-"
            ) as artifacts_dir_str:
                artifacts_dir = Path(artifacts_dir_str)
                _download_artifacts(workflow_id, artifacts_dir, args.repo)
                install_binary_components(
                    artifacts_dir,
                    vendor_dir,
                    selected_binary_components,
                    selected_targets,
                )

    if "rg" in components:
        with _gha_group("Fetch ripgrep binaries"):
            print("Fetching ripgrep binaries...")
            fetch_rg(vendor_dir, selected_targets, manifest_path=RG_MANIFEST)

    if "obscura" in components:
        with _gha_group("Fetch Obscura browser binaries"):
            if args.obscura_binary:
                target = resolve_local_obscura_target(
                    selected_targets,
                    binary_target=args.obscura_binary_target,
                )
                installed = install_local_obscura_binary(
                    vendor_dir,
                    target,
                    args.obscura_binary,
                )
                print(f"Installed local Obscura browser binary: {installed}")
            elif args.obscura_source_dir:
                target = resolve_local_obscura_target(
                    selected_targets,
                    binary_target=args.obscura_binary_target,
                )
                installed = build_and_install_local_obscura_from_source(
                    vendor_dir,
                    target,
                    source_dir=args.obscura_source_dir,
                    patch_path=args.obscura_source_patch,
                    release=args.obscura_source_release,
                )
                print(f"Built and installed local Obscura browser binary: {installed}")
            else:
                print(f"Fetching Obscura browser binaries from {args.obscura_version}...")
                fetch_obscura(vendor_dir, selected_targets, release_version=args.obscura_version)

    print(f"Installed native dependencies into {vendor_dir}")
    return 0


def fetch_rg(
    vendor_dir: Path,
    targets: Sequence[str] | None = None,
    *,
    manifest_path: Path,
) -> list[Path]:
    """Download ripgrep binaries described by the DotSlash manifest."""

    if targets is None:
        targets = DEFAULT_RG_TARGETS

    if not manifest_path.exists():
        raise FileNotFoundError(f"DotSlash manifest not found: {manifest_path}")

    manifest = _load_manifest(manifest_path)
    platforms = manifest.get("platforms", {})

    vendor_dir.mkdir(parents=True, exist_ok=True)

    targets = list(targets)
    if not targets:
        return []

    task_configs: list[tuple[str, str, dict]] = []
    for target in targets:
        platform_key = RG_TARGET_TO_PLATFORM.get(target)
        if platform_key is None:
            raise ValueError(f"Unsupported ripgrep target '{target}'.")

        platform_info = platforms.get(platform_key)
        if platform_info is None:
            raise RuntimeError(f"Platform '{platform_key}' not found in manifest {manifest_path}.")

        task_configs.append((target, platform_key, platform_info))

    results: dict[str, Path] = {}
    max_workers = min(len(task_configs), max(1, (os.cpu_count() or 1)))

    print("Installing ripgrep binaries for targets: " + ", ".join(targets))

    with ThreadPoolExecutor(max_workers=max_workers) as executor:
        future_map = {
            executor.submit(
                _fetch_single_rg,
                vendor_dir,
                target,
                platform_key,
                platform_info,
                manifest_path,
            ): target
            for target, platform_key, platform_info in task_configs
        }

        for future in as_completed(future_map):
            target = future_map[future]
            try:
                results[target] = future.result()
            except Exception as exc:
                _gha_error(
                    title="ripgrep install failed",
                    message=f"target={target} error={exc!r}",
                )
                raise RuntimeError(f"Failed to install ripgrep for target {target}.") from exc
            print(f"  installed ripgrep for {target}")

    return [results[target] for target in targets]


def fetch_obscura(
    vendor_dir: Path,
    targets: Sequence[str] | None = None,
    *,
    release_version: str = DEFAULT_OBSCURA_VERSION,
) -> list[Path]:
    """Download optional Obscura browser binaries for targets with upstream assets."""

    if targets is None:
        targets = list(BINARY_TARGETS)

    assets = OBSCURA_RELEASES[release_version]
    targets = list(targets)
    selected_assets = [assets[target] for target in targets if target in assets]
    missing_targets = [target for target in targets if target not in assets]
    if missing_targets:
        print(
            "  no Obscura release asset for targets: " + ", ".join(missing_targets),
            flush=True,
        )

    if not selected_assets:
        raise RuntimeError(
            f"No Obscura {release_version} release assets match selected targets: "
            + ", ".join(targets)
        )

    vendor_dir.mkdir(parents=True, exist_ok=True)
    results: dict[str, Path] = {}
    max_workers = min(len(selected_assets), max(1, (os.cpu_count() or 1)))

    with ThreadPoolExecutor(max_workers=max_workers) as executor:
        future_map = {
            executor.submit(
                _fetch_single_obscura, vendor_dir, release_version, asset
            ): asset.target
            for asset in selected_assets
        }
        for future in as_completed(future_map):
            target = future_map[future]
            try:
                results[target] = future.result()
            except Exception as exc:
                _gha_error(
                    title="Obscura install failed",
                    message=f"target={target} release={release_version} error={exc!r}",
                )
                raise RuntimeError(
                    f"Failed to install Obscura {release_version} for target {target}."
                ) from exc
            print(f"  installed Obscura for {target}")

    return [results[asset.target] for asset in selected_assets]


def resolve_local_obscura_target(
    selected_targets: Sequence[str],
    *,
    binary_target: str | None,
) -> str:
    if binary_target:
        return binary_target
    if len(selected_targets) == 1:
        return selected_targets[0]
    raise RuntimeError(
        "local Obscura install requires exactly one --target or --obscura-binary-target"
    )


def install_local_obscura_binary(vendor_dir: Path, target: str, source: Path) -> Path:
    source = source.resolve()
    if not source.is_file():
        raise FileNotFoundError(f"Local Obscura binary not found: {source}")
    binary_name = "obscura.exe" if "windows" in target else "obscura"
    dest = vendor_dir / target / "browser" / binary_name
    dest.parent.mkdir(parents=True, exist_ok=True)
    if source != dest.resolve():
        dest.unlink(missing_ok=True)
        shutil.copy2(source, dest)
    if "windows" not in target:
        dest.chmod(0o755)
    return dest


def build_and_install_local_obscura_from_source(
    vendor_dir: Path,
    target: str,
    *,
    source_dir: Path,
    patch_path: Path | None,
    release: bool,
) -> Path:
    ensure_obscura_source_build_target_matches_host(target)
    source_dir = source_dir.resolve()
    if not (source_dir / "Cargo.toml").is_file():
        raise FileNotFoundError(f"Obscura Cargo.toml not found under {source_dir}")
    with tempfile.TemporaryDirectory(prefix="codex-obscura-source-") as tmp_dir_str:
        tmp_dir = Path(tmp_dir_str)
        build_dir = tmp_dir / "obscura-src"
        shutil.copytree(
            source_dir,
            build_dir,
            ignore=shutil.ignore_patterns(
                ".git",
                "target",
                ".cargo",
            ),
        )
        if patch_path:
            apply_obscura_patch_if_needed(build_dir, patch_path.resolve())
        target_dir = tmp_dir / "target"
        cmd = ["cargo", "build", "--bin", "obscura"]
        if release:
            cmd.append("--release")
        env = os.environ.copy()
        env["CARGO_HOME"] = source_build_cargo_home(env, tmp_dir)
        env["CARGO_TARGET_DIR"] = str(target_dir)
        subprocess.check_call(cmd, cwd=build_dir, env=env)
        profile = "release" if release else "debug"
        binary = target_dir / profile / ("obscura.exe" if sys.platform == "win32" else "obscura")
        if not binary.is_file():
            raise FileNotFoundError(f"Built Obscura binary not found: {binary}")
        return install_local_obscura_binary(vendor_dir, target, binary)


def ensure_obscura_source_build_target_matches_host(target: str) -> None:
    host_system = platform.system()
    host_machine = platform.machine().lower()
    if host_system == "Darwin" and host_machine in {"arm64", "aarch64"}:
        host_target = "aarch64-apple-darwin"
    elif host_system == "Darwin" and host_machine in {"x86_64", "amd64"}:
        host_target = "x86_64-apple-darwin"
    elif host_system == "Windows" and host_machine in {"amd64", "x86_64"}:
        host_target = "x86_64-pc-windows-msvc"
    else:
        raise RuntimeError(
            f"--obscura-source-dir is only supported on macOS and x86_64 Windows hosts; "
            f"detected {host_system} {host_machine}."
        )
    if target != host_target:
        raise RuntimeError(
            f"--obscura-source-dir builds a host binary for {host_target}; selected target "
            f"{target}. Pass --target {host_target} or install a prebuilt binary with "
            "--obscura-binary."
        )


def source_build_cargo_home(env: dict[str, str], tmp_dir: Path) -> str:
    cargo_home = env.get("CARGO_HOME")
    if cargo_home:
        cargo_home_path = Path(cargo_home).expanduser()
        if directory_is_writable(cargo_home_path):
            return str(cargo_home_path)
    return str(tmp_dir / "cargo-home")


def directory_is_writable(path: Path) -> bool:
    try:
        path.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile(prefix=".codex-write-test-", dir=path, delete=True):
            return True
    except OSError:
        return False


def apply_obscura_patch_if_needed(source_dir: Path, patch_path: Path) -> None:
    if not patch_path.is_file():
        raise FileNotFoundError(f"Obscura patch not found: {patch_path}")
    check_cmd = ["git", "apply", "--check", str(patch_path)]
    reverse_check_cmd = ["git", "apply", "--reverse", "--check", str(patch_path)]
    if subprocess.run(
        check_cmd, cwd=source_dir, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    ).returncode == 0:
        subprocess.check_call(["git", "apply", str(patch_path)], cwd=source_dir)
        return
    if subprocess.run(
        reverse_check_cmd, cwd=source_dir, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    ).returncode == 0:
        return
    raise RuntimeError(f"Obscura patch cannot be applied to {source_dir}: {patch_path}")


def _download_artifacts(workflow_id: str, dest_dir: Path, github_repo: str) -> None:
    cmd = [
        "gh",
        "run",
        "download",
        "--dir",
        str(dest_dir),
        "--repo",
        github_repo,
        workflow_id,
    ]
    subprocess.check_call(cmd)


def install_binary_components(
    artifacts_dir: Path,
    vendor_dir: Path,
    selected_components: Sequence[BinaryComponent],
    selected_targets: Sequence[str],
) -> None:
    if not selected_components:
        return

    selected_target_set = set(selected_targets)
    for component in selected_components:
        component_targets = list(component.targets or BINARY_TARGETS)
        component_targets = [target for target in component_targets if target in selected_target_set]

        print(
            f"Installing {component.binary_basename} binaries for targets: "
            + ", ".join(component_targets)
        )
        max_workers = min(len(component_targets), max(1, (os.cpu_count() or 1)))
        with ThreadPoolExecutor(max_workers=max_workers) as executor:
            futures = {
                executor.submit(
                    _install_single_binary,
                    artifacts_dir,
                    vendor_dir,
                    target,
                    component,
                ): target
                for target in component_targets
            }
            for future in as_completed(futures):
                installed_path = future.result()
                print(f"  installed {installed_path}")


def _install_single_binary(
    artifacts_dir: Path,
    vendor_dir: Path,
    target: str,
    component: BinaryComponent,
) -> Path:
    artifact_subdir = artifacts_dir / target
    archive_name = _archive_name_for_target(component.artifact_prefix, target)
    archive_path = artifact_subdir / archive_name
    if not archive_path.exists():
        raise FileNotFoundError(f"Expected artifact not found: {archive_path}")

    dest_dir = vendor_dir / target / component.dest_dir
    dest_dir.mkdir(parents=True, exist_ok=True)

    binary_name = (
        f"{component.binary_basename}.exe" if "windows" in target else component.binary_basename
    )
    dest = dest_dir / binary_name
    dest.unlink(missing_ok=True)
    extract_archive(archive_path, "zst", None, dest)
    if "windows" not in target:
        dest.chmod(0o755)
    return dest


def _archive_name_for_target(artifact_prefix: str, target: str) -> str:
    if "windows" in target:
        return f"{artifact_prefix}-{target}.exe.zst"
    return f"{artifact_prefix}-{target}.zst"


def _fetch_single_obscura(
    vendor_dir: Path,
    release_version: str,
    asset: ObscuraAsset,
) -> Path:
    url = (
        "https://github.com/h4ckf0r0day/obscura/releases/download/"
        f"{release_version}/{asset.archive_name}"
    )
    binary_name = "obscura.exe" if "windows" in asset.target else "obscura"
    dest = vendor_dir / asset.target / "browser" / binary_name

    with tempfile.TemporaryDirectory() as tmp_dir_str:
        tmp_dir = Path(tmp_dir_str)
        download_path = tmp_dir / asset.archive_name
        print(f"  downloading Obscura for {asset.target} from {url}", flush=True)
        _download_file(url, download_path)
        _verify_download(download_path, expected_digest=asset.digest, expected_size=asset.size)
        dest.unlink(missing_ok=True)
        extract_archive(download_path, asset.archive_format, asset.archive_member, dest)

    if "windows" not in asset.target:
        dest.chmod(0o755)

    return dest


def _fetch_single_rg(
    vendor_dir: Path,
    target: str,
    platform_key: str,
    platform_info: dict,
    manifest_path: Path,
) -> Path:
    providers = platform_info.get("providers", [])
    if not providers:
        raise RuntimeError(f"No providers listed for platform '{platform_key}' in {manifest_path}.")

    url = providers[0]["url"]
    archive_format = platform_info.get("format", "zst")
    archive_member = platform_info.get("path")
    digest = platform_info.get("digest")
    expected_size = platform_info.get("size")

    dest_dir = vendor_dir / target / "path"
    dest_dir.mkdir(parents=True, exist_ok=True)

    is_windows = platform_key.startswith("win")
    binary_name = "rg.exe" if is_windows else "rg"
    dest = dest_dir / binary_name

    with tempfile.TemporaryDirectory() as tmp_dir_str:
        tmp_dir = Path(tmp_dir_str)
        archive_filename = os.path.basename(urlparse(url).path)
        download_path = tmp_dir / archive_filename
        print(
            f"  downloading ripgrep for {target} ({platform_key}) from {url}",
            flush=True,
        )
        try:
            _download_file(url, download_path)
        except Exception as exc:
            _gha_error(
                title="ripgrep download failed",
                message=f"target={target} platform={platform_key} url={url} error={exc!r}",
            )
            raise RuntimeError(
                "Failed to download ripgrep "
                f"(target={target}, platform={platform_key}, format={archive_format}, "
                f"expected_size={expected_size!r}, digest={digest!r}, url={url}, dest={download_path})."
            ) from exc

        dest.unlink(missing_ok=True)
        try:
            extract_archive(download_path, archive_format, archive_member, dest)
        except Exception as exc:
            raise RuntimeError(
                "Failed to extract ripgrep "
                f"(target={target}, platform={platform_key}, format={archive_format}, "
                f"member={archive_member!r}, url={url}, archive={download_path})."
            ) from exc

    if not is_windows:
        dest.chmod(0o755)

    return dest


def _verify_download(
    path: Path,
    *,
    expected_digest: str | None,
    expected_size: int | None,
) -> None:
    if expected_size is not None:
        actual_size = path.stat().st_size
        if actual_size != expected_size:
            raise RuntimeError(
                f"Downloaded file has size {actual_size}, expected {expected_size}: {path}"
            )

    if expected_digest:
        algorithm, _, expected_hex = expected_digest.partition(":")
        if algorithm != "sha256" or not expected_hex:
            raise RuntimeError(f"Unsupported digest '{expected_digest}' for {path}.")
        hasher = hashlib.sha256()
        with open(path, "rb") as file:
            for chunk in iter(lambda: file.read(1024 * 1024), b""):
                hasher.update(chunk)
        actual_hex = hasher.hexdigest()
        if actual_hex != expected_hex:
            raise RuntimeError(
                f"Downloaded file has sha256 {actual_hex}, expected {expected_hex}: {path}"
            )


def _download_file(url: str, dest: Path) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.unlink(missing_ok=True)

    with urlopen(url, timeout=DOWNLOAD_TIMEOUT_SECS) as response, open(dest, "wb") as out:
        shutil.copyfileobj(response, out)


def extract_archive(
    archive_path: Path,
    archive_format: str,
    archive_member: str | None,
    dest: Path,
) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)

    if archive_format == "zst":
        output_path = archive_path.parent / dest.name
        subprocess.check_call(
            ["zstd", "-f", "-d", str(archive_path), "-o", str(output_path)]
        )
        shutil.move(str(output_path), dest)
        return

    if archive_format == "tar.gz":
        if not archive_member:
            raise RuntimeError("Missing 'path' for tar.gz archive in DotSlash manifest.")
        with tarfile.open(archive_path, "r:gz") as tar:
            try:
                member = tar.getmember(archive_member)
            except KeyError as exc:
                raise RuntimeError(
                    f"Entry '{archive_member}' not found in archive {archive_path}."
                ) from exc
            tar.extract(member, path=archive_path.parent, filter="data")
        extracted = archive_path.parent / archive_member
        shutil.move(str(extracted), dest)
        return

    if archive_format == "zip":
        if not archive_member:
            raise RuntimeError("Missing 'path' for zip archive in DotSlash manifest.")
        with zipfile.ZipFile(archive_path) as archive:
            try:
                with archive.open(archive_member) as src, open(dest, "wb") as out:
                    shutil.copyfileobj(src, out)
            except KeyError as exc:
                raise RuntimeError(
                    f"Entry '{archive_member}' not found in archive {archive_path}."
                ) from exc
        return

    raise RuntimeError(f"Unsupported archive format '{archive_format}'.")


def _load_manifest(manifest_path: Path) -> dict:
    cmd = ["dotslash", "--", "parse", str(manifest_path)]
    stdout = subprocess.check_output(cmd, text=True)
    try:
        manifest = json.loads(stdout)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"Invalid DotSlash manifest output from {manifest_path}.") from exc

    if not isinstance(manifest, dict):
        raise RuntimeError(
            f"Unexpected DotSlash manifest structure for {manifest_path}: {type(manifest)!r}"
        )

    return manifest


if __name__ == "__main__":
    import sys

    sys.exit(main())

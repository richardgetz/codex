# npm releases

Use the staging helper in the repo root to generate npm tarballs for a release. For
example, to stage the CLI, responses proxy, and SDK packages for version `0.6.0`:

```bash
./scripts/stage_npm_packages.py \
  --release-version 0.6.0 \
  --package codex \
  --package codex-responses-api-proxy \
  --package codex-sdk
```

This downloads the native artifacts once, hydrates `vendor/` for each package, and writes
tarballs to `dist/npm/`.

When `--package codex` is provided, the staging helper builds the lightweight
`@openai/codex` meta package plus all platform-native `@openai/codex` variants
that are later published under platform-specific dist-tags.

If you need to invoke `build_npm_package.py` directly, run
`codex-cli/scripts/install_native_deps.py` first and pass `--vendor-src` pointing to the
directory that contains the populated `vendor/` tree.

The built-in agent browser can also ship optional browser resources. The
WRY/WebKit helper is built from this repository's release artifacts and staged
as a native component for macOS packages; Obscura remains available as a
lightweight/headless Rust browser resource. Fetch the currently pinned upstream
Obscura assets with:

```bash
codex-cli/scripts/install_native_deps.py --component obscura
```

For local patched Obscura builds, install a specific binary into the same vendor
layout:

```bash
codex-cli/scripts/install_native_deps.py --component obscura \
  --target aarch64-apple-darwin \
  --obscura-binary /path/to/obscura
```

When `--obscura-binary` is provided without any `--component`, the helper only
installs the local Obscura binary.

The helper can also build from a local Obscura source checkout and apply the
tracked runtime patch before staging the host binary:

```bash
codex-cli/scripts/install_native_deps.py --component obscura \
  --target aarch64-apple-darwin \
  --obscura-source-dir /path/to/obscura
```

When `--obscura-source-dir` is provided without any `--component`, the helper
only builds and installs Obscura. This mode intentionally requires the selected
target to match the current host; use `--obscura-binary` for cross-target
staging. Source builds reuse the caller's `CARGO_HOME` only when it is writable,
then fall back to temporary Cargo home and target directories so package staging
does not depend on local cache permissions.

`obscura-runtime-dom-render.patch` records the current local Obscura runtime
patch used to render the Mobian React app during development. The patch covers
root ES module execution, DOM text/style collection support, linked stylesheet
injection, bounded image data-url inlining, and heuristic element rects while
waiting for an upstream or forked Obscura release asset.

The helper installs macOS WRY and available Obscura browser binaries under
`vendor/<target>/browser/`; platform packages and standalone installers then
preserve those resources under `codex-resources/` when present.

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

The built-in agent browser can also ship an optional Obscura browser resource.
Fetch the currently pinned upstream Obscura assets with:

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

`obscura-runtime-dom-render.patch` records the current local Obscura runtime
patch used to render the Mobian React app during development while waiting for
an upstream or forked Obscura release asset.

The helper installs available Obscura release assets under
`vendor/<target>/browser/`; platform packages and standalone installers then
preserve that resource as `codex-resources/obscura` when present.

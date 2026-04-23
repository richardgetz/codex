# Fork npm releases

This fork includes a mac-focused npm release lane for publishing `@rickgetz/codex`
automatically from pushes to `stable`.

## Versioning

Use the upstream release version as the base, then append the fork revision:

- `0.122.0-rick.1`
- `0.122.0-rick.2`
- `0.123.0-rick.1`

The workflow creates a matching annotated tag automatically:

- `rick-v0.122.0-rick.1`

## Supported release targets

The fork npm release workflow currently publishes:

- `aarch64-apple-darwin`

That keeps the release matrix focused on Apple Silicon macOS installs only.

## One-time npm setup

You still need to do the npm account-side setup yourself:

1. Ensure the `rickgetz` npm user/scope is the one you want to publish from.
2. Use a package name under that scope, currently `@rickgetz/codex`.
3. Add this repo/workflow as a trusted publisher for both published npm packages:
   `@rickgetz/codex` and `@rickgetz/codex-darwin-arm64`.
4. In npm, configure each trusted publisher for GitHub Actions with owner
   `richardgetz`, repository `codex`, workflow filename `fork-release.yml`, and
   no environment unless the workflow later adds one.

The workflow publishes through npm Trusted Publishing / GitHub OIDC. It does not
read `NPM_TOKEN`; if a bootstrap token was used for the first publish, revoke the
npm token and remove the GitHub Actions `NPM_TOKEN` secret after Trusted
Publishing is configured.

Before any publish step runs, the workflow audits the generated npm tarballs and
fails if they contain secret-like paths such as `.npmrc`, `.env*`, `.ssh/`,
`.aws/`, or key/certificate files. The publish step also redacts auth-shaped
output before writing logs.

## Automatic counter behavior

The workflow derives the upstream base version from `codex-rs/Cargo.toml` and
then looks for existing fork release tags in this repository:

- existing tags matching `rick-v0.122.0-rick.*` => next release becomes `0.122.0-rick.2`
- once the base version changes to `0.122.1`, the next release becomes `0.122.1-rick.1`

## Releasing

No manual version bump or tag juggling is required.

Merge or push to `stable` and the workflow will:

1. Read the base version from `codex-rs/Cargo.toml`.
2. Compute the next fork counter for that base release line.
3. Build the macOS release binaries with the derived display version.
4. Create the matching `rick-v<base>-rick.<n>` tag on the merge commit.
5. Generate GitHub release notes that separate fork changes from mainline
   Codex refreshes.
6. Create a GitHub release.
7. Publish the npm package.

## Release notes

Fork release notes are generated from first-parent history since the previous
`rick-v*` release tag:

- `Fork changes` lists PR merges and direct commits that are specific to this
  fork release line.
- `Mainline Codex` lists detected `stable-refresh/*` merges, reports whether
  the upstream base version changed, and links to the matching upstream Codex
  release notes.

Fork versions use prerelease semver suffixes like `-rick.3`, so npm requires
explicit dist-tags at publish time. The root `@rickgetz/codex` package is
published with `latest` so `npm install -g @rickgetz/codex` works normally, and
the Apple Silicon payload package is published with `darwin-arm64`.

If `stable` is at upstream `0.122.0` and there is already a release
`0.122.0-rick.1`, the next merge to `stable` will produce:

```bash
0.122.0-rick.2
```

Once upstream moves to `0.122.1`, the first merge to `stable` on that release
line will produce:

```bash
0.122.1-rick.1
```

## Install path

After publish, installs look like:

```bash
npm install -g @rickgetz/codex
codex-rick --version
```

The fork installs the executable as `codex-rick`, so the upstream
`@openai/codex` package can remain installed as `codex` for fallback use.

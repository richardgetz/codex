# codex-app-server-daemon

> `codex-app-server-daemon` is experimental and its lifecycle contract may
> change while the remote-management flow is still being developed.

`codex-app-server-daemon` backs the machine-readable `codex app-server`
lifecycle commands used by remote clients such as the desktop and mobile apps.
It is intended for Codex instances launched over SSH, including fresh developer
machines that should expose app-server with `remote_control` enabled.

## Platform support

The daemon supports Linux, macOS, and Windows using platform-specific process
and file-locking primitives. Windows startup requires a non-elevated terminal
whose host permits detached child processes.

Windows automatic attachment requires the canonical socket address to fit the
108-byte AF_UNIX limit (including its terminator). A short junction alias whose
resolved address exceeds that limit falls back to the embedded server. Use a
shorter `CODEX_HOME` to share the daemon; discovery does not trust a mutable alias.

Shared clients use the environment inherited when the daemon started. Opening a
new terminal or clearing variables there does not clear the running daemon's
environment; per-client environment isolation is not provided.
An invocation that sets `CODEX_EXEC_SERVER_URL` skips implicit daemon attachment
so its executor selection is preserved. If an implicitly discovered daemon cannot
initialize the connection, the TUI starts an embedded server instead. Explicit
`--remote` endpoints remain authoritative and report connection failures.

## Commands

```sh
codex app-server daemon start
codex app-server daemon restart
codex app-server daemon apply
codex app-server daemon recover
codex app-server daemon apply-status
codex app-server daemon enable-remote-control
codex app-server daemon disable-remote-control
codex app-server daemon stop
codex app-server daemon version
codex app-server daemon bootstrap --remote-control
```

On success, every command writes exactly one JSON object to stdout. Consumers
should parse that JSON rather than relying on human-readable text. Lifecycle
responses report the selected launcher path, launcher version, resolved backend,
socket path, local CLI version, and running app-server version when applicable.

## Bootstrap flow

For a new Linux or macOS machine:

```sh
curl -fsSL https://chatgpt.com/codex/install.sh | sh
$HOME/.codex/packages/standalone/current/codex app-server daemon bootstrap --remote-control
```

For a package-managed launcher, configure its stable executable path once. The
daemon keeps that path (including a symlink or shim) and resolves it again on
each start or restart, so an external upgrade is picked up without changing
the daemon settings:

```sh
codex-rick app-server daemon bootstrap \
  --codex-bin /opt/homebrew/bin/codex-rick
```

With no `--remote-control` flag, this configured-launcher form keeps the
app-server on its local Unix control socket and does not start the app-server's
remote-control websocket. It does not run the standalone installer or updater.
Run the launcher owner's update command (for example, `agent-manager upgrade
codex`) and then use `codex-rick app-server daemon restart` to roll the running
app-server onto the new version. Restart uses the existing graceful shutdown
window and never replays a turn; active work follows the normal app-server
shutdown and resume semantics. Pass `--remote-control` only when explicitly
requesting the app-server's existing remote-control behavior and service
endpoint.

On Windows, use a non-elevated PowerShell terminal whose host allows breakaway:

```powershell
irm https://chatgpt.com/codex/install.ps1 | iex
$codexHome = if ($env:CODEX_HOME) { $env:CODEX_HOME } else { Join-Path $HOME '.codex' }
& "$codexHome\packages\standalone\current\bin\codex.exe" app-server daemon bootstrap --remote-control
```

`bootstrap` records the daemon settings under
`CODEX_HOME/app-server-daemon/`, starts app-server as a pidfile-backed detached
process, and launches the detached updater loop only for the default standalone
selection.

## Installation and update cases

By default, the daemon uses the standalone installer (`install.sh` on Unix,
`install.ps1` on Windows) and its managed binary under
`CODEX_HOME/packages/standalone/current`: `bin/codex` or `bin/codex.exe`,
falling back to the legacy flat layout when present. A configured launcher from
`bootstrap --codex-bin` takes precedence.

| Situation | What starts | Does this daemon fetch new binaries? | Does a running app-server eventually move to a newer binary on its own? |
| --- | --- | --- | --- |
| Installer has run; only `start` is used | Managed binary | No | No; explicit restart is required. |
| Installer has run; `bootstrap` is used | Managed binary and detached updater | Yes; the platform's installer runs hourly. | Yes; after a successful update, a running app-server restarts with the new binary before the updater replaces itself. |
| Another tool updates the managed binary | Next start or restart uses it | Only with `bootstrap`, on its normal cadence. | With `bootstrap`, the next successful installer pass compares binary contents and refreshes a running app-server before the updater. |
| Configured launcher (for example, an npm shim) | The configured launcher path | No; the launcher owner controls updates. | No; run that owner's update command, then restart the daemon. |

### Standalone installs

For installs created by either platform's standalone installer:

- lifecycle commands always use the standalone managed binary path
- `bootstrap` is supported
- `bootstrap` starts a detached pid-backed updater loop that fetches via
  the platform's installer
- after a successful refresh, if app-server is running and the managed binary
  contents changed, the updater restarts app-server with that binary first and
  only then replaces its own process image
- the updater loop is not reboot-persistent; it must be started again by
  rerunning `bootstrap` after a reboot

### Configured launchers

A local `bootstrap --codex-bin PATH` selection is persisted in
`app-server-daemon/settings.json` as `managedCodexPath`. `PATH` must be an
absolute executable path. The daemon invokes that path directly with the usual
`app-server` arguments; it does not accept a launcher path from a remote
client. When this setting is present, standalone updater supervision is
intentionally disabled and `autoUpdateEnabled` is `false` in the bootstrap
response. Unless `--remote-control` is explicitly supplied, the app-server
uses only its local Unix control socket.

### Out-of-band updates

This daemon does not watch arbitrary executable files for replacement. For the
standalone selection, if some other tool updates the managed binary path:

- without `bootstrap`, a currently running app-server remains on the old
  executable image until an explicit `restart`
- with `bootstrap`, the detached updater loop notices the changed managed
  binary on its next successful scheduled installer pass; if app-server is
  running, it refreshes app-server first and then refreshes itself once that
  replacement starts successfully

For a configured launcher, run the launcher's normal update command and then
restart the daemon. The persisted launcher path is reused and re-evaluated, so
symlinks and shims can point to the newly installed version.

### Safe installed-version apply

apply is the daemon-wide update button for a locally installed launcher. It asks
the app-server coordinator to checkpoint every loaded root and child tree. The
daemon stops only after the coordinator returns a suspended receipt in which
every node is either suspended with its exact turn id or notActive with no turn
id. A durable apply-receipt.json is written before the old process is stopped;
the selected launcher is then started, and recover restores the recorded trees
by exact turn id. The command reports applied only after the coordinator reports
completed. During an active checkpoint or recovery, the JSON status is
inProgress; blocked checkpoints and start/recovery failures report
needsAttention and stay in the receipt for reconciliation with:

~~~sh
codex app-server daemon recover
codex app-server daemon apply-status
~~~

apply always prepares all loaded roots because replacing the daemon would
interrupt every tree. apply-status is read-only. These commands use the local
Unix control socket and do not enable remote control or enroll a cloud service.

## Lifecycle semantics

`start` is idempotent and returns after app-server is ready to answer the normal
JSON-RPC initialize handshake on the Unix control socket.

`restart` stops any managed daemon and starts it again.

`enable-remote-control` and `disable-remote-control` persist the launch setting
for future starts. If a managed app-server is already running, they restart it
so the new setting takes effect immediately.

Top-level `codex remote-control` bootstraps with `--remote-control` when the
updater loop is not running. Otherwise it enables remote control and starts the
daemon normally.

`stop` sends a graceful termination request first, then sends a second
termination signal after the grace window if the process is still alive.

All mutating lifecycle commands are serialized per CODEX_HOME, so a concurrent
start, restart, apply, recover, enable-remote-control, disable-remote-control,
stop, or bootstrap does not race another in-flight lifecycle operation. An
unresolved apply receipt blocks a new apply until recover reconciles it.

## State

The daemon stores its local state under `CODEX_HOME/app-server-daemon/`:

- `settings.json` for persisted launch settings, including an optional
  `managedCodexPath` selected by a local bootstrap command
- `app-server.pid` for the app-server process record
- `app-server-updater.pid` for the pid-backed standalone updater loop
- `daemon.lock` for daemon-wide lifecycle serialization
- apply-receipt.json for the latest checkpoint, selected launcher, and any
  recovery failure that needs attention

# codex-tailnet

Find the machines you can reach over SSH (your Tailscale tailnet plus the
aliases in `~/.ssh/config`), then attach the Codex TUI to Codex running on one
of them. This does not depend on OpenAI cloud features such as
`remote-control` or `tcp-tunnel`: the transport is plain SSH.

## Usage

```sh
# Show tailnet peers and ~/.ssh/config aliases, online machines first.
codex remote list
codex remote list --json

# Attach the TUI to Codex on a machine. HOST can be a MagicDNS name, a
# tailnet machine name (with MagicDNS), an IP, an ~/.ssh/config alias,
# user@host, or ssh://user@host:port.
codex remote attach devbox
codex remote attach me@devbox.tail1234.ts.net

# No HOST: pick from a numbered list of online machines.
codex remote attach

# The same connection without the helper:
codex --remote ssh://devbox
codex --remote ssh://me@100.64.0.2:2222
```

If `codex` is not on the remote machine's non-interactive `PATH` (common for
installs that only edit `~/.bashrc` or `~/.zshrc`), point at it:

```sh
codex remote attach devbox --remote-codex-bin ~/.codex/packages/standalone/current/codex
CODEX_REMOTE_BIN=/opt/codex/bin/codex codex --remote ssh://devbox
codex --remote 'ssh://devbox?codex_bin=/opt/codex/bin/codex'
```

Set `CODEX_TAILSCALE_BIN` if the `tailscale` CLI is not on `PATH`. On macOS the
CLI inside `/Applications/Tailscale.app` is also tried.

## How it works

1. **Discovery.** `tailscale status --json` is parsed into machines (`Self` plus
   the `Peer` map): MagicDNS name without its trailing dot, Tailscale IPs, OS,
   online state, and whether Tailscale SSH host keys are advertised. A
   `BackendState` other than `Running` becomes a clear error (`NeedsLogin` means
   "not logged in"; `Stopped` means "run `tailscale up`"). Concrete `Host`
   aliases from `~/.ssh/config` (and files it `Include`s) are added; an alias
   whose name or `HostName` matches a tailnet machine is attached to it so that
   `ssh` applies your user, port, and key settings. Results are deduplicated
   and sorted online first. If Tailscale is unavailable but the SSH config has
   hosts, those are listed with a warning.
2. **Daemon.** Before connecting, the client runs
   `ssh HOST -- codex app-server daemon start` on the remote machine. That
   command is idempotent and returns once the daemon's control socket
   (`CODEX_HOME/app-server-control/app-server-control.sock`) answers
   `initialize`. `codex app-server proxy` does not start the daemon on its own.
   If `daemon start` fails for a reason other than SSH or a missing `codex`,
   the client still tries the proxy (an app server you started yourself with
   `codex app-server --listen unix://` also works) and includes the warning in
   any connection error.
3. **Transport.** The client spawns `ssh HOST -- codex app-server proxy`, which
   relays the ssh session's stdio to that control socket. The control socket
   speaks WebSocket, so the client runs the same WebSocket handshake and
   JSON-RPC client it uses for `unix://` remotes, over the ssh child's
   stdin/stdout. The child is killed when the connection is dropped, and the
   TUI's normal reconnect logic spawns a new one. Your threads keep running in
   the remote daemon while you are detached.

## Security notes

- Every `ssh` call uses `-o BatchMode=yes` (never prompt: the TUI owns the
  terminal), `ConnectTimeout=10`, `RemoteCommand=none` and `RequestTTY=no` (a
  host alias cannot replace Codex's command or allocate a TTY),
  `ClearAllForwardings=yes`, and `ServerAliveInterval=15`. Authentication must
  work without prompting: keys, an SSH agent, or Tailscale SSH. A new host key
  must be accepted once with a plain `ssh HOST`.
- Options end with `--` before the host, hosts and users starting with `-` are
  rejected, and the remote command is quoted for a POSIX shell.
- SSH is the authentication. No bearer token is ever sent over an `ssh://`
  remote; `--remote-auth-token-env` is refused for it.
- The remote daemon runs as the SSH user with that user's `CODEX_HOME`,
  credentials, and sandbox settings. Anyone who can SSH in as that user can
  already drive it.
- Errors from `tailscale` are shortened and auth keys (`tskey-...`) are
  redacted.

## Gaps

- Not verified against a real tailnet or remote host here: tests use a fake
  `tailscale` and a fake `ssh` that runs the "remote" command locally.
- Remote hosts must run a POSIX shell for the SSH command (Windows OpenSSH
  hosts with `cmd.exe` or PowerShell are untested), and the remote Codex must
  support `app-server daemon start` and `app-server proxy`.
- `daemon start` requires a managed daemon install on the remote (see
  `app-server-daemon/README.md`, `codex app-server daemon bootstrap`); otherwise
  run an app server there yourself.
- Tailscale SSH in "check" mode needs a browser re-authentication that
  `BatchMode=yes` cannot complete; run `ssh HOST` once first.
- `known_hosts` entries are not offered (T3 Code lists them); `Match` blocks
  and tokens other than `%h` in `HostName` leave the resolved name unknown.
- `codex remote attach NAME` passes NAME to `ssh` verbatim; a short tailnet
  name resolves only when MagicDNS is enabled on this machine.
- Each connect (and each reconnect) costs two SSH sessions. An SSH
  `ControlMaster` setting in your config makes them cheap.

## Credits

The Tailscale status parsing and the SSH config discovery and hardening options
are ported from [T3 Code](https://github.com/pingdotgg/t3code)
(`packages/tailscale/src/tailscale.ts`, `packages/ssh/src/config.ts`,
`packages/ssh/src/command.ts`), MIT License, Copyright (c) 2026 T3 Tools Inc.

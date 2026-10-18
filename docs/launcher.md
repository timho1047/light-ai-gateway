# lgwl: the background launcher

**Status:** the contract for `src/bin/lgwl.rs` and `src/launcher.rs`: what the
launcher owns, where it records it, and when it will -- and will not -- signal a
process. It is a small supervisor, not a service manager.

**English** | [简体中文](launcher.zh-CN.md)

## What it is for

`lgw serve` is a foreground process. It is useful to keep it visible while
debugging, but inconvenient as the normal way to run a gateway: it owns a
terminal, and its log scrollback disappears with that terminal. `lgwl` starts
the same binary in the background, records the process identity, keeps its
output in a log file, and stops it on request.

`lgwl` is intentionally one managed instance. Starting a second one while the
first is running is refused with the running pid. Running multiple gateways on
different ports would need one state directory per instance; that is not
implemented.

## Commands

    lgwl launch [--config PATH]     start detached; refuses if one is already running
    lgwl status                     show pid, start time, arguments, cwd and log path
    lgwl log [-n 50] [--no-follow]  show/follow the log; Ctrl-C stops only the viewer
    lgwl exit                       stop: SIGTERM, then SIGKILL after the grace period
    lgwl update [--repo PATH] [--no-restart]
                                    reinstall, restarting if it was running

`--config` is passed through to `lgw`; without it, `lgw` uses its normal
resolution order relative to the directory `lgwl launch` was run from.

After starting, `launch` waits briefly for the startup line and prints the API
base (`.../v1`) and console URL. The address comes from the log, so a
configured port 0 still prints the port the kernel actually bound. `update`
prints the same pair when it restarts the gateway. `status` and a second `launch` (which is refused) read the
same line from the existing log and print the same URLs.

## Process identity

The launcher stores more than a pid. A state record contains the pid, the
process start time reported by `ps`, the launch arguments and the working
directory. Every later `status`, `exit` and `update` compares the start time
before signalling: a recycled pid is treated as a stale record, not as the
gateway, so an unrelated process is never killed.

The child is started with `setsid`, stdin on `/dev/null`, and stdout/stderr
appended to the log. Closing the terminal or pressing Ctrl-C in it does not
stop the gateway.

## Stop semantics

`lgwl exit` sends SIGTERM. `lgw serve` handles SIGTERM (and SIGINT) by
stopping the listener, letting in-flight work finish, then flushing the ledger
and writing its final log line. A streaming request that never ends must not
hold the process forever, so the server flushes and exits after eight seconds.
If the process still exists after ten seconds, the launcher sends SIGKILL and
says so. `update` uses the same stop path before changing the binary.

Only `lgwl exit` is the supported way to stop it. A manual `kill` can still
terminate it; the next `launch` notices the stale record and replaces it.

## State, logs and overrides

    ~/.light-ai-gateway/lgwl/state.json       launch record (pid, start time, args, cwd)
    ~/.light-ai-gateway/lgwl/lgw.log         stdout + stderr of lgw serve
    ~/.light-ai-gateway/lgwl/lgwl.lock       serialises launch/exit/update
    ~/.light-ai-gateway/lgwl/install.conf    written by scripts/lgw-install.sh

`LGWL_STATE_DIR` moves that directory (useful for tests and isolated
profiles). `LGW_BIN` selects a specific `lgw` binary; by default the binary
next to `lgwl` is used, not a PATH lookup.

The log is mode 0600 and the state directory 0700. On launch it is rotated to
`lgw.log.1` when it exceeds 10 MiB; `lgwl log` follows across that rotation.

## Update

`lgwl update` does not duplicate the installer. It runs the repository's
`scripts/lgw-install.sh`, which builds, signs (on macOS) and installs both
binaries, then writes `install.conf` with the repository path, signing identity
and lgw path.

The repository is found from, in order: `--repo PATH`, `$LGW_REPO`, the path
recorded by the installer, then the current directory and its ancestors. If it
cannot be found, the command fails with instructions instead of guessing.

If the gateway was running, it is stopped before the installer starts. A
successful install restarts it with the same arguments, cwd and log,
`--no-restart` keeps it stopped. A failed install leaves it stopped and says
so; the old binary remains in place unless the installer replaced it.

## Limits and deliberate choices

- Unix only: the launcher uses `setsid`, signals and `flock`.
- It is not a service manager: no launch-at-login and no automatic restart
  after a crash. Those belong to launchd/systemd; adding them here would turn a
  small launcher into a partially implemented supervisor.
- It does not manage multiple gateways with different configs on one machine.
- Log rotation is one generation of 10 MiB. Archive older logs externally if
  they matter.

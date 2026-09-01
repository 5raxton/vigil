# Vigil design notes

This document records the architectural decisions in Vigil. It is written for
operators who want to understand *why* things behave the way they do, and for
future maintainers who want to change them safely.

## Process tree and "second chance" model

```
vigil (PID 1)
  └── vigil-scan                       # service manager
        ├── vigil-supervise (syslog)   # one supervisor per service
        │     ├── syslog-ng            # the service process
        │     └── vigillog             # log pipe consumer
        ├── vigil-supervise (sshd)
        │     └── sshd
        └── ...
```

- **PID 1 never runs service code.** It reaps zombies, routes signals, mounts
  the early filesystems, and spawns/restarts `vigil-scan`.
- **Supervisors are disposable.** If a `vigil-supervise` dies, `vigil-scan`
  reaps it, checks its status dir to decide whether the service was alive
  ("second chance"), and respawns the supervisor (not the service). This is the
  same relationship `vigil` has with `vigil-scan`. A crash in any one process
  therefore never takes down the machine or unrelated services. Each service is
  started with `PR_SET_PDEATHSIG(SIGKILL)`, so a supervisor that dies (even
  mid-run, e.g. SIGKILLed or panicking) takes its service with it — a respawned
  supervisor always starts a fresh service and can never leave an orphaned
  duplicate of the old one running under a reaper.

## Privilege separation

Layout (see `src/util.rs::exec_search_paths`): subsidary binaries are found by
searching `$ORIGIN` of the current executable, then `/usr/local/bin`,
`/usr/sbin`, `/sbin`, `/usr/bin`, `/bin`. Installation into `/usr/sbin` works
from any of those roots. No PATH dependency at boot.

`vigil-scan` is launched by PID 1; supervisors are launched by vigilant-scan;
`vigillog` is launched by `vigil-supervise` inside a fork, so each layer can
pipeline the next without a shared config lookup.

## Supervision loop

`vigil-supervise` runs a tight event loop that:

1. binds `[socket]` listeners (so sockets exist before the service);
2. spawns `vigillog` for the configured logging kind and connects a pipe;
3. forks the service, remaps the pre-bound descriptors to `3..n`, sets
   `LISTEN_FDS`/`LISTEN_PID`, applies rlimits, drops privileges
   (`setgroups`/`initgroups`, `setgid`, `setuid` in that order), chdirs, and
   execs;
4. polls readiness (default 30 s, tunable), treating failure like any other
   abnormal exit;
5. on exit, runs the optional `<config_dir>/<service>/finish` script, writes
   the status dir (`state`, `pid`, `restarts`), and sleeps with exponential
   backoff before restarting, subject to `max_restarts`;
6. in parallel reaps its own children and answers signals.

The control loop and the child waiter both poll at 50 ms — fast enough for
realtime-feeling restarts while remaining a constant, allocation-free hot path.

## Readiness checks

| kind  | condition |
|-------|-----------|
| `none` | ready immediately after exec |
| `pid`  | the file named by `check` exists and parses to a live PID |
| `socket` | reconnecting to the first `[socket]` listen target succeeds |
| `signal` | the child raises the configured signal (default `USR1`); `TERM`/`INT`/`HUP` are rejected as collisions |
| `exec`  | `/bin/sh -c check` exits 0, each run capped at 5 s to avoid wedging the loop |

A service that fails readiness under `restart.policy = on-failure` counts as a
failure; under `on-abnormal` a readiness failure still restarts. `never` stops.

## Socket activation

`[socket]` entries are parsed by `src/sockspec.rs` into `ListenSpec` values.
Hosts must be IP literals: name resolution via NSS/resolver is intentionally
not available to PID-1-spawned code at boot, and depending on it would make
startup order-dependent. `tcp:`/`udp:`/`unix:` prefixes set the protocol; a bare
`port` or `host:port` defaults to the `socket_type` (tcp).

The supervisor binds before forking, then the child receives the descriptors
remapped to `3..n` with:

- `LISTEN_FDS` = number of descriptors,
- `LISTEN_PID` = the child PID,
- `VIGIL_SUPERVISOR_PID` = the supervising process's PID.

This matches the systemd convention, so unmodified systemd-aware daemons (sshd,
systemd socket services) accept the descriptors. Unix socket paths are unlinked
before bind so a crashed prior instance cannot wedge the path.

## cgroup v2 CPU shares

`resource_limits.cpu_shares` is converted to the `cpu.weight` controller using
the canonical systemd mapping `weight = 1 + shares * 9999 / 2^18`, clamped to
the kernel's `[1, 10000]` range. The supervisor walks `/sys/fs/cgroup`, writes
the weight into `cpu.weight`, and relocates the service into the new
`vigil-<name>.scope` child cgroup so the daemon can keep managing its own
subtree. When cgroup v2 is absent (hosted kernels, early boot before cgroupfs
mount) the whole step is skipped with a log line — resource limits must never
be able to fail a boot.

## Boot targets and degradation

`vigil-scan` at boot (and on reload) resolves the active target:

- `[target] requires` entries are always enabled, even if omitted from the
  services map, and are closed under their `dependencies[].required` edges
  (a required dependency of a required service becomes required).
- `enabled = true` (non-`optional`) entries are required.
- `wants` dependencies are enabled unless the target explicitly disables them.
- When a required service exhausts its restarts, the boot is declared
  **degraded**: the event loop logs a `DEGRADED` line and writes
  `<runtime>/degraded`.

Degradation is deliberately *soft* at boot (other services continue), but
visible both in the supervisor log and on disk. There is no watchdog failure
action yet (a future feature).

## Logging

`vigillog` supports three real modes, selected by `logging.kind`:

- `pipe` (default): the supervisor connects a pipe; vigillog consumes it and
  rotates files in the service's log directory (`current`, `1`, `2`, …).
- `file`: vigillog appends directly to the configured path (`app.log` with
  `app.log.N` suffixes, or the default log directory with `current`).
- `syslog`: vigillog writes RFC 3164 messages (`<PRI>TAG[pid]:`) to
  `/dev/log` with a `file` fallback.
- `none`: the service's stdio goes to `/dev/null`.

Rotation keeps an open descriptor and tracks byte counts in memory — no
per-line `stat`. `vigil-ctl log` resolves the *effective* file per logging kind
(`pipe`/`file` only); syslog/none services return an error, since there is no
scannable file.

## Shutdown

`vigil-ctl reboot|poweroff|halt` sends a JSON `Shutdown` request; `vigil-scan`
forwards a signal to PID 1 (`SIGTERM` reboot, `SIGUSR1` halt, `SIGUSR2`
poweroff); `vigil` chain-loads `/sbin/shutdown`/`/usr/sbin/shutdown` (falling
back to `reboot(2)`). `vigil-scan` stops services in reverse dependency order
with the standard TERM → grace → KILL sequence before signaling PID 1.

## Signal map (PID 1)

| Signal | Action |
|--------|--------|
| `SIGCHLD` | reap, respawn dead scanner |
| `SIGINT`/`SIGTERM` | reboot |
| `SIGUSR1` | halt |
| `SIGUSR2` | poweroff |
| `SIGPWR` | poweroff |
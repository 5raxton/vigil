# Vigil

A fast, memory-safe Linux init system and process supervisor written in Rust.

Vigil combines the best ideas from decades of init system design — the supervision model of daemontools/runit/s6, the dependency awareness of dinit, and the minimalism of sinit — into a single, coherent, auditable codebase. It keeps PID 1 small, allocates nothing in its core loop, and relies on a compact event loop rather than a heavyweight bus.

## Philosophy

Vigil is built around four guiding principles:

1. **PID 1 stays small.** The actual init process only reaps zombies, routes signals, and chain-loads the scanner. All real supervision lives one level down, so a crash in a supervisor can never brick the machine — the init process restarts it (a "second-chance" model).
2. **Everything is a process.** Services are supervised by dedicated lightweight supervisors. No single point of failure.
3. **Correct teardown.** `vigil-ctl stop/restart` and system shutdown terminate the whole service process tree (the service and any children it spawned), not just the supervisor — so nothing is left orphaned. Services stop in reverse dependency order.
4. **Original, not a clone.** Vigil doesn't copy runit or s6. It takes the strongest ideas from each system and reinvents them with a clean, dependency-aware, event-driven architecture.

## Architecture

```
                ┌─────────────────────────────┐
                │          vigil (PID 1)      │
                │   · reap zombies            │
                │   · route signals           │
                │   · mount /dev /proc etc.   │
                │   · chain-load & respawn    │
                └──────────────┬──────────────┘
                      spawn/restart (second chance)
                               ▼
                ┌─────────────────────────────┐
                │         vigil-scan          │
                │   · load TOML services      │
                │   · build dependency graph  │
                │   · supervise tree          │
                │   · control socket / RPC    │
                └───────┬────────────┬────────┘
                  spawn │            │ (read departures)
                        ▼            ▼
        ┌──────────────────────┐   ┌───────────────────┐
        │    vigil-supervise   │   │  vigil-ctl (CLI)  │
        │   one per service    │   │  over Unix socket │
        │  · run / restart     │   └───────────────────┘
        │  · backoff / limits  │
        │  · reaps child       │
        └──────────┬───────────┘
                   │ stdio pipe
                   ▼
        ┌──────────────────────┐
        │        vigillog      │
        │  rotate by size/time │
        └──────────────────────┘
```

- **`vigil`** — PID 1. Survives supervisor failures by design; respawns failed supervisors.
- **`vigil-scan`** — the service manager / supervisor tree root. Event-driven.
- **`vigil-supervise`** — one per running service. Restarts with exponential backoff, enforces resource limits, and reaps its own children.
- **`vigil-ctl`** — the control plane CLI (`start`, `stop`, `restart`, `status`, `list`, `log`, ...).
- **`vigillog`** — per-service log rotator fed by a live pipe, so logs are never lost and rotation never requires stopping a service.

## Features

- **Dependency-aware startup** — requirement vs. ordering separated (`after` / `before` / `wants`), topological sort with cycle detection, run in dependency order for teardown.
- **Boot targets** — `default_target` + per-target service enablement under `/etc/vigil/targets/`.
- **Smart restart policy** — `always` / `on-failure` / `on-abnormal` / `never`, with exponential backoff and a restart ceiling to stop crash loops.
- **Exponential backoff** — initial delay grows geometrically up to a cap; both the delay and the restart tally reset after a stable run.
- **Supervisor resilience** — a crashed supervisor is detected and respawned (a "second chance"), so one bad supervisor can never take down unrelated services; PID 1 likewise restarts the scanner if it ever dies.
- **Resource limits** — max open files, processes, and address space per service.
- **Per-service logging** — size- and count-based rotation via a live pipe; never lossy; never stops the service.
- **Unix-socket control plane** — a simple, documented JSON-over-socket protocol (no D-Bus).
- **Config reload** — `SIGHUP` or `vigil-ctl reload` reloads service definitions.
- **Graceful teardown** — reverse-dependency order; `stop`/`restart` SIGTERM the supervisor, which in turn terminates the service and its entire process group, then waits through the configured TERM→KILL grace.
- **Correct service PID reporting** — `vigil-ctl status`/`list` report the real service PID (from the status dir), not the supervisor's PID.
- **No dynamic allocation in supervision cores**, compact event-driven loops.

## Implementation status

The following subsystems are **fully implemented** and exercised by the control-plane and supervision code:

- Process supervision with exponential backoff, restart ceiling, and per-service process-group teardown
- Dependency-ordered start and reverse-dependency teardown
- Boot targets (`default_target`/`target_dir`)
- Shutdown/reboot/halt/poweroff via `vigil-ctl` (scanner signals PID 1, which performs the action)
- Per-service logging with size/count rotation (`vigillog`)
- Unix-socket JSON control plane (`vigil-ctl`), config reload (`SIGHUP`)
- Resource limits (open files, processes, address space)

The following fields are **reserved but not yet implemented**: `service.readiness` (pid/socket/signal/exec readiness checks), `service.resource_limits.cpu_shares` (cgroup), socket activation (`[socket]`), and the `[target] requires` / per-service `optional` flags. They are accepted by the parser for forward-compatibility but currently have no effect.

`logging.kind = "file"` and `"syslog"` are currently routed through the same per-service, size/count-rotated pipe log as `"pipe"` (written under `logging.path`, defaulting to `<log_dir>/<name>/`); native syslog forwarding is not yet implemented. `logging.timestamp` and `service.shutdown.kill_signal` are honored.

## Requirements

- Rust **1.87+** (edition 2021)
- Linux (tested on the process supervisor, process-group, and reaper APIs)
- For use as the system init, install as `/sbin/init` and run as PID 1

## Building

```sh
cargo build --release
```

All five binaries are produced in `target/release/`:

```
vigil           # PID 1
vigil-scan      # service manager / supervisor root
vigil-supervise # per-service supervisor
vigil-ctl       # control CLI
vigillog        # log rotator
```

## Installation (as system init)

```sh
cargo build --release
install -Dm755 target/release/vigil          /sbin/init
install -Dm755 target/release/vigil-scan     /usr/sbin/vigil-scan
install -Dm755 target/release/vigil-supervise /usr/sbin/vigil-supervise
install -Dm755 target/release/vigillog       /usr/sbin/vigillog
install -Dm755 target/release/vigil-ctl      /usr/sbin/vigil-ctl
```

Point the kernel at it with the `init=` parameter and let Vigil mount `/dev`, `/proc`, `/sys`, and `/run` at boot.

## Configuration

Vigil configuration lives under `/etc/vigil/`.

### Global configuration (`/etc/vigil/vigil.toml`)

```toml
service_dir    = "/etc/vigil/services"
target_dir     = "/etc/vigil/targets"
control_socket = "/run/vigil/control.sock"
log_dir        = "/var/log/vigil"
runtime_dir    = "/run/vigil"
default_target = "default"
hostname       = "vigil-box"
```

### Services (`/etc/vigil/services/<name>.toml`)

Each file defines one supervised service:

```toml
[service]
description = "Secure shell daemon"
command     = "/usr/bin/sshd"
args        = ["-D"]
user        = "root"
group       = "root"
working_dir = "/"

[service.environment]
LANG = "C.UTF-8"

[service.readiness]
type       = "socket"      # none | pid | socket | signal | exec
timeout_ms = 5000

[service.restart]
policy            = "on-failure"  # always | on-failure | on-abnormal | never
max_restarts      = 10
backoff_initial_ms = 1000
backoff_max_ms     = 30000
backoff_multiplier = 2.0

[service.shutdown]
signal     = "TERM"
timeout_ms = 5000
kill_signal = "KILL"

[service.resource_limits]
max_files    = 1024
max_procs    = 256
max_memory_mb = 512

[logging]
type        = "pipe"   # pipe | file | syslog | none
max_size_mb = 10
max_files   = 5
timestamp   = true

[[dependencies]]
service  = "ntpd"
type     = "after"    # after | before | wants
required = true
```

### Targets (`/etc/vigil/targets/<target>.toml`)

Targets group services into boot states (the analog of runlevels or systemd targets):

```toml
[target]
description = "Default multi-user"

[services]
"syslog"  = { enabled = true }
"ntpd"    = { enabled = true }
"sshd"    = { enabled = true }
"dbus"    = { enabled = true, optional = true }
```

The `default_target` from the global config is brought up at boot.

## Usage

Control the system with `vigil-ctl`:

```sh
vigil-ctl list                      # list all services
vigil-ctl status                    # status of all services
vigil-ctl status sshd               # status of one service
vigil-ctl start sshd                # start a service
vigil-ctl stop sshd                 # stop a service
vigil-ctl restart sshd              # restart a service
vigil-ctl log sshd -n 100           # read the last 100 log lines
vigil-ctl reload                    # reload service definitions
vigil-ctl ping                      # check the daemon is alive
vigil-ctl reboot                    # request reboot
vigil-ctl poweroff                  # request power-off
vigil-ctl halt                      # request halt
```

You can point `vigil-ctl` at a different socket with `-s` or `VIGIL_SOCK`.

## Signal map (PID 1)

| Signal | Action |
|--------|--------|
| `SIGCHLD` | reap zombies, respawn dead supervisor |
| `SIGINT` / `SIGTERM` | reboot |
| `SIGUSR1` | halt |
| `SIGUSR2` | poweroff |
| `SIGPWR` | power-fail (poweroff) |

The scanner responds to `SIGHUP` with a config reload.

## Project layout

```
src/
  lib.rs            # shared constants
  config.rs         # TOML configuration types
  dep.rs            # dependency graph + topological sort
  protocol.rs       # control-plane RPC protocol
  bin/
    vigil.rs        # PID 1
    vigil-scan.rs   # service manager / supervisor root
    vigil-supervise.rs  # per-service supervisor
    vigil-ctl.rs    # control CLI
    vigillog.rs     # log rotator
examples/           # sample configuration
```

## Testing

```sh
cargo test
```

## License

Vigil is licensed under the GNU General Public License, version 3 or later. See [LICENSE](LICENSE).

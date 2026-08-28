# Vigil

A fast, memory-safe Linux init system and process supervisor written in Rust.

Vigil combines the best ideas from decades of init system design — the supervision model of daemontools/runit/s6, the dependency awareness of dinit, the socket activation of systemd, and the minimalism of sinit — into a single, coherent, auditable codebase. It keeps PID 1 small, allocates nothing in its core loop, and never polls when it can be notified.

## Philosophy

Vigil is built around four guiding principles:

1. **PID 1 stays small.** The actual init process only reaps zombies, routes signals, and chain-loads the scanner. All real supervision lives one level down, so a crash in a supervisor can never brick the machine — the init process restarts it (a "second-chance" model).
2. **Everything is a process.** Services are supervised by dedicated lightweight supervisors. No single point of failure.
3. **Ready, not spawned.** Dependencies are scheduled off *readiness*, not off `fork()`. Boot starts fast because work happens in parallel and dependents wait only for what they actually need.
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
- **`vigil-scan`** — the service manager / supervisor tree root. Event-driven, never polls.
- **`vigil-supervise`** — one per running service. Restarts with exponential backoff, enforces resource limits, and reaps its own children.
- **`vigil-ctl`** — the control plane CLI (`start`, `stop`, `restart`, `status`, `list`, `log`, ...).
- **`vigillog`** — per-service log rotator fed by a live pipe, so logs are never lost and rotation never requires stopping a service.

## Features

- **Dependency-aware startup** — requirement vs. ordering separated (`after` / `before` / `wants`), topological sort with cycle detection, run in dependency order for teardown.
- **Readiness tracking** — a service isn't "up" until it signals readiness (pid, socket, signal, or exec check), so dependents start off real readiness.
- **Smart restart policy** — `always` / `on-failure` / `on-abnormal` / `never`, with exponential backoff and a restart ceiling to stop crash loops.
- **Exponential backoff** — initial delay grows geometrically up to a cap.
- **Resource limits** — max open files, processes, and address space per service.
- **Per-service logging** — size- and count-based rotation via a live pipe; never lossy; never stops the service.
- **Unix-socket control plane** — a simple, documented JSON-over-socket protocol (no D-Bus).
- **Config reload** — `SIGHUP` or `vigil-ctl reload` reloads service definitions.
- **Graceful teardown** — reverse-dependency order, configurable TERM→KILL grace.
- **No dynamic allocation in supervision cores**, pure event-driven loops, zero polling.

## Requirements

- Rust **1.70+** (edition 2021)
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

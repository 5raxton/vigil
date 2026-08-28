# Vigil

A fast, memory-safe Linux init system and process supervisor written in Rust.

Vigil combines the best ideas from decades of init system design — the supervision model of daemontools/runit/s6, the dependency awareness of dinit, cgroup v2 resource control, and the minimalism of sinit — into a single, coherent, auditable codebase. It keeps PID 1 small, allocates nothing in its core loop, and relies on a compact event loop rather than a heavyweight bus.

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
                │   · resolve boot target     │
                │   · supervise tree          │
                │   · control socket / RPC    │
                └───────┬────────────┬────────┘
                  spawn │            │ (read departures)
                        ▼            ▼
        ┌──────────────────────┐   ┌───────────────────┐
        │    vigil-supervise   │   │  vigil-ctl (CLI)  │
        │   one per service    │   │  over Unix socket │
        │  · run / restart     │   └───────────────────┘
        │  · readiness checks  │
        │  · cgroup v2 limits  │
        │  · socket activation │
        │  · reaps child       │
        └──────────┬───────────┘
                   │ stdio pipe
                   ▼
        ┌──────────────────────┐
        │        vigillog      │
        │  pipe / file / syslog│
        └──────────────────────┘
```

- **`vigil`** — PID 1. Survives supervisor failures by design; respawns failed supervisors.
- **`vigil-scan`** — the service manager / supervisor tree root. Resolves the boot target, applies dependency ordering, and runs the control socket. Event-driven.
- **`vigil-supervise`** — one per running service. Applies readiness checks, cgroup v2 cpu limits, socket activation, and privilege drop, restarts with exponential backoff, and reaps its own children.
- **`vigil-ctl`** — the control plane CLI (`start`, `stop`, `restart`, `status`, `list`, `log`, ...).
- **`vigillog`** — per-service log rotator fed by a live pipe (pipe/file/syslog modes), so logs are never lost and rotation never requires stopping a service.

## Features

- **Dependency-aware startup** — requirement vs. ordering separated (`after` / `before` / `wants`), topological sort with cycle detection, run in reverse dependency order for teardown.
- **Boot targets** — `default_target` + per-target service enablement under `/etc/vigil/targets/`.
- **`[target] requires` + per-service `optional`** — a target can hard-require services (pulled in and closed under their required dependencies even if absent from the services map) and mark entries optional; if any required service gives up, vigil-scan flags the boot as **degraded** and writes `<runtime>/degraded`.
- **Readiness checks** — `pid` (pidfile), `socket` (connect probe), `signal` (e.g. `USR1`, checked against the process), and `exec` (a `/bin/sh -c` check command, with a per-run timeout), each with a configurable overall timeout. A service that never becomes ready is treated as failed and restarted per policy.
- **Socket activation** — `[socket]` listen specs (TCP/UDP/Unix) are bound by the supervisor *before* exec and handed to the service as descriptors `3..n` with the standard `LISTEN_FDS`/`LISTEN_PID` environment, so daemons like sshd start instantly and bind only once.
- **cgroup v2 CPU shares** — `resource_limits.cpu_shares` maps to the `cpu.weight` controller (systemd's canonical `shares → weight` mapping); the supervisor silently skips it when cgroup v2 is unavailable rather than failing the boot.
- **Smart restart policy** — `always` / `on-failure` / `on-abnormal` / `never`, with exponential backoff and a restart ceiling to stop crash loops.
- **Supervisor resilience** — a crashed supervisor is detected and respawned (a "second chance"), so one bad supervisor can never take down unrelated services; PID 1 likewise restarts the scanner if it ever dies.
- **Resource limits** — max open files, processes, address space, and cgroup CPU shares per service, applied with `setrlimit` before dropping privileges.
- **Privilege drop** — services run as the configured `user`/`group` (primary group fallback, supplementary groups re-initialized via `initgroups`).
- **Per-service logging** — three real modes: `pipe` (live pipeline into size/count-rotated files), `file` (supervisor opens/rotates the file directly), and `syslog` (RFC 3164 messages to `/dev/log`), plus `none`. Never lossy; never stops the service.
- **Unix-socket control plane** — a simple, documented JSON-over-socket protocol (no D-Bus).
- **Config reload** — `SIGHUP` or `vigil-ctl reload` reloads service definitions and re-applies the boot target.
- **Graceful teardown** — reverse-dependency order; `stop`/`restart` SIGTERM the supervisor, which in turn terminates the service and its entire process group, then waits through the configured TERM→KILL grace.
- **Correct service PID reporting** — `vigil-ctl status`/`list` report the real service PID (from the status dir), not the supervisor's PID.
- **No dynamic allocation in supervision cores**, compact event-driven loops.

## Requirements

- Rust **1.87+** (edition 2021)
- Linux (tested on the process supervisor, process-group, cgroup v2, and reaper APIs)
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

Point the kernel at it with the `init=` parameter and let Vigil mount `/dev`, `/proc`, `/sys`, and `/run` at boot. Subsidary binaries are located by searching the current executable's directory and then `/usr/local/bin`, `/usr/sbin`, `/sbin`, `/usr/bin`, `/bin`, so the layout above works from any of them.

## Configuration

Vigil configuration lives under `/etc/vigil/`. A complete, commented example tree is in [`examples/`](examples/); the parser tests keep it in sync.

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
signal     = "USR1"        # only for type = "signal"
check      = "/path/to/check"  # only for type = "exec", run via /bin/sh -c

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
max_files     = 1024
max_procs     = 256
max_memory_mb = 512
cpu_shares    = 512   # cgroup v2 cpu.weight (1 = 2 .. 262144 = 10000)

[logging]
type        = "pipe"   # pipe | file | syslog | none
path        = "/var/log/vigil/sshd"   # optional; defaults log dir
max_size_mb = 10
max_files   = 5
timestamp   = true

[socket]
listen      = ["tcp:22", "unix:/run/sshd.sock"]
socket_type = "tcp"    # default protocol for specs without a prefix

[[dependencies]]
service  = "ntpd"
type     = "after"    # after | before | wants
required = true
```

Readiness semantics:

| type     | becomes ready when |
|----------|--------------------|
| `none`   | immediately after exec |
| `pid`    | the pidfile given by `check` exists and contains a live PID |
| `socket` | a connection to the first `[socket]` listen endpoint succeeds |
| `signal` | the service raises the signal from `signal` (default `USR1`); collisions with `TERM`/`INT`/`HUP` are rejected |
| `exec`   | `/bin/sh -c check` exits 0 within the per-run cap |

Socket activation follows the systemd convention: the supervisor binds every `[socket]` listen spec before exec, remaps the descriptors to `3..n` in the child, and exports `LISTEN_FDS`, `LISTEN_PID`, and `VIGIL_SOCK` (`supervisor=<pid>`). Hosts in listen specs must be IP literals — name resolution is deliberately avoided at boot.

### Targets (`/etc/vigil/targets/<target>.toml`)

Targets group services into boot states (the analog of runlevels or systemd targets):

```toml
[target]
description = "Default multi-user"
requires    = ["network"]

[services]
"syslog"  = { enabled = true, optional = true }
"ntpd"    = { enabled = true }
"sshd"    = { enabled = true }
```

- Entries in `requires` are always enabled — even if omitted from the services map — and are closed under their `required` dependencies.
- A `wants`-dependency is enabled unless the target explicitly disables that service (`{ enabled = false }`).
- Any `enabled = true` (non-`optional`) entry is required: if a required service exhausts its restarts, vigil-scan logs **DEGRADED** and writes `<runtime>/degraded` naming the service. A missing target file enables every service.

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
  sockspec.rs       # listen-spec parsing + socket binding (readiness/socket activation)
  util.rs           # shared exec search-path helper
  bin/
    vigil.rs        # PID 1
    vigil-scan.rs   # service manager / supervisor root
    vigil-supervise.rs  # per-service supervisor
    vigil-ctl.rs    # control CLI
    vigillog.rs     # log rotator
examples/           # sample configuration (parsed by the test suite)
docs/               # design notes
```

## Testing

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

Unit tests cover config parsing (against `examples/`), the dependency sorter, dependency/degradation logic decisions, listen-spec parsing and socket bind/connect roundtrips, `shares → weight` conversion endpoints, readiness signal resolution, and log rotation edge cases.

## License

Vigil is licensed under the GNU General Public License, version 3 or later. See [LICENSE](LICENSE).
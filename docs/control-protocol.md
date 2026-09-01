# Control-plane protocol

`vigil-ctl` and `vigil-scan` communicate over a Unix stream socket (default
`/run/vigil/control.sock`) using length-prefixed JSON (newline-free, so frames
never need escaping).

## Framing

Every message is:

```
4-byte little-endian payload length  |  JSON payload
```

- Reads enforce a 1 MiB maximum payload.
- Requests and Responses are serialized `enum`s; an unknown variant is a
  protocol error and the connection is closed.
- On success the scanner accepts the connection; there is no auth beyond Unix
  socket permissions (chmod 0600, root by default).

## Requests → Responses

| Request | Payload | Response |
|---------|---------|----------|
| `Status` | `{ "service": "name" | null }` | `Status(ServiceStatus)` or `List(Vec<ServiceInfo>)` |
| `List` | — | `List(Vec<ServiceInfo>)` |
| `Start` | `{ "service": "name" }` | `Ok` / `Error` |
| `Stop` | `{ "service": "name" }` | `Ok` / `Error` |
| `Restart` | `{ "service": "name" }` | `Ok` / `Error` |
| `Log` | `{ "service": "name", "lines": n }` | `LogLines(Vec<String>)` or `Error` (incl. syslog/none logging) |
| `Reload` | — | `Ok` / `Error` |
| `Shutdown` | `{ "action": "reboot" | "poweroff" | "halt" }` | `Ok` |
| `Ping` | — | `Pong` |

### `ServiceStatus`

```json
{
  "name": "sshd",
  "state": "running",
  "pid": 1234,
  "uptime_secs": 120,
  "restart_count": 2,
  "description": "Secure shell daemon",
  "command": "/usr/bin/sshd"
}
```

`pid` is the real service PID (read from the supervisor's status dir), not the
supervisor's.

### `ServiceInfo`

```json
{ "name": "sshd", "state": "running", "pid": 1234, "description": "..." }
```

## Wire example

```
> 4a 00 00 00  {"Status":{"service":"sshd"}}
< ee 00 00 00  {"Status":{"name":"sshd","state":"running",...}}
```
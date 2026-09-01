use anyhow::{bail, Context, Result};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::unix::io::IntoRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::Duration;

/// Normalized description of a listening endpoint declared in
/// `[socket] listen` or used as a readiness connect target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenSpec {
    Tcp { addr: SocketAddr },
    Udp { addr: SocketAddr },
    Unix { path: PathBuf },
}

/// Default protocol used when a listen spec carries no `tcp:`/`udp:`/`unix:`
/// prefix.
pub const DEFAULT_SOCKET_TYPE: &str = "tcp";

pub fn default_type() -> &'static str {
    DEFAULT_SOCKET_TYPE
}

/// Parse a single listen spec.
///
/// Supported forms (an optional `tcp:`/`udp:`/`unix:` prefix overrides
/// `default_type`):
///
/// - `22` / `tcp:22` — TCP on port 22, any IPv4 address
/// - `tcp:127.0.0.1:8000` — TCP on a specific address
/// - `udp:53` — UDP on port 53
/// - `[::]:80` / `tcp:[::]:80` — TCP on any IPv6 address
/// - `unix:/run/x.sock` — Unix stream socket
///
/// Hosts must be IP literals. Name resolution is deliberately *not*
/// performed: at boot time the resolver / NSS may not be available yet, so
/// depending on it would make startup order-dependent and fragile.
pub fn parse_listen_spec(spec: &str, default_type: &str) -> Result<ListenSpec> {
    let spec = spec.trim();
    if spec.is_empty() {
        bail!("empty listen spec");
    }

    let (proto, rest) = match spec.split_once(':') {
        Some((p, r)) if matches!(p, "tcp" | "udp" | "unix") => (p, r),
        _ => (default_type, spec),
    };

    match proto {
        "tcp" | "udp" => {
            let (host, port) = split_host_port(rest)?;
            let ip = parse_ip(&host)?;
            let addr = SocketAddr::new(ip, port);
            if proto == "tcp" {
                Ok(ListenSpec::Tcp { addr })
            } else {
                Ok(ListenSpec::Udp { addr })
            }
        }
        "unix" => {
            if rest.is_empty() {
                bail!("unix socket path is empty");
            }
            Ok(ListenSpec::Unix {
                path: PathBuf::from(rest),
            })
        }
        other => bail!("unsupported socket type '{}'", other),
    }
}

/// Parse a host:port string. `[v6]:port` and bare `port` are accepted.
fn split_host_port(rest: &str) -> Result<(String, u16)> {
    let rest = rest.trim();

    if let Some(stripped) = rest.strip_prefix('[') {
        let idx = stripped
            .find(']')
            .ok_or_else(|| anyhow::anyhow!("unterminated '[' in listen spec '{}'", rest))?;
        let host = stripped[..idx].to_string();
        let port_str = stripped[idx + 1..].strip_prefix(':').unwrap_or("");
        let port = parse_port(port_str)?;
        return Ok((host, port));
    }

    if let Some((host, port_str)) = rest.rsplit_once(':') {
        let host = if host.is_empty() { "0.0.0.0" } else { host };
        let port = parse_port(port_str)?;
        Ok((host.to_string(), port))
    } else {
        let port = parse_port(rest)?;
        Ok(("0.0.0.0".to_string(), port))
    }
}

fn parse_port(s: &str) -> Result<u16> {
    let port = s
        .parse::<u16>()
        .with_context(|| format!("invalid port '{}'", s))?;
    if port == 0 {
        bail!("binding port 0 (ephemeral) is not supported; pick a fixed port");
    }
    Ok(port)
}

/// Resolve a host string to an IP literal.
fn parse_ip(host: &str) -> Result<IpAddr> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(ip);
    }
    match host {
        "*" | "" => Ok(IpAddr::from([0, 0, 0, 0])),
        _ => bail!(
            "'{}' is not an IP literal; use an IP address (or 0.0.0.0 / :: / *) — \
             name resolution is not available at boot",
            host
        ),
    }
}

/// Bind a listening descriptor for the given spec and return its raw fd.
///
/// TCP sockets use `SO_REUSEADDR` (set by `std`), UDP sockets are bound
/// without `listen`, and Unix sockets unlink any stale path first so a
/// crashed service cannot wedge the path for the next start.
pub fn bind(spec: &ListenSpec) -> Result<i32> {
    let fd = match spec {
        ListenSpec::Tcp { addr } => {
            let listener = TcpListener::bind(addr)
                .with_context(|| format!("failed to bind TCP socket on {}", addr))?;
            listener.into_raw_fd()
        }
        ListenSpec::Udp { addr } => {
            let socket = UdpSocket::bind(addr)
                .with_context(|| format!("failed to bind UDP socket on {}", addr))?;
            socket.into_raw_fd()
        }
        ListenSpec::Unix { path } => {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::remove_file(path);
            let listener = UnixListener::bind(path)
                .with_context(|| format!("failed to bind Unix socket at {}", path.display()))?;
            listener.into_raw_fd()
        }
    };
    Ok(fd)
}

/// Attempt to connect to the endpoint described by `spec`. Used for
/// readiness probing (a service is "socket-ready" once it accepts
/// connections).
pub fn can_connect(spec: &ListenSpec, timeout: Duration) -> bool {
    match spec {
        ListenSpec::Tcp { addr } => TcpStream::connect_timeout(addr, timeout).is_ok(),
        ListenSpec::Udp { addr } => {
            if let Ok(sock) = UdpSocket::bind((IpAddr::from([0, 0, 0, 0]), 0)) {
                sock.connect(addr).map(|_| true).unwrap_or(false)
            } else {
                false
            }
        }
        ListenSpec::Unix { path } => UnixStream::connect(path).is_ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::net::Ipv6Addr;

    fn tcp(ip: IpAddr, port: u16) -> ListenSpec {
        ListenSpec::Tcp {
            addr: SocketAddr::new(ip, port),
        }
    }

    #[test]
    fn parse_port_only_tcp() {
        let spec = parse_listen_spec("22", "tcp").unwrap();
        assert_eq!(spec, tcp(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 22));
    }

    #[test]
    fn parse_tcp_prefix() {
        let spec = parse_listen_spec("tcp:127.0.0.1:8000", "tcp").unwrap();
        assert_eq!(spec, tcp(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8000));
    }

    #[test]
    fn parse_udp_default_type() {
        let spec = parse_listen_spec("udp:53", "tcp").unwrap();
        assert_eq!(
            spec,
            ListenSpec::Udp {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 53)
            }
        );
    }

    #[test]
    fn parse_ipv6_bracketed() {
        let spec = parse_listen_spec("[::1]:80", "tcp").unwrap();
        assert_eq!(spec, tcp(IpAddr::V6(Ipv6Addr::LOCALHOST), 80));
    }

    #[test]
    fn parse_ipv6_any() {
        let spec = parse_listen_spec("tcp:[::]:80", "tcp").unwrap();
        assert_eq!(spec, tcp(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 80));
    }

    #[test]
    fn parse_unix() {
        let spec = parse_listen_spec("unix:/run/x.sock", "tcp").unwrap();
        assert_eq!(
            spec,
            ListenSpec::Unix {
                path: PathBuf::from("/run/x.sock")
            }
        );
    }

    #[test]
    fn parse_wildcard_default() {
        let spec = parse_listen_spec("9000", "tcp").unwrap();
        assert_eq!(spec, tcp(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 9000));
    }

    #[test]
    fn reject_hostname() {
        assert!(parse_listen_spec("tcp:localhost:80", "tcp").is_err());
    }

    #[test]
    fn reject_bad_port() {
        assert!(parse_listen_spec("tcp:1.2.3.4:notaport", "tcp").is_err());
        assert!(parse_listen_spec("tcp:1.2.3.4:70000", "tcp").is_err());
        assert!(parse_listen_spec("tcp:1.2.3.4:0", "tcp").is_err());
    }

    #[test]
    fn reject_empty() {
        assert!(parse_listen_spec("", "tcp").is_err());
    }

    #[test]
    fn reject_unknown_type() {
        assert!(parse_listen_spec("weird:22", "tcp").is_err());
    }

    #[test]
    fn unix_bind_then_connect_roundtrip() {
        let path =
            std::env::temp_dir().join(format!("vigil-sock-test-{}.sock", std::process::id()));
        let spec = ListenSpec::Unix { path: path.clone() };

        let fd = bind(&spec).unwrap();
        assert!(fd >= 0);
        assert!(can_connect(&spec, Duration::from_millis(100)));

        // bind() took ownership of the descriptor via into_raw_fd(); close it.
        unsafe {
            libc::close(fd);
        }
        let _ = std::fs::remove_file(&path);
    }
}

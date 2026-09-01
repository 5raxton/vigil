use anyhow::{bail, Context, Result};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::unix::io::IntoRawFd;
use std::os::unix::net::UnixListener;
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
        ListenSpec::Udp { addr } => can_connect_udp(addr, timeout),
        ListenSpec::Unix { path } => can_connect_unix(path, timeout),
    }
}

/// Probe a UDP endpoint. A plain `connect()` always succeeds for UDP (it never
/// requires a listener), so instead we send a zero-length probe datagram and
/// watch for an immediate ICMP "connection refused" reply, which is the
/// reliable signal that nothing is bound to the port yet. No reply within the
/// window (or any actual response datagram) is treated as "up".
fn can_connect_udp(addr: &SocketAddr, timeout: Duration) -> bool {
    let sock = match UdpSocket::bind((IpAddr::from([0, 0, 0, 0]), 0)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    if sock.connect(addr).is_err() {
        return false;
    }
    let _ = sock.send(&[0u8; 1]);
    let _ = sock.set_read_timeout(Some(timeout));

    let mut buf = [0u8; 8];
    match sock.recv(&mut buf) {
        // A response (or any datagram) arrived — the endpoint is live.
        Ok(_) => true,
        Err(ref e) if e.kind() == std::io::ErrorKind::ConnectionRefused => false,
        // Timed out with no ICMP error: nothing proved it is down.
        Err(_) => true,
    }
}

/// Probe a Unix stream endpoint without ever blocking the caller, matching
/// the bounded behaviour of TCP's `connect_timeout`. A plain
/// `UnixStream::connect` blocks indefinitely while the listener's accept
/// queue is full, which would wedge the readiness probe (and with it the
/// whole supervision loop) behind a service that is up but not accepting.
fn can_connect_unix(path: &std::path::Path, timeout: Duration) -> bool {
    use std::io::Error as IoError;

    let (addr, socklen) = match unix_sockaddr(path) {
        Some(x) => x,
        None => return false,
    };

    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return false;
    }

    let mut ok = false;
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);

        let rc = libc::connect(
            fd,
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            socklen,
        );

        if rc == 0 {
            ok = true;
        } else {
            let errno = IoError::last_os_error().raw_os_error().unwrap_or(-1);
            // The attempt is pending in the server's accept queue; it
            // completes once the server accepts. Any other errno (EAGAIN when
            // the queue is full, ECONNREFUSED when the path is gone, ...)
            // means the endpoint is not accepting connections yet, so say so
            // immediately instead of burning the whole timeout.
            if errno == libc::EINPROGRESS {
                let mut pfd = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
                let poll_rc = libc::poll(&mut pfd, 1, ms);
                if poll_rc > 0 && (pfd.revents & libc::POLLOUT) != 0 {
                    let mut so_error: libc::c_int = 0;
                    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
                    let opt = libc::getsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_ERROR,
                        &mut so_error as *mut _ as *mut libc::c_void,
                        &mut len,
                    );
                    ok = opt == 0 && so_error == 0;
                }
            }
        }
        libc::close(fd);
    }
    ok
}

/// Build a `sockaddr_un` + length for a filesystem Unix socket path.
fn unix_sockaddr(path: &std::path::Path) -> Option<(libc::sockaddr_un, libc::socklen_t)> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let bytes = c_path.as_bytes();
    if bytes.len() >= 108 {
        return None;
    }
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in addr.sun_path.iter_mut().zip(bytes.iter().copied()) {
        *slot = byte as libc::c_char;
    }
    let socklen = std::mem::offset_of!(libc::sockaddr_un, sun_path) as libc::socklen_t
        + bytes.len() as libc::socklen_t
        + 1;
    Some((addr, socklen))
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

    #[test]
    fn unix_bind_then_connect_roundtrip_after_rebind() {
        let path = std::env::temp_dir().join(format!(
            "vigil-sock-test-rebind-{}.sock",
            std::process::id()
        ));
        let spec = ListenSpec::Unix { path: path.clone() };

        for _ in 0..2 {
            let fd = bind(&spec).unwrap();
            assert!(can_connect(&spec, Duration::from_millis(200)));
            unsafe {
                libc::close(fd);
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unix_bind_then_connect_unstable_path() {
        // A missing unix path must be reported as "not ready" immediately
        // (never blocking, never panicking) so the supervision loop keeps
        // polling instead of hanging.
        let path = std::env::temp_dir().join(format!(
            "vigil-sock-test-missing-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let spec = ListenSpec::Unix { path };
        assert!(!can_connect(&spec, Duration::from_millis(100)));
    }

    #[test]
    fn unix_can_connect_bounded_when_backlog_full() {
        use std::os::unix::io::RawFd;
        use std::time::Instant;

        let path = std::env::temp_dir().join(format!(
            "vigil-sock-test-full-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        // A raw listener with an explicit tiny backlog (std's UnixListener
        // derives its effective queue from somaxconn, which varies by kernel
        // and makes the fill nondeterministic). The kernel queues `backlog+1`
        // pending connections for a Unix stream socket before connect() fails.
        let lfd = unsafe {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
            assert!(fd >= 0, "socket() failed");
            let (addr, slen) = unix_sockaddr(&path).unwrap();
            assert_eq!(
                libc::bind(fd, &addr as *const libc::sockaddr_un as *const libc::sockaddr, slen),
                0,
                "bind() failed"
            );
            assert_eq!(libc::listen(fd, 2), 0, "listen() failed");
            fd
        };

        // Fill the accept queue with non-blocking clients so the test process
        // itself never blocks in connect().
        let (addr, slen) = unix_sockaddr(&path).unwrap();
        let mut queued: Vec<RawFd> = Vec::new();
        for _ in 0..64 {
            let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
            if fd < 0 {
                break;
            }
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
            let rc = unsafe {
                libc::connect(
                    fd,
                    &addr as *const libc::sockaddr_un as *const libc::sockaddr,
                    slen,
                )
            };
            if rc != 0 {
                unsafe {
                    libc::close(fd);
                }
                break;
            }
            queued.push(fd);
        }
        assert!(
            queued.len() >= 3,
            "expected to queue at least backlog+1 connections, got {}",
            queued.len()
        );

        let start = Instant::now();
        let ok = can_connect(
            &ListenSpec::Unix {
                path: path.clone(),
            },
            Duration::from_millis(300),
        );
        let elapsed = start.elapsed();

        // With the accept queue full the probe must report "not ready" and,
        // crucially, must return promptly — the blocking `UnixStream::connect`
        // this replaces hangs here forever.
        assert!(!ok, "a full backlog must not satisfy the readiness probe");
        assert!(
            elapsed < Duration::from_secs(2),
            "probe took too long: {:?}",
            elapsed
        );

        for fd in queued {
            unsafe {
                libc::close(fd);
            }
        }
        unsafe {
            libc::close(lfd);
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn udp_probe_detects_closed_port() {
        use std::net::Ipv4Addr;

        let sock = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = sock.local_addr().unwrap();
        drop(sock);

        // Give the previous socket a moment to fully release the port.
        std::thread::sleep(Duration::from_millis(30));

        let spec = ListenSpec::Udp { addr };
        assert!(
            !can_connect(&spec, Duration::from_millis(500)),
            "a closed UDP port must not be ready"
        );
    }

    #[test]
    fn udp_probe_succeeds_for_bound_socket() {
        use std::net::Ipv4Addr;

        let sock = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = sock.local_addr().unwrap();

        let spec = ListenSpec::Udp { addr };
        assert!(can_connect(&spec, Duration::from_millis(500)));
        drop(sock);
    }
}

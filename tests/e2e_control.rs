use std::fs;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use vigil::protocol::{self, Request, Response};

/// End-to-end test of the control plane: a real vigil-scan process is launched
/// against a throwaway config tree and driven through the full control socket
/// lifecycle (ping/list/status/restart/stop/start/reload + graceful shutdown).
///
/// This requires no privileges: everything lives under a per-run temp
/// directory and vigil-supervise/vigillog are found next to the built
/// vigil-scan binary via `exec_search_paths`.
const SCAN_EXE: &str = env!("CARGO_BIN_EXE_vigil-scan");

struct TestScan {
    child: Child,
    base: PathBuf,
    control_socket: PathBuf,
}

impl TestScan {
    fn spawn() -> Self {
        let base = std::env::temp_dir().join(format!(
            "vigil-e2e-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();

        let service_dir = base.join("services.d");
        let target_dir = base.join("targets");
        let log_dir = base.join("log");
        let runtime_dir = base.join("run");
        fs::create_dir_all(&service_dir).unwrap();
        fs::create_dir_all(&target_dir).unwrap();
        fs::create_dir_all(&log_dir).unwrap();
        fs::create_dir_all(&runtime_dir).unwrap();

        let control_socket = runtime_dir.join("control.sock");

        let global = format!(
            "service_dir = {:?}\ntarget_dir = {:?}\nlog_dir = {:?}\nruntime_dir = {:?}\ncontrol_socket = {:?}\ndefault_target = \"default\"\n",
            service_dir.to_str().unwrap(),
            target_dir.to_str().unwrap(),
            log_dir.to_str().unwrap(),
            runtime_dir.to_str().unwrap(),
            control_socket.to_str().unwrap(),
        );
        fs::write(base.join("vigil.toml"), global).unwrap();

        let service = r#"
[service]
command = "/bin/sh"
args = ["-c", "sleep 300"]
description = "e2e test service"
stdout = "null"
stderr = "null"

[service.restart]
policy = "never"

[logging]
type = "none"
"#;
        fs::write(service_dir.join("echosvc.toml"), service).unwrap();

        let child = Command::new(SCAN_EXE)
            .arg("--config")
            .arg(base.join("vigil.toml"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn vigil-scan");

        TestScan {
            child,
            base,
            control_socket,
        }
    }

    fn control_path(&self) -> &Path {
        &self.control_socket
    }
}

impl Drop for TestScan {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn roundtrip(sock: &Path, req: &Request) -> Response {
    let mut stream = UnixStream::connect(sock)
        .unwrap_or_else(|e| panic!("connect to {} failed: {}", sock.display(), e));
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    protocol::write_message(&mut stream, req).unwrap();
    protocol::read_message(&mut stream).unwrap()
}

/// Poll `connect` until it succeeds (the socket appears once vigil-scan has
/// opened and drained its listener), bounding the whole wait to `deadline`.
fn wait_for_control_socket(sock: &Path, deadline: Instant) {
    loop {
        if let Ok(mut stream) = UnixStream::connect(sock) {
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            protocol::write_message(&mut stream, &Request::Ping).unwrap();
            if let Ok(Response::Pong) = protocol::read_message(&mut stream) {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "control socket never became ready"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Poll `pred` until it returns `Some`, panicking once `deadline` passes.
fn wait_until<T>(label: &str, deadline: Instant, mut pred: impl FnMut() -> Option<T>) -> Option<T> {
    loop {
        if let Some(value) = pred() {
            return Some(value);
        }
        assert!(Instant::now() < deadline, "timed out waiting for {}", label);
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn control_plane_lifecycle() {
    let mut scan = TestScan::spawn();
    let sock = scan.control_path().to_path_buf();

    let start = Instant::now();
    let deadline = start + Duration::from_secs(30);
    wait_for_control_socket(&sock, deadline);

    // list: the service must be present.
    let infos = match roundtrip(&sock, &Request::List) {
        Response::List(infos) => infos,
        other => panic!("unexpected list response: {other:?}"),
    };
    assert!(
        infos.iter().any(|i| i.name == "echosvc"),
        "echosvc missing from list: {infos:?}"
    );

    // status: the service should come up with its real pid recorded.
    let first_pid: Option<u32> = wait_until("echosvc running", deadline, || {
        match roundtrip(
            &sock,
            &Request::Status {
                service: Some("echosvc".into()),
            },
        ) {
            Response::Status(s) if s.state == "running" && s.pid.is_some() => s.pid,
            Response::Status(s) => {
                assert_ne!(s.state, "failed", "echosvc failed to start");
                None
            }
            other => panic!("unexpected status response: {other:?}"),
        }
    });

    // restart: a new pid replaces the old one, proving the old instance was
    // torn down (terminate_tree waits for the whole process group).
    match roundtrip(
        &sock,
        &Request::Restart {
            service: "echosvc".into(),
        },
    ) {
        Response::Ok { .. } => {}
        other => panic!("unexpected restart response: {other:?}"),
    }
    let new_pid: Option<u32> =
        wait_until(
            "echosvc running after restart",
            deadline,
            || match roundtrip(
                &sock,
                &Request::Status {
                    service: Some("echosvc".into()),
                },
            ) {
                Response::Status(s) if s.state == "running" => s.pid,
                _ => None,
            },
        );
    assert!(
        new_pid.is_some() && new_pid != first_pid,
        "restart did not replace the service pid (old {:?}, new {:?})",
        first_pid,
        new_pid
    );

    // log: logging.type=none means no scannable log file -> an error response.
    match roundtrip(
        &sock,
        &Request::Log {
            service: "echosvc".into(),
            lines: 10,
        },
    ) {
        Response::Error { message } => {
            assert!(
                message.contains("no log file"),
                "unexpected log error: {}",
                message
            );
        }
        other => panic!("expected log error, got: {other:?}"),
    }

    // stop: supervisor + service shutdown; state falls back to stopped.
    match roundtrip(
        &sock,
        &Request::Stop {
            service: "echosvc".into(),
        },
    ) {
        Response::Ok { .. } => {}
        other => panic!("unexpected stop response: {other:?}"),
    }
    wait_until("echosvc stopped", deadline, || {
        match roundtrip(
            &sock,
            &Request::Status {
                service: Some("echosvc".into()),
            },
        ) {
            Response::Status(s) if s.state == "stopped" => Some(()),
            Response::Status(s) if s.state == "failed" => panic!("echosvc failed on stop"),
            _ => None,
        }
    });

    // start from the control plane brings it back up with a fresh pid.
    match roundtrip(
        &sock,
        &Request::Start {
            service: "echosvc".into(),
        },
    ) {
        Response::Ok { .. } => {}
        other => panic!("unexpected start response: {other:?}"),
    }
    wait_until("echosvc running after start", deadline, || match roundtrip(
        &sock,
        &Request::Status {
            service: Some("echosvc".into()),
        },
    ) {
        Response::Status(s) if s.state == "running" && s.pid.is_some() => Some(()),
        _ => None,
    });

    // reload: running service must keep running, still visible.
    match roundtrip(&sock, &Request::Reload) {
        Response::Ok { .. } => {}
        other => panic!("unexpected reload response: {other:?}"),
    }
    wait_until(
        "echosvc running after reload",
        deadline,
        || match roundtrip(
            &sock,
            &Request::Status {
                service: Some("echosvc".into()),
            },
        ) {
            Response::Status(s) if s.state == "running" => Some(()),
            _ => None,
        },
    );

    // graceful shutdown: SIGTERM -> event loop exits -> all services stopped
    // and the control socket is removed.
    unsafe {
        libc::kill(scan.child.id() as i32, libc::SIGTERM);
    }
    let status = wait_until("vigil-scan exit", deadline, || {
        match scan.child.try_wait() {
            Ok(Some(status)) => Some(status),
            Ok(None) => None,
            Err(e) => panic!("wait error: {e}"),
        }
    });
    assert!(
        status.expect("vigil-scan exited").success(),
        "vigil-scan exited with {status:?}"
    );
    assert!(!sock.exists(), "control socket not removed after shutdown");

    println!("e2e control plane lifecycle OK");
}

use anyhow::{Context, Result};
use nix::sys::resource::{setrlimit, Resource};
use nix::sys::signal::Signal;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{chdir, execvp, fork, ForkResult, Pid};
use std::env;
use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::time::{Duration, Instant};

use vigil::config::{LogType, ReadinessType, ServiceConfig};
use vigil::sockspec::{self, ListenSpec};
use vigil::{
    LISTEN_FDS_ENV, LISTEN_FDS_START, LISTEN_PID_ENV, READY_SIGNAL_ENV, SUPERVISOR_PID_ENV,
    VIGIL_LOG_DIR, VIGIL_SUPERVISE_DIR,
};

/// Default readiness timeout when `[service.readiness] timeout_ms` is unset.
const DEFAULT_READINESS_TIMEOUT: u64 = 30_000;

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "usage: vigil-supervise <service_name> <config_path> [log_dir] [supervise_dir]"
        );
        exit(1);
    }

    let service_name = &args[1];
    let config_path = PathBuf::from(&args[2]);
    let default_log_dir = args.get(3).map(|s| s.as_str()).unwrap_or(VIGIL_LOG_DIR);
    let default_supervise_dir = args
        .get(4)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(VIGIL_SUPERVISE_DIR));

    let config_content = fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read config: {}", config_path.display()))?;
    let config: ServiceConfig =
        toml::from_str(&config_content).with_context(|| "failed to parse config")?;

    let supervise_dir = default_supervise_dir.join(service_name);
    fs::create_dir_all(&supervise_dir)?;

    let status_dir = supervise_dir.join("status");
    fs::create_dir_all(&status_dir)?;

    eprintln!(
        "vigil-supervise [{}]: starting service supervisor",
        service_name
    );

    block_stop_signals(&config)?;

    // Validate the configured shutdown/kill signal names so a typo surfaces
    // loudly at startup instead of silently degrading teardown (e.g. a
    // mistyped `kill_signal = "KILLL"` falling back to SIGTERM and potentially
    // never force-killing a stuck process).
    validate_signal_name(&config.service.shutdown.signal, "shutdown.signal", service_name)?;
    validate_signal_name(
        &config.service.shutdown.kill_signal,
        "shutdown.kill_signal",
        service_name,
    )?;

    let mut restart_count: u32 = 0;
    let mut current_backoff = config.service.restart.backoff_initial_ms;
    let mut last_restart = Instant::now();

    loop {
        let log_base = resolve_log_base(&config, default_log_dir, service_name);
        if let Err(e) = fs::create_dir_all(&log_base) {
            eprintln!(
                "vigil-supervise [{}]: failed to create log dir {}: {}",
                service_name,
                log_base.display(),
                e
            );
        }

        // Bind listening sockets before creating the log pipe so the pipe
        // can never occupy the descriptors we reserve for socket activation.
        let listen_fds = match bind_listen_sockets(&config) {
            Ok(fds) => fds,
            Err(e) => {
                eprintln!(
                    "vigil-supervise [{}]: socket activation bind failed: {}",
                    service_name, e
                );
                writeln_state(&status_dir, "failed", None)?;
                break;
            }
        };

        let (log_read, log_write) = create_log_pipe(&config, service_name);

        writeln_state(&status_dir, "starting", None)?;
        eprintln!("vigil-supervise [{}]: starting service", service_name);

        let child_pid = match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => child,
            Ok(ForkResult::Child) => {
                if log_read >= 0 {
                    unsafe {
                        libc::close(log_read);
                    }
                }
                run_service_child(service_name, &config, log_write, &listen_fds);
            }
            Err(e) => {
                eprintln!("vigil-supervise [{}]: fork failed: {}", service_name, e);
                for fd in &listen_fds {
                    unsafe {
                        libc::close(*fd);
                    }
                }
                writeln_state(&status_dir, "failed", None)?;
                break;
            }
        };

        let parent_write = log_write;
        if parent_write >= 0 {
            unsafe {
                libc::close(parent_write);
            }
        }

        let vigillog_pid = if log_read >= 0 {
            match spawn_logger(&config, default_log_dir, service_name, log_read) {
                Ok(pid) => Some(pid),
                Err(e) => {
                    eprintln!(
                        "vigil-supervise [{}]: failed to spawn log writer: {}",
                        service_name, e
                    );
                    None
                }
            }
        } else {
            None
        };

        if log_read >= 0 {
            unsafe {
                libc::close(log_read);
            }
        }

        // The service inherited its own copies of the listening descriptors;
        // closing ours lets a later restart rebind the same endpoints.
        for fd in &listen_fds {
            unsafe {
                libc::close(*fd);
            }
        }

        try_setup_cpu_shares(&config, service_name, child_pid.as_raw() as u32);

        let outcome = supervise_child(service_name, &config, child_pid, vigillog_pid, &status_dir);

        match outcome {
            ChildOutcome::StopRequested => {
                eprintln!(
                    "vigil-supervise [{}]: stop requested; exiting",
                    service_name
                );
                writeln_state(&status_dir, "stopped", None)?;
                run_finish_script(service_name, &config_path, &log_base, restart_count);
                break;
            }
            other => {
                let (code, signal, readiness_failed) = match other {
                    ChildOutcome::StoppedCleanly(code, signal) => (code, signal, false),
                    ChildOutcome::ReadinessFailed => (None, None, true),
                    ChildOutcome::StopRequested => unreachable!(),
                };

                writeln_state(&status_dir, "stopped", None)?;

                let reason = if readiness_failed {
                    "readiness check failed".to_string()
                } else {
                    describe_exit(code, signal)
                };

                let should_restart = match config.service.restart.policy {
                    vigil::config::RestartPolicy::Never => false,
                    vigil::config::RestartPolicy::Always => true,
                    vigil::config::RestartPolicy::OnFailure => {
                        readiness_failed || !matches!((code, signal), (Some(0), None))
                    }
                    vigil::config::RestartPolicy::OnAbnormal => {
                        readiness_failed || signal.is_some()
                    }
                };

                // Post-exit hook: runs every time the service process exits,
                // before the restart decision is taken.
                run_finish_script(service_name, &config_path, &log_base, restart_count);

                if !should_restart {
                    eprintln!(
                        "vigil-supervise [{}]: service terminated ({}) and restart policy is {:?}; giving up",
                        service_name,
                        reason,
                        config.service.restart.policy
                    );
                    writeln_state(&status_dir, "terminated", None)?;
                    break;
                }

                if restart_count >= config.service.restart.max_restarts {
                    eprintln!(
                        "vigil-supervise [{}]: max restarts ({}) reached; giving up",
                        service_name, config.service.restart.max_restarts
                    );
                    writeln_state(&status_dir, "failed", None)?;
                    break;
                }

                restart_count += 1;
                if last_restart.elapsed() >= Duration::from_secs(600) && restart_count > 1 {
                    restart_count = 1;
                    current_backoff = config.service.restart.backoff_initial_ms;
                }
                last_restart = Instant::now();
                save_restart_count(&status_dir, restart_count);

                eprintln!(
                    "vigil-supervise [{}]: restarting in {}ms (attempt {}/{})",
                    service_name,
                    current_backoff,
                    restart_count,
                    config.service.restart.max_restarts
                );
                writeln_state(&status_dir, "restarting", None)?;
                match sleep_interruptible(Duration::from_millis(current_backoff), &status_dir) {
                    Ok(()) => {}
                    Err(_) => break,
                }
                current_backoff =
                    (current_backoff as f64 * config.service.restart.backoff_multiplier) as u64;
                if current_backoff > config.service.restart.backoff_max_ms {
                    current_backoff = config.service.restart.backoff_max_ms;
                }
            }
        }
    }

    eprintln!("vigil-supervise [{}]: supervisor exiting", service_name);
    Ok(())
}

enum ChildOutcome {
    StoppedCleanly(Option<i32>, Option<Signal>),
    StopRequested,
    ReadinessFailed,
}

fn reset_signal_mask() {
    let mut empty: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut empty);
        libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
    }
}

/// Block the supervisor's control signals. Everything in this set is
/// consumed with `sigtimedwait` so the process never dies from an
/// unexpected delivery and messages are never lost.
fn block_stop_signals(config: &ServiceConfig) -> Result<()> {
    let mut sigset: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut sigset);
        libc::sigaddset(&mut sigset, libc::SIGCHLD);
        libc::sigaddset(&mut sigset, libc::SIGTERM);
        libc::sigaddset(&mut sigset, libc::SIGINT);
        libc::sigaddset(&mut sigset, libc::SIGHUP);
    }

    if config.service.readiness.kind == ReadinessType::Signal {
        let sig = signal_signo(config.service.readiness.ready_signal())
            .context("invalid readiness signal name")?;
        if matches!(sig, libc::SIGTERM | libc::SIGINT | libc::SIGHUP) {
            anyhow::bail!(
                "readiness signal must not be one of TERM/INT/HUP (reserved for stopping)"
            );
        }
        unsafe {
            libc::sigaddset(&mut sigset, sig);
        }
    }

    unsafe {
        libc::sigprocmask(libc::SIG_BLOCK, &sigset, std::ptr::null_mut());
    }
    Ok(())
}

fn pop_signal_detail(sigset: &libc::sigset_t) -> Result<Option<i32>> {
    let timeout = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let signo = unsafe { libc::sigtimedwait(sigset, std::ptr::null_mut(), &timeout) };
    if signo > 0 {
        Ok(Some(signo))
    } else {
        Ok(None)
    }
}

fn supervise_child(
    service_name: &str,
    config: &ServiceConfig,
    child_pid: Pid,
    vigillog_pid: Option<Pid>,
    status_dir: &Path,
) -> ChildOutcome {
    let readiness = &config.service.readiness;
    let ready_sig = if readiness.kind == ReadinessType::Signal {
        signal_signo(readiness.ready_signal())
    } else {
        None
    };

    let mut sigset: libc::sigset_t = unsafe {
        let mut s = std::mem::zeroed();
        libc::sigemptyset(&mut s);
        libc::sigaddset(&mut s, libc::SIGCHLD);
        libc::sigaddset(&mut s, libc::SIGTERM);
        libc::sigaddset(&mut s, libc::SIGINT);
        libc::sigaddset(&mut s, libc::SIGHUP);
        s
    };
    if let Some(sig) = ready_sig {
        unsafe {
            libc::sigaddset(&mut sigset, sig);
        }
    }

    let deadline = Instant::now() + readiness_timeout(readiness);
    let mut ready = readiness.kind == ReadinessType::None;
    // Whether the "running" state + real service PID has been recorded. For
    // `none` readiness the service is considered ready immediately after fork;
    // for other kinds it becomes ready when the probe or readiness signal
    // fires. It must be written exactly once so `vigil-ctl status` reports the
    // real PID for every running service, including those with no readiness
    // check.
    let mut ready_recorded = false;
    let mut probe = if ready {
        None
    } else {
        let socket_target = socket_readiness_target(config);
        Some(ReadinessProbe::new(readiness, service_name, socket_target))
    };

    let child_pid_raw = child_pid.as_raw();
    let mut vigillog_pid = vigillog_pid;

    loop {
        match waitpid(child_pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(_, code)) => {
                if let Some(p) = probe.as_mut() {
                    p.cleanup();
                }
                reap_vigillog(vigillog_pid);
                return ChildOutcome::StoppedCleanly(Some(code), None);
            }
            Ok(WaitStatus::Signaled(_, sig, _)) => {
                if let Some(p) = probe.as_mut() {
                    p.cleanup();
                }
                reap_vigillog(vigillog_pid);
                return ChildOutcome::StoppedCleanly(None, Some(sig));
            }
            Ok(WaitStatus::StillAlive) | Ok(WaitStatus::Stopped(_, _)) => {}
            Ok(_) => {
                if let Some(p) = probe.as_mut() {
                    p.cleanup();
                }
                reap_vigillog(vigillog_pid);
                return ChildOutcome::StoppedCleanly(None, None);
            }
            Err(nix::errno::Errno::ECHILD) => {
                if let Some(p) = probe.as_mut() {
                    p.cleanup();
                }
                reap_vigillog(vigillog_pid);
                return ChildOutcome::StoppedCleanly(None, None);
            }
            Err(e) => {
                eprintln!("vigil-supervise [{}]: waitpid error: {}", service_name, e);
                if let Some(p) = probe.as_mut() {
                    p.cleanup();
                }
                reap_vigillog(vigillog_pid);
                return ChildOutcome::StoppedCleanly(None, None);
            }
        }

        if let Some(vp) = vigillog_pid {
            match waitpid(vp, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(_, _)) | Ok(WaitStatus::Signaled(_, _, _)) => {
                    vigillog_pid = None;
                }
                _ => {}
            }
        }

        match pop_signal_detail(&sigset).ok().flatten() {
            Some(libc::SIGTERM) | Some(libc::SIGINT) | Some(libc::SIGHUP) => {
                let shutdown_signal = signal_from_name(&config.service.shutdown.signal);
                let kill_signal = signal_from_name(&config.service.shutdown.kill_signal);
                let grace = Duration::from_millis(config.service.shutdown.timeout_ms);
                terminate_tree(
                    child_pid_raw,
                    shutdown_signal,
                    kill_signal,
                    grace,
                    service_name,
                );
                if let Some(p) = probe.as_mut() {
                    p.cleanup();
                }
                reap_vigillog(vigillog_pid);
                return ChildOutcome::StopRequested;
            }
            Some(sig) if ready_sig == Some(sig) => {
                ready = true;
            }
            Some(libc::SIGCHLD) => {}
            Some(_) => {}
            None => {}
        }

        // Record the service as running (with its real PID) exactly once, as
        // soon as it is ready. This covers the `none` readiness kind, which
        // otherwise never wrote the state/pid, and any kind that just became
        // ready above.
        if ready && !ready_recorded {
            let _ = writeln_state(status_dir, "running", Some(child_pid_raw as u32));
            ready_recorded = true;
        }

        if !ready {
            if Instant::now() >= deadline {
                eprintln!(
                    "vigil-supervise [{}]: readiness timed out; shutting down service",
                    service_name
                );
                if let Some(p) = probe.as_mut() {
                    p.cleanup();
                }
                terminate_tree(
                    child_pid_raw,
                    signal_from_name(&config.service.shutdown.signal),
                    signal_from_name(&config.service.shutdown.kill_signal),
                    Duration::from_millis(config.service.shutdown.timeout_ms),
                    service_name,
                );
                reap_vigillog(vigillog_pid);
                return ChildOutcome::ReadinessFailed;
            }

            if let Some(probe) = probe.as_mut() {
                match probe.tick() {
                    ProbeResult::Ready => {
                        ready = true;
                        probe.cleanup();
                    }
                    ProbeResult::Failed(msg) => {
                        eprintln!(
                            "vigil-supervise [{}]: readiness failed: {}",
                            service_name, msg
                        );
                        probe.cleanup();
                        terminate_tree(
                            child_pid_raw,
                            signal_from_name(&config.service.shutdown.signal),
                            signal_from_name(&config.service.shutdown.kill_signal),
                            Duration::from_millis(config.service.shutdown.timeout_ms),
                            service_name,
                        );
                        reap_vigillog(vigillog_pid);
                        return ChildOutcome::ReadinessFailed;
                    }
                    ProbeResult::Pending => {}
                }
            }
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}

fn readiness_timeout(readiness: &vigil::config::ReadinessConfig) -> Duration {
    Duration::from_millis(readiness.timeout_ms.unwrap_or(DEFAULT_READINESS_TIMEOUT))
}

enum ProbeResult {
    Ready,
    Pending,
    Failed(String),
}

/// The parsed first `[socket]` listen endpoint, used as the connect target
/// for `readiness.type = "socket"` when no explicit `check` spec is given.
fn socket_readiness_target(config: &ServiceConfig) -> Option<ListenSpec> {
    let sock = config.socket.as_ref()?;
    let first = sock.listen.first()?;
    let proto = sock.proto();
    sockspec::parse_listen_spec(first, proto).ok()
}

/// Drives a configured readiness check. Only one probe type is active per
/// service (from `[service.readiness]`), driven from the supervision loop.
struct ReadinessProbe {
    kind: ReadinessType,
    spec: Option<ListenSpec>,
    pidfile: Option<PathBuf>,
    exec_cmd: Option<Vec<String>>,
    exec_child: Option<std::process::Child>,
    exec_start: Option<Instant>,
    next_exec_at: Option<Instant>,
    invalid: Option<String>,
}

impl ReadinessProbe {
    /// `socket_target` is the parsed first `[socket]` listen endpoint, used
    /// when `readiness.type = "socket"` and no explicit `check` connect spec
    /// is given (matching the documented behaviour of probing the first
    /// listen endpoint).
    fn new(
        readiness: &vigil::config::ReadinessConfig,
        service_name: &str,
        socket_target: Option<ListenSpec>,
    ) -> Self {
        let check = readiness.check.clone();
        let mut probe = ReadinessProbe {
            kind: readiness.kind.clone(),
            spec: socket_target,
            pidfile: None,
            exec_cmd: None,
            exec_child: None,
            exec_start: None,
            next_exec_at: None,
            invalid: None,
        };

        match probe.kind {
            ReadinessType::None | ReadinessType::Signal => {
                // handled by the caller; nothing to probe
            }
            ReadinessType::Pid => match check {
                Some(path) => probe.pidfile = Some(PathBuf::from(path)),
                None => {
                    probe.invalid =
                        Some("readiness type 'pid' requires check=<pidfile path>".to_string());
                }
            },
            ReadinessType::Socket => match check {
                Some(spec) => match sockspec::parse_listen_spec(&spec, sockspec::default_type()) {
                    Ok(s) => probe.spec = Some(s),
                    Err(e) => {
                        probe.invalid =
                            Some(format!("invalid readiness socket spec '{}': {}", spec, e));
                    }
                },
                None => {
                    // No explicit connect target: fall back to the first
                    // `[socket]` listen endpoint if one is configured. This is
                    // what the sshd example relies on.
                    if probe.spec.is_none() {
                        probe.invalid = Some(
                            "readiness type 'socket' requires check=<connect spec> \
                             or a [socket] listen entry"
                                .to_string(),
                        );
                    }
                }
            },
            ReadinessType::Exec => match check {
                Some(cmd) => {
                    probe.exec_cmd = Some(vec!["/bin/sh".to_string(), "-c".to_string(), cmd]);
                    probe.next_exec_at = Some(Instant::now());
                }
                None => {
                    probe.invalid =
                        Some("readiness type 'exec' requires check=<command>".to_string());
                }
            },
        }

        if let Some(ref reason) = probe.invalid {
            eprintln!(
                "vigil-supervise [{}]: readiness configured but {}",
                service_name, reason
            );
        }

        probe
    }

    fn tick(&mut self) -> ProbeResult {
        if let Some(ref err) = self.invalid {
            return ProbeResult::Failed(err.clone());
        }

        match self.kind {
            // `Signal` readiness is satisfied only when the child raises the
            // configured signal (handled directly in the supervision loop),
            // never by the probe itself.
            ReadinessType::None => ProbeResult::Ready,
            ReadinessType::Signal => ProbeResult::Pending,
            ReadinessType::Pid => self.tick_pidfile(),
            ReadinessType::Socket => self.tick_socket(),
            ReadinessType::Exec => self.tick_exec(),
        }
    }

    fn tick_pidfile(&mut self) -> ProbeResult {
        let path = match &self.pidfile {
            Some(p) => p,
            None => return ProbeResult::Failed("pid check configured without a path".into()),
        };

        let raw = match fs::read_to_string(path) {
            Ok(r) => r,
            Err(_) => return ProbeResult::Pending,
        };
        let raw = raw.trim();
        let pid: i32 = match raw.parse() {
            Ok(p) => p,
            Err(_) => return ProbeResult::Pending,
        };
        if pid <= 0 {
            return ProbeResult::Pending;
        }

        // The recorded PID must be alive and owned by (or visible to) us.
        let rc = unsafe { libc::kill(pid, 0) };
        if rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) {
            ProbeResult::Ready
        } else {
            ProbeResult::Pending
        }
    }

    fn tick_socket(&mut self) -> ProbeResult {
        let spec = match &self.spec {
            Some(s) => s,
            None => return ProbeResult::Failed("socket check configured without a spec".into()),
        };
        if sockspec::can_connect(spec, Duration::from_millis(700)) {
            ProbeResult::Ready
        } else {
            ProbeResult::Pending
        }
    }

    fn tick_exec(&mut self) -> ProbeResult {
        if self.exec_child.is_none() {
            let now = Instant::now();
            let run_ok = match self.next_exec_at {
                Some(t) => now >= t,
                None => true,
            };
            if !run_ok {
                return ProbeResult::Pending;
            }
            let cmd = match &self.exec_cmd {
                Some(c) => c,
                None => {
                    return ProbeResult::Failed("exec check configured without a command".into())
                }
            };
            match std::process::Command::new(&cmd[0])
                .args(&cmd[1..])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(c) => {
                    self.exec_start = Some(Instant::now());
                    self.exec_child = Some(c);
                }
                Err(_) => {
                    self.next_exec_at = Some(Instant::now() + Duration::from_millis(500));
                    return ProbeResult::Pending; // transient; keep retrying
                }
            }
        }

        if let Some(child) = self.exec_child.as_mut() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.exec_child = None;
                    self.exec_start = None;
                    if status.success() {
                        ProbeResult::Ready
                    } else {
                        // The service is not ready yet; retry shortly.
                        self.next_exec_at = Some(Instant::now() + Duration::from_millis(500));
                        ProbeResult::Pending
                    }
                }
                Ok(None) => {
                    // A single check run is capped so a wedged check cannot
                    // stall readiness forever.
                    if let Some(start) = self.exec_start {
                        if start.elapsed() >= Duration::from_secs(5) {
                            let _ = child.kill();
                            let _ = child.wait();
                            self.exec_child = None;
                            self.exec_start = None;
                            self.next_exec_at = Some(Instant::now() + Duration::from_millis(500));
                        }
                    }
                    ProbeResult::Pending
                }
                Err(e) => {
                    self.exec_child = None;
                    self.exec_start = None;
                    ProbeResult::Failed(format!("failed to run readiness check: {}", e))
                }
            }
        } else {
            ProbeResult::Pending
        }
    }

    fn cleanup(&mut self) {
        if let Some(ref mut child) = self.exec_child {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.exec_child = None;
    }
}

fn reap_vigillog(pid: Option<Pid>) {
    let Some(pid) = pid else { return };
    let start = Instant::now();
    loop {
        match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => {}
            Ok(_) => return,
            Err(_) => return,
        }
        if start.elapsed() >= Duration::from_secs(2) {
            let _ = nix::sys::signal::kill(pid, Signal::SIGKILL);
            let _ = waitpid(pid, None);
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn sleep_interruptible(duration: Duration, status_dir: &Path) -> Result<()> {
    let sigset: libc::sigset_t = unsafe {
        let mut s = std::mem::zeroed();
        libc::sigemptyset(&mut s);
        libc::sigaddset(&mut s, libc::SIGCHLD);
        libc::sigaddset(&mut s, libc::SIGTERM);
        libc::sigaddset(&mut s, libc::SIGINT);
        libc::sigaddset(&mut s, libc::SIGHUP);
        s
    };

    let mut slept = Duration::ZERO;
    loop {
        match pop_signal_detail(&sigset) {
            Ok(Some(libc::SIGTERM)) | Ok(Some(libc::SIGINT)) | Ok(Some(libc::SIGHUP)) => {
                eprintln!("vigil-supervise: stop during restart backoff");
                writeln_state(status_dir, "stopped", None)?;
                return Err(anyhow::anyhow!("stop requested during backoff"));
            }
            _ => {}
        }
        if slept >= duration {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
        slept += Duration::from_millis(50);
    }
    Ok(())
}

fn terminate_tree(
    pgid: i32,
    signal: Signal,
    kill_signal: Signal,
    grace: Duration,
    service_name: &str,
) {
    let _ = nix::sys::signal::kill(Pid::from_raw(-pgid), signal);

    let start = Instant::now();
    loop {
        if !process_group_alive(pgid) {
            return;
        }
        if start.elapsed() >= grace {
            eprintln!(
                "vigil-supervise [{}]: grace period elapsed, force-killing tree PGID {}",
                service_name, pgid
            );
            let _ = nix::sys::signal::kill(Pid::from_raw(-pgid), kill_signal);
            for _ in 0..40 {
                if !process_group_alive(pgid) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn process_group_alive(pgid: i32) -> bool {
    match nix::sys::signal::kill(Pid::from_raw(-pgid), None) {
        Ok(()) => true,
        Err(nix::errno::Errno::ESRCH) => false,
        Err(nix::errno::Errno::EPERM) => true,
        Err(_) => false,
    }
}

fn describe_exit(code: Option<i32>, signal: Option<Signal>) -> String {
    match (code, signal) {
        (Some(c), None) => format!("exit code {}", c),
        (None, Some(s)) => format!("signal {:?}", s),
        _ => "unknown".to_string(),
    }
}

/// Accept only signal names `signal_from_name` maps exactly (the runtime
/// source of truth), so a name cannot be reported as valid here and then
/// silently sent as a different signal by `signal_from_name`.
fn validate_signal_name(name: &str, field: &str, service_name: &str) -> Result<()> {
    let valid = matches!(
        name,
        "TERM" | "KILL" | "STOP" | "HUP" | "INT" | "QUIT" | "USR1" | "USR2" | "ALRM" | "PIPE"
    );
    if valid {
        Ok(())
    } else {
        anyhow::bail!(
            "vigil-supervise [{}]: invalid {} '{}' (TERM/INT/QUIT/KILL/HUP/USR1/USR2/ALRM/PIPE/STOP)",
            service_name,
            field,
            name
        )
    }
}

fn signal_from_name(name: &str) -> Signal {
    match name {
        "TERM" => Signal::SIGTERM,
        "KILL" => Signal::SIGKILL,
        "STOP" => Signal::SIGSTOP,
        "HUP" => Signal::SIGHUP,
        "INT" => Signal::SIGINT,
        "QUIT" => Signal::SIGQUIT,
        "USR1" => Signal::SIGUSR1,
        "USR2" => Signal::SIGUSR2,
        "ALRM" => Signal::SIGALRM,
        "PIPE" => Signal::SIGPIPE,
        _ => Signal::SIGTERM,
    }
}

fn signal_signo(name: &str) -> Option<libc::c_int> {
    Some(match name {
        "HUP" => libc::SIGHUP,
        "INT" => libc::SIGINT,
        "QUIT" => libc::SIGQUIT,
        "KILL" => libc::SIGKILL,
        "STOP" => libc::SIGSTOP,
        "TERM" => libc::SIGTERM,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        "ALRM" => libc::SIGALRM,
        "PIPE" => libc::SIGPIPE,
        "CHLD" => libc::SIGCHLD,
        "CONT" => libc::SIGCONT,
        "ABRT" => libc::SIGABRT,
        _ => return None,
    })
}

fn resolve_log_base(config: &ServiceConfig, default_log_dir: &str, name: &str) -> PathBuf {
    match config.logging.kind {
        LogType::File => config
            .logging
            .path
            .as_ref()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from(default_log_dir).join(name)),
        _ => config
            .logging
            .path
            .clone()
            .unwrap_or_else(|| PathBuf::from(default_log_dir).join(name)),
    }
}

fn resolve_log_file(config: &ServiceConfig, default_log_dir: &str, name: &str) -> PathBuf {
    config
        .logging
        .path
        .clone()
        .unwrap_or_else(|| PathBuf::from(default_log_dir).join(name).join("current"))
}

fn resolve_syslog_socket(config: &ServiceConfig) -> PathBuf {
    config
        .logging
        .path
        .clone()
        .unwrap_or_else(|| PathBuf::from("/dev/log"))
}

fn create_log_pipe(config: &ServiceConfig, service_name: &str) -> (i32, i32) {
    use vigil::config::OutputTarget;

    // Any stream routed to the log pipeline (Log, or Syslog meaning "through
    // the configured logger") needs a pipe so output is never silently lost.
    let any_to_log = matches!(config.service.stdout, OutputTarget::Log | OutputTarget::Syslog)
        || matches!(config.service.stderr, OutputTarget::Log | OutputTarget::Syslog);

    if config.logging.kind == LogType::None
        || (config.service.stdout == OutputTarget::Null
            && config.service.stderr == OutputTarget::Null)
        || !any_to_log
    {
        return (-1, -1);
    }

    let mut fds: [libc::c_int; 2] = [-1, -1];
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc != 0 {
        eprintln!(
            "vigil-supervise [{}]: failed to create log pipe",
            service_name
        );
        return (-1, -1);
    }
    (fds[0], fds[1])
}

fn spawn_logger(
    config: &ServiceConfig,
    default_log_dir: &str,
    service_name: &str,
    log_read: i32,
) -> Result<Pid> {
    let raw_args: Vec<String> = match config.logging.kind {
        LogType::Pipe => {
            let base = resolve_log_base(config, default_log_dir, service_name);
            vec![
                "pipe".into(),
                service_name.into(),
                base.to_string_lossy().into_owned(),
                config.logging.max_size_mb.to_string(),
                config.logging.max_files.to_string(),
                if config.logging.timestamp { "1" } else { "0" }.to_string(),
            ]
        }
        LogType::File => {
            let path = resolve_log_file(config, default_log_dir, service_name);
            vec![
                "file".into(),
                service_name.into(),
                path.to_string_lossy().into_owned(),
                config.logging.max_size_mb.to_string(),
                config.logging.max_files.to_string(),
                if config.logging.timestamp { "1" } else { "0" }.to_string(),
            ]
        }
        LogType::Syslog => {
            let base = resolve_log_base(config, default_log_dir, service_name);
            let sock = resolve_syslog_socket(config);
            vec![
                "syslog".into(),
                service_name.into(),
                base.to_string_lossy().into_owned(),
                sock.to_string_lossy().into_owned(),
            ]
        }
        LogType::None => unreachable!("no logger is spawned for LogType::None"),
    };

    let mut argv: Vec<CString> = Vec::with_capacity(raw_args.len() + 1);
    argv.push(CString::new("vigillog").unwrap());
    for arg in &raw_args {
        argv.push(CString::new(arg.as_str()).unwrap());
    }

    match unsafe { fork() } {
        Ok(ForkResult::Parent { child }) => Ok(child),
        Ok(ForkResult::Child) => {
            reset_signal_mask();

            if log_read != 0 {
                unsafe {
                    libc::dup2(log_read, 0);
                    libc::close(log_read);
                }
            }

            for path in vigil::util::exec_search_paths("vigillog") {
                if let Ok(path_c) = CString::new(path.to_string_lossy().as_bytes()) {
                    let _ = execvp(&path_c, &argv);
                }
            }

            eprintln!(
                "vigil-supervise [{}]: failed to exec vigillog",
                service_name
            );
            exit(1);
        }
        Err(e) => {
            eprintln!(
                "vigil-supervise [{}]: failed to fork vigillog: {}",
                service_name, e
            );
            Err(anyhow::anyhow!("failed to fork vigillog: {}", e))
        }
    }
}

fn run_service_child(
    service_name: &str,
    config: &ServiceConfig,
    log_write: i32,
    listen_fds: &[i32],
) -> ! {
    unsafe {
        libc::setpgid(0, 0);
    }

    reset_signal_mask();

    if let Err(e) = prepare_service_environment(config) {
        eprintln!(
            "vigil-supervise [{}]: failed to prepare environment: {}",
            service_name, e
        );
        exit(127);
    }

    if let Some(ref work_dir) = config.service.working_dir {
        if let Err(e) = chdir(work_dir) {
            eprintln!(
                "vigil-supervise [{}]: chdir to {} failed: {}",
                service_name,
                work_dir.display(),
                e
            );
            exit(127);
        }
    }

    dup_stdio(config, log_write);

    if log_write > 2 {
        unsafe {
            libc::close(log_write);
        }
    }

    // Socket activation: relocate the listening descriptors into the fixed
    // `LISTEN_FDS_START`.. range so services see a stable, systemd-style
    // environment (`LISTEN_FDS`/`LISTEN_PID`).
    for (i, &fd) in listen_fds.iter().enumerate() {
        let target = LISTEN_FDS_START + i as i32;
        unsafe {
            if fd != target {
                libc::dup2(fd, target);
                libc::close(fd);
            }
        }
    }

    for (key, value) in &config.service.environment {
        env::set_var(key, value);
    }

    env::set_var(SUPERVISOR_PID_ENV, unsafe { libc::getppid().to_string() });
    if config.service.readiness.kind == ReadinessType::Signal {
        env::set_var(READY_SIGNAL_ENV, config.service.readiness.ready_signal());
    }
    if !listen_fds.is_empty() {
        env::set_var(LISTEN_FDS_ENV, listen_fds.len().to_string());
        env::set_var(LISTEN_PID_ENV, std::process::id().to_string());
    }

    let cmd = match CString::new(config.service.command.clone()) {
        Ok(c) => c,
        Err(_) => exit(127),
    };

    let mut argv_c: Vec<CString> = Vec::with_capacity(config.service.args.len() + 2);
    argv_c.push(cmd);
    for arg in &config.service.args {
        match CString::new(arg.as_str()) {
            Ok(a) => argv_c.push(a),
            Err(_) => {
                eprintln!("vigil-supervise [{}]: invalid argument", service_name);
                exit(127);
            }
        }
    }

    let mut argv_ptrs: Vec<*const libc::c_char> = argv_c.iter().map(|c| c.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());

    unsafe {
        libc::execvp(argv_ptrs[0], argv_ptrs.as_ptr());
    }

    eprintln!(
        "vigil-supervise [{}]: exec failed for {}",
        service_name, config.service.command
    );
    exit(127);
}

fn dup_stdio(config: &ServiceConfig, log_write: i32) {
    use vigil::config::OutputTarget;

    for (stream_fd, target) in [(1, &config.service.stdout), (2, &config.service.stderr)] {
        match target {
            // `Syslog` on an individual stream means "send it through the
            // configured logger" (which for logging.kind=syslog reaches
            // syslog); otherwise fall back to the log pipe like `Log`.
            OutputTarget::Log | OutputTarget::Syslog => {
                if log_write >= 0 {
                    let _ = nix::unistd::dup2(log_write, stream_fd);
                } else {
                    redirect_to_dev_null(stream_fd, libc::O_WRONLY);
                }
            }
            OutputTarget::Null => {
                redirect_to_dev_null(stream_fd, libc::O_WRONLY);
            }
            OutputTarget::Stdout => {
                if stream_fd == 2 {
                    let _ = nix::unistd::dup2(1, 2);
                }
            }
        }
    }

    redirect_to_dev_null(0, libc::O_RDONLY);
}

fn redirect_to_dev_null(fd: i32, flags: i32) {
    let dev_null = CString::new("/dev/null").unwrap();
    let nfd = unsafe { libc::open(dev_null.as_ptr(), flags) };
    if nfd >= 0 {
        unsafe {
            libc::dup2(nfd, fd);
            libc::close(nfd);
        }
    }
}

fn prepare_service_environment(config: &ServiceConfig) -> Result<()> {
    apply_resource_limits(config)?;
    drop_privileges(config)?;
    Ok(())
}

fn apply_resource_limits(config: &ServiceConfig) -> Result<()> {
    if let Some(max_files) = config.service.resource_limits.max_files {
        setrlimit(Resource::RLIMIT_NOFILE, max_files, max_files)?;
    }
    if let Some(max_procs) = config.service.resource_limits.max_procs {
        setrlimit(Resource::RLIMIT_NPROC, max_procs, max_procs)?;
    }
    if let Some(max_memory) = config.service.resource_limits.max_memory_mb {
        let bytes = max_memory
            .saturating_mul(1024)
            .saturating_mul(1024);
        setrlimit(Resource::RLIMIT_AS, bytes, bytes)?;
    }
    Ok(())
}

/// Drop the service to its configured user/group (and supplementary
/// groups) *after* resource limits have been applied while still
/// privileged.
fn drop_privileges(config: &ServiceConfig) -> Result<()> {
    let user_name = &config.service.user;
    let group_name = &config.service.group;

    if user_name == "root" && group_name == "root" {
        return Ok(());
    }

    let user = if user_name == "root" {
        None
    } else {
        Some(
            users::get_user_by_name(user_name)
                .with_context(|| format!("user '{}' not found", user_name))?,
        )
    };

    // If only a user was configured, drop to that user's primary group
    // instead of inheriting the "root" default.
    let gid: libc::gid_t = if group_name == "root" {
        user.as_ref().map(|u| u.primary_group_id()).unwrap_or(0)
    } else {
        let group = users::get_group_by_name(group_name)
            .with_context(|| format!("group '{}' not found", group_name))?;
        group.gid()
    };

    let c_user =
        CString::new(user_name.as_str()).map_err(|_| anyhow::anyhow!("user name contains NUL"))?;

    unsafe {
        if user_name != "root" {
            if libc::initgroups(c_user.as_ptr(), gid) != 0 {
                // Fall back to the primary group only if /etc/group cannot
                // be read.
                let groups = [gid];
                libc::setgroups(1, groups.as_ptr());
            }
            if libc::setgid(gid) != 0 {
                return Err(anyhow::anyhow!(
                    "setgid({}) failed: {}",
                    gid,
                    std::io::Error::last_os_error()
                ));
            }
            if let Some(user) = user {
                if libc::setuid(user.uid()) != 0 {
                    return Err(anyhow::anyhow!(
                        "setuid({}) failed: {}",
                        user.uid(),
                        std::io::Error::last_os_error()
                    ));
                }
            }
        } else {
            // Root user with a non-root group: keep the uid, drop the gid.
            if libc::setgid(gid) != 0 {
                return Err(anyhow::anyhow!(
                    "setgid({}) failed: {}",
                    gid,
                    std::io::Error::last_os_error()
                ));
            }
        }
    }

    Ok(())
}

/// Apply `cpu_shares` via cgroup v2 (`cpu.weight`). This is best-effort:
/// on systems without an enabled v2 `cpu` controller the supervisor logs a
/// warning and continues. The child is attached after fork by writing its
/// PID into `cgroup.procs`.
fn try_setup_cpu_shares(config: &ServiceConfig, service_name: &str, pid: u32) {
    let Some(shares) = config.service.resource_limits.cpu_shares else {
        return;
    };

    let cgroup_root = Path::new("/sys/fs/cgroup");
    if !cgroup_root.join("cgroup.controllers").exists() {
        eprintln!(
            "vigil-supervise [{}]: cgroup v2 not detected; cpu_shares not applied",
            service_name
        );
        return;
    }

    let dir = cgroup_root.join("vigil").join(service_name);
    let mkdir = fs::create_dir_all(&dir);
    if let Err(e) = mkdir {
        eprintln!(
            "vigil-supervise [{}]: cannot create cgroup {}: {}",
            service_name,
            dir.display(),
            e
        );
        return;
    }

    let weight = shares_to_weight(shares);
    if let Err(e) = fs::write(dir.join("cpu.weight"), format!("{}\n", weight)) {
        eprintln!(
            "vigil-supervise [{}]: failed to set cpu.weight {} (is the cpu controller enabled?): {}",
            service_name, weight, e
        );
        return;
    }
    if let Err(e) = fs::write(dir.join("cgroup.procs"), format!("{}\n", pid)) {
        eprintln!(
            "vigil-supervise [{}]: failed to attach pid {} to cgroup: {}",
            service_name, pid, e
        );
    }
}

/// Convert cgroup v1 `cpu.shares` (2..262144) to cgroup v2 `cpu.weight`
/// (1..10000), the conversion used by systemd and containerd.
fn shares_to_weight(shares: u64) -> u64 {
    // canonical mapping used by systemd: weight = 1 + shares * 9999 / 2^18,
    // clamped to the kernel's [1, 10000] range.
    (1 + shares.saturating_mul(9_999) / 262_144).min(10_000)
}

fn bind_listen_sockets(config: &ServiceConfig) -> Result<Vec<i32>> {
    let Some(sock) = &config.socket else {
        return Ok(Vec::new());
    };
    if sock.listen.is_empty() {
        return Ok(Vec::new());
    }

    let proto = sock.proto();
    let mut fds: Vec<i32> = Vec::new();
    let result: Result<()> = (|| {
        for spec in &sock.listen {
            let parsed = sockspec::parse_listen_spec(spec, proto)
                .with_context(|| format!("invalid listen spec '{}'", spec))?;
            let fd = sockspec::bind(&parsed)
                .with_context(|| format!("failed to bind listen spec '{}'", spec))?;
            fds.push(fd);
        }
        Ok(())
    })();

    if let Err(e) = result {
        for fd in &fds {
            unsafe {
                libc::close(*fd);
            }
        }
        return Err(e);
    }
    Ok(fds)
}

fn writeln_state(status_dir: &Path, state: &str, pid: Option<u32>) -> Result<()> {
    let state_path = status_dir.join("state");
    let mut f = fs::File::create(&state_path)?;
    writeln!(f, "{}", state)?;

    let pid_path = status_dir.join("pid");
    match pid {
        Some(pid) => fs::write(&pid_path, pid.to_string())?,
        None => {
            let _ = fs::remove_file(&pid_path);
        }
    }

    Ok(())
}

fn save_restart_count(status_dir: &Path, count: u32) {
    let _ = fs::write(status_dir.join("restarts"), count.to_string());
}

/// Optional per-service post-exit hook: `<service config dir>/<name>/finish`
/// is executed whenever the service process exits, before the restart
/// decision is taken. Environment: `SERVICE_NAME`, `CONFIG_PATH`,
/// `LOG_DIR`, `RESTART_COUNT`.
fn run_finish_script(service_name: &str, config_path: &Path, log_dir: &Path, restart_count: u32) {
    let config_dir = config_path
        .parent()
        .unwrap_or(Path::new("/etc/vigil/services"));
    let finish_path = config_dir.join(service_name).join("finish");

    if finish_path.is_file() {
        eprintln!(
            "vigil-supervise [{}]: running finish script {}",
            service_name,
            finish_path.display()
        );
        let _ = std::process::Command::new(&finish_path)
            .arg(service_name)
            .env("SERVICE_NAME", service_name)
            .env("CONFIG_PATH", config_path)
            .env("LOG_DIR", log_dir)
            .env("RESTART_COUNT", restart_count.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_to_weight_endpoints() {
        assert_eq!(shares_to_weight(2), 1);
        assert_eq!(shares_to_weight(262_144), 10_000);
        assert_eq!(shares_to_weight(1024), 40); // canonical mapping used by systemd
    }

    #[test]
    fn shares_to_weight_clamps() {
        assert_eq!(shares_to_weight(0), 1);
        assert_eq!(shares_to_weight(1_000_000), 10_000);
    }

    #[test]
    fn signal_names_resolve() {
        assert_eq!(signal_signo("USR1"), Some(libc::SIGUSR1));
        assert_eq!(signal_signo("term"), None); // must match exact names
        assert_eq!(signal_signo("bogus"), None);
        assert!(signal_signo("KILL").is_some());
    }

    #[test]
    fn describe_exit_forms() {
        assert_eq!(describe_exit(Some(3), None), "exit code 3");
        assert!(describe_exit(None, Some(Signal::SIGSEGV)).contains("SIGSEGV"));
        assert_eq!(describe_exit(None, None), "unknown");
    }

    #[test]
    fn readiness_timeout_default() {
        let rc = vigil::config::ReadinessConfig::default();
        assert_eq!(
            readiness_timeout(&rc),
            Duration::from_millis(DEFAULT_READINESS_TIMEOUT)
        );
    }

    fn parse_service(toml: &str) -> ServiceConfig {
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn signal_readiness_is_not_ready_from_probe() {
        // A `signal`-type readiness must only become ready when the child
        // actually raises the signal — the probe itself must never declare it
        // ready.
        let cfg = parse_service(
            r#"
            [service]
            command = "/bin/true"
            [service.readiness]
            type = "signal"
            signal = "USR1"
            "#,
        );
        let rc = &cfg.service.readiness;
        let mut probe =
            ReadinessProbe::new(rc, "test", socket_readiness_target(&cfg));
        assert!(
            matches!(probe.tick(), ProbeResult::Pending),
            "signal readiness must wait for the signal, not the probe"
        );
    }

    #[test]
    fn socket_readiness_falls_back_to_first_listen_spec() {
        // sshd-style: `[socket] listen = ["tcp:22"]` with `readiness.type =
        // "socket"` and no explicit `check` should resolve to the listen
        // endpoint instead of being rejected.
        let cfg = parse_service(
            r#"
            [service]
            command = "/bin/true"
            [service.readiness]
            type = "socket"
            [socket]
            listen = ["tcp:22"]
            "#,
        );
        let rc = &cfg.service.readiness;
        let mut probe =
            ReadinessProbe::new(rc, "test", socket_readiness_target(&cfg));
        assert!(probe.invalid.is_none(), "probe should not be invalid");
        assert!(matches!(probe.kind, ReadinessType::Socket));
        assert!(probe.spec.is_some(), "socket probe should carry a target");
        // The socket is not yet listening, so it must report pending (not a
        // false failure).
        assert!(matches!(probe.tick(), ProbeResult::Pending));
    }

    #[test]
    fn socket_readiness_without_target_is_failed() {
        let cfg = parse_service(
            r#"
            [service]
            command = "/bin/true"
            [service.readiness]
            type = "socket"
            "#,
        );
        let rc = &cfg.service.readiness;
        let mut probe =
            ReadinessProbe::new(rc, "test", socket_readiness_target(&cfg));
        assert!(probe.invalid.is_some());
        assert!(matches!(probe.tick(), ProbeResult::Failed(_)));
    }
}

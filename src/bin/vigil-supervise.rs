use anyhow::{Context, Result};
use nix::sys::resource::{setrlimit, Resource};
use nix::sys::signal::Signal;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{chdir, execvp, fork, ForkResult, Gid, Pid, Uid};
use std::env;
use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::time::{Duration, Instant};

use vigil::config::{LogType, OutputTarget, RestartPolicy, ServiceConfig};
use vigil::{VIGIL_LOG_DIR, VIGIL_SUPERVISE_DIR};

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "usage: vigil-supervise <service_name> <config_path> [log_dir]"
        );
        exit(1);
    }

    let service_name = &args[1];
    let config_path = &args[2];
    let default_log_dir = args
        .get(3)
        .map(|s| s.as_str())
        .unwrap_or(VIGIL_LOG_DIR);

    let config_content = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read config: {}", config_path))?;
    let config: ServiceConfig =
        toml::from_str(&config_content).with_context(|| "failed to parse config")?;

    let supervise_dir = PathBuf::from(format!(
        "{}/{}",
        VIGIL_SUPERVISE_DIR,
        service_name
    ));
    fs::create_dir_all(&supervise_dir)?;

    let status_dir = supervise_dir.join("status");
    fs::create_dir_all(&status_dir)?;

    eprintln!(
        "vigil-supervise [{}]: starting service supervisor",
        service_name
    );

    block_stop_signals()?;

    let mut restart_count: u32 = 0;
    let mut current_backoff = config.service.restart.backoff_initial_ms;
    let mut last_restart = Instant::now();

    loop {
        let log_path = resolve_log_path(&config, default_log_dir, service_name);
        if let Err(e) = fs::create_dir_all(&log_path) {
            eprintln!(
                "vigil-supervise [{}]: failed to create log dir {}: {}",
                service_name,
                log_path.display(),
                e
            );
        }

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
                run_service_child(service_name, &config, log_write);
                exit(127);
            }
            Err(e) => {
                eprintln!(
                    "vigil-supervise [{}]: fork failed: {}",
                    service_name, e
                );
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
            Some(spawn_vigillog(
                service_name,
                &log_path,
                &config,
                log_read,
            ))
        } else {
            None
        };

        if log_read >= 0 {
            unsafe {
                libc::close(log_read);
            }
        }

        let service_pgid = child_pid.as_raw();

        writeln_state(&status_dir, "running", Some(child_pid.as_raw() as u32))?;
        eprintln!(
            "vigil-supervise [{}]: service running as PID {}/PGID {}",
            service_name, child_pid.as_raw(), service_pgid
        );

        let outcome = supervise_child(service_name, &config, child_pid, vigillog_pid);

        match outcome {
            ChildOutcome::StoppedCleanly(code, signal) => {
                writeln_state(&status_dir, "stopped", None)?;

                let should_restart = match config.service.restart.policy {
                    RestartPolicy::Never => false,
                    RestartPolicy::Always => true,
                    RestartPolicy::OnFailure => {
                        !matches!((code, signal), (Some(0), None))
                    }
                    RestartPolicy::OnAbnormal => signal.is_some(),
                };

                if !should_restart {
                    eprintln!(
                        "vigil-supervise [{}]: service terminated ({}) and restart policy is {:?}; giving up",
                        service_name,
                        describe_exit(code, signal),
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

                run_finish_script(service_name, &config, &log_path);

                restart_count += 1;
                if last_restart.elapsed() >= Duration::from_secs(600)
                    && restart_count > 1
                {
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
                sleep_interruptible(Duration::from_millis(current_backoff), &status_dir)?;
                current_backoff =
                    (current_backoff as f64 * config.service.restart.backoff_multiplier) as u64;
                if current_backoff > config.service.restart.backoff_max_ms {
                    current_backoff = config.service.restart.backoff_max_ms;
                }
            }
            ChildOutcome::StopRequested => {
                eprintln!(
                    "vigil-supervise [{}]: stop requested; exiting",
                    service_name
                );
                writeln_state(&status_dir, "stopped", None)?;
                run_finish_script(service_name, &config, &log_path);
                break;
            }
        }
    }

    eprintln!("vigil-supervise [{}]: supervisor exiting", service_name);
    Ok(())
}

enum ChildOutcome {
    StoppedCleanly(Option<i32>, Option<Signal>),
    StopRequested,
}

fn reset_signal_mask() {
    let mut empty: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut empty);
        libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
    }
}

fn block_stop_signals() -> Result<()> {
    let mut sigset: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut sigset);
        libc::sigaddset(&mut sigset, libc::SIGCHLD);
        libc::sigaddset(&mut sigset, libc::SIGTERM);
        libc::sigaddset(&mut sigset, libc::SIGINT);
        libc::sigaddset(&mut sigset, libc::SIGHUP);
        libc::sigprocmask(
            libc::SIG_BLOCK,
            &sigset,
            std::ptr::null_mut(),
        );
    }
    Ok(())
}

fn pop_signal_detail(sigset: &libc::sigset_t) -> Result<Option<i32>> {
    let timeout = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let signo = unsafe {
        libc::sigtimedwait(sigset, std::ptr::null_mut(), &timeout)
    };
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
) -> ChildOutcome {
    let sigset: libc::sigset_t = unsafe {
        let mut s = std::mem::zeroed();
        libc::sigemptyset(&mut s);
        libc::sigaddset(&mut s, libc::SIGCHLD);
        libc::sigaddset(&mut s, libc::SIGTERM);
        libc::sigaddset(&mut s, libc::SIGINT);
        libc::sigaddset(&mut s, libc::SIGHUP);
        s
    };

    let timeout = Duration::from_millis(200);
    let mut vigillog_pid = vigillog_pid;

    loop {
        match waitpid(child_pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(_, code)) => {
                reap_vigillog(vigillog_pid);
                return ChildOutcome::StoppedCleanly(Some(code), None);
            }
            Ok(WaitStatus::Signaled(_, sig, _)) => {
                reap_vigillog(vigillog_pid);
                return ChildOutcome::StoppedCleanly(None, Some(sig));
            }
            Ok(WaitStatus::StillAlive) | Ok(WaitStatus::Stopped(_, _)) => {}
            Ok(_) => {
                reap_vigillog(vigillog_pid);
                return ChildOutcome::StoppedCleanly(None, None);
            }
            Err(nix::errno::Errno::ECHILD) => {
                reap_vigillog(vigillog_pid);
                return ChildOutcome::StoppedCleanly(None, None);
            }
            Err(e) => {
                eprintln!(
                    "vigil-supervise [{}]: waitpid error: {}",
                    service_name, e
                );
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
                    child_pid.as_raw(),
                    shutdown_signal,
                    kill_signal,
                    grace,
                    service_name,
                );
                reap_vigillog(vigillog_pid);
                return ChildOutcome::StopRequested;
            }
            Some(libc::SIGCHLD) => {
                continue;
            }
            Some(_) => {}
            None => {}
        }

        std::thread::sleep(timeout);
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

fn resolve_log_path(config: &ServiceConfig, default_log_dir: &str, name: &str) -> PathBuf {
    match config.logging.path.as_ref() {
        Some(p) => p.clone(),
        None => PathBuf::from(default_log_dir).join(name),
    }
}

fn create_log_pipe(config: &ServiceConfig, service_name: &str) -> (i32, i32) {
    if config.logging.kind == LogType::None
        || (config.service.stdout == OutputTarget::Null
            && config.service.stderr == OutputTarget::Null)
        || (config.service.stdout != OutputTarget::Log
            && config.service.stderr != OutputTarget::Log)
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

fn spawn_vigillog(
    service_name: &str,
    log_path: &Path,
    config: &ServiceConfig,
    log_read: i32,
) -> Pid {
    let leaf = log_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("service");
    let parent = log_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/var/log/vigil"));

    match unsafe { fork() } {
        Ok(ForkResult::Parent { child }) => child,
        Ok(ForkResult::Child) => {
            reset_signal_mask();

            if log_read != 0 {
                unsafe {
                    libc::dup2(log_read, 0);
                    libc::close(log_read);
                }
            }

            let exe_path = std::env::current_exe().ok();
            let name = CString::new("vigillog").unwrap();

            let mut search_paths: Vec<String> = Vec::new();
            if let Some(ref exe) = exe_path {
                if let Some(dir) = exe.parent() {
                    search_paths.push(dir.join("vigillog").to_string_lossy().to_string());
                }
            }
            search_paths.extend(
                ["/usr/local/bin", "/usr/bin", "/bin"]
                    .iter()
                    .map(|p| format!("{}/vigillog", p)),
            );

            let service_c = CString::new(leaf.to_string()).unwrap();
            let log_dir_c = CString::new(parent.to_string_lossy().to_string()).unwrap();
            let size_c = CString::new(config.logging.max_size_mb.to_string()).unwrap();
            let files_c = CString::new(config.logging.max_files.to_string()).unwrap();
            let ts_c = CString::new(if config.logging.timestamp { "1" } else { "0" }).unwrap();

            for path in &search_paths {
                if let Ok(c_path) = CString::new(path.as_str()) {
                    if execvp(
                        &c_path,
                        &[
                            name.clone(),
                            service_c.clone(),
                            log_dir_c.clone(),
                            size_c.clone(),
                            files_c.clone(),
                            ts_c.clone(),
                        ],
                    )
                    .is_ok()
                    {
                        unreachable!();
                    }
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
                "vigil-supervise [{}]: failed to spawn vigillog: {}",
                service_name, e
            );
            exit(1);
        }
    }
}

fn run_service_child(
    service_name: &str,
    config: &ServiceConfig,
    log_write: i32,
) {
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

    for (key, value) in &config.service.environment {
        env::set_var(key, value);
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

    let mut argv_ptrs: Vec<*const libc::c_char> =
        argv_c.iter().map(|c| c.as_ptr()).collect();
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

fn dup_stdio(
    config: &ServiceConfig,
    log_write: i32,
) {
    for (stream_fd, target) in [
        (1, &config.service.stdout),
        (2, &config.service.stderr),
    ] {
        match target {
            OutputTarget::Log => {
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
            OutputTarget::Syslog => {
                redirect_to_dev_null(stream_fd, libc::O_WRONLY);
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
    if config.service.group != "root" {
        if let Some(group) = users::get_group_by_name(&config.service.group) {
            let gid = Gid::from_raw(group.gid());
            nix::unistd::setgid(gid)?;
        }
    }

    if config.service.user != "root" {
        if let Some(user) = users::get_user_by_name(&config.service.user) {
            let uid = Uid::from_raw(user.uid());
            nix::unistd::setuid(uid)?;
        }
    }

    if let Some(max_files) = config.service.resource_limits.max_files {
        setrlimit(Resource::RLIMIT_NOFILE, max_files, max_files)?;
    }
    if let Some(max_procs) = config.service.resource_limits.max_procs {
        setrlimit(Resource::RLIMIT_NPROC, max_procs, max_procs)?;
    }
    if let Some(max_memory) = config.service.resource_limits.max_memory_mb {
        let bytes = max_memory * 1024 * 1024;
        setrlimit(Resource::RLIMIT_AS, bytes, bytes)?;
    }

    Ok(())
}

fn writeln_state(status_dir: &Path, state: &str, pid: Option<u32>) -> Result<()> {
    let state_path = status_dir.join("state");
    let mut f = fs::File::create(&state_path)?;
    writeln!(f, "{}", state)?;

    if let Some(pid) = pid {
        fs::write(status_dir.join("pid"), pid.to_string())?;
    }

    Ok(())
}

fn save_restart_count(status_dir: &Path, count: u32) {
    let _ = fs::write(status_dir.join("restarts"), count.to_string());
}

fn run_finish_script(
    service_name: &str,
    config: &ServiceConfig,
    log_path: &Path,
) {
    let config_dir = Path::new(&config.service.command)
        .parent()
        .unwrap_or(Path::new("/"));
    let finish_path = config_dir.join("finish");

    if finish_path.exists() {
        eprintln!(
            "vigil-supervise [{}]: running finish script {}",
            service_name,
            finish_path.display()
        );
        let _ = std::process::Command::new(&finish_path)
            .arg(service_name)
            .env("SERVICE_NAME", service_name)
            .env("LOG_DIR", log_path)
            .env("RESTART_COUNT", "0")
            .output();
    }
}

use anyhow::{Context, Result};
use nix::sys::signal::Signal;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{execvp, fork, ForkResult, Pid};
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs;
use std::io::BufRead;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::time::{Duration, Instant};

use vigil::config::{GlobalConfig, ServiceConfig};
use vigil::dep::DepGraph;
use vigil::protocol::{self, Request, Response, ServiceInfo, ServiceStatus};

struct ServiceState {
    config: ServiceConfig,
    config_path: PathBuf,
    state: String,
    pid: Option<u32>,
    restart_count: u32,
    start_time: Option<Instant>,
    enabled: bool,
}

struct Scanner {
    config: GlobalConfig,
    services: HashMap<String, ServiceState>,
    dep_graph: DepGraph,
    control_listener: Option<UnixListener>,
    running: bool,
}

fn main() -> Result<()> {
    let config = load_global_config()?;

    ensure_directories(&config)?;

    let mut scanner = Scanner {
        config: config.clone(),
        services: HashMap::new(),
        dep_graph: DepGraph::new(),
        control_listener: None,
        running: true,
    };

    eprintln!("vigil-scan: loading service definitions");
    scanner.load_services()?;

    eprintln!("vigil-scan: building dependency graph");
    scanner.build_dep_graph()?;

    eprintln!(
        "vigil-scan: starting {} services",
        scanner.services.len()
    );
    scanner.start_services()?;

    eprintln!("vigil-scan: opening control socket");
    scanner.open_control_socket()?;

    eprintln!("vigil-scan: entering event loop");
    scanner.run_event_loop()?;

    Ok(())
}

fn load_global_config() -> Result<GlobalConfig> {
    let config_path = "/etc/vigil/vigil.toml";
    if Path::new(config_path).exists() {
        let content = fs::read_to_string(config_path)
            .context("failed to read vigil.toml")?;
        let config: GlobalConfig =
            toml::from_str(&content).context("failed to parse vigil.toml")?;
        Ok(config)
    } else {
        Ok(GlobalConfig::default())
    }
}

fn ensure_directories(config: &GlobalConfig) -> Result<()> {
    let dirs = [
        &config.service_dir,
        &config.target_dir,
        &config.log_dir,
        &config.runtime_dir,
    ];
    for dir in &dirs {
        fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }

    let supervise_dir = config.runtime_dir.join("supervise");
    fs::create_dir_all(&supervise_dir)?;

    if let Some(parent) = config.control_socket.parent() {
        fs::create_dir_all(parent)?;
    }

    Ok(())
}

impl Scanner {
    fn load_services(&mut self) -> Result<()> {
        if !self.config.service_dir.exists() {
            eprintln!(
                "vigil-scan: service dir {} does not exist",
                self.config.service_dir.display()
            );
            return Ok(());
        }

        for entry in fs::read_dir(&self.config.service_dir)
            .context("failed to read service directory")?
        {
            let entry = entry?;
            let path = entry.path();

            if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string();

                match fs::read_to_string(&path) {
                    Ok(content) => match toml::from_str::<ServiceConfig>(&content) {
                        Ok(config) => {
                            eprintln!("vigil-scan: loaded service '{}'", name);
                            self.services.insert(
                                name.clone(),
                                ServiceState {
                                    config,
                                    config_path: path,
                                    state: "stopped".into(),
                                    pid: None,
                                    restart_count: 0,
                                    start_time: None,
                                    enabled: true,
                                },
                            );
                        }
                        Err(e) => {
                            eprintln!(
                                "vigil-scan: failed to parse {}: {}",
                                path.display(),
                                e
                            );
                        }
                    },
                    Err(e) => {
                        eprintln!(
                            "vigil-scan: failed to read {}: {}",
                            path.display(),
                            e
                        );
                    }
                }
            }
        }

        Ok(())
    }

    fn build_dep_graph(&mut self) -> Result<()> {
        let mut graph = DepGraph::new();

        let names: Vec<String> = self.services.keys().cloned().collect();
        for name in &names {
            graph.add_service(name.clone());
        }

        for (name, state) in &self.services {
            for dep in &state.config.dependencies {
                graph.add_dependency(name, dep);
            }
        }

        let available: HashSet<String> = self.services.keys().cloned().collect();
        let missing = graph.get_missing_required(&available);
        for (service, dep) in &missing {
            eprintln!(
                "vigil-scan: WARNING: service '{}' requires '{}' which is not available",
                service, dep
            );
        }

        self.dep_graph = graph;
        Ok(())
    }

    fn start_services(&mut self) -> Result<()> {
        let names: Vec<String> = self.services.keys().cloned().collect();

        match self.dep_graph.resolve_order(&names) {
            Ok(order) => {
                for name in &order {
                    if let Some(state) = self.services.get(name.as_str()) {
                        if state.enabled {
                            self.start_service(name)?;
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("vigil-scan: dependency error: {}, starting all", e);
                for name in &names {
                    if let Some(state) = self.services.get(name.as_str()) {
                        if state.enabled {
                            self.start_service(name)?;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn start_service(&mut self, name: &str) -> Result<()> {
        let state = match self.services.get_mut(name) {
            Some(s) => s,
            None => return Err(anyhow::anyhow!("service '{}' not found", name)),
        };

        if state.state == "running" || state.state == "starting" {
            return Ok(());
        }

        let supervise_dir = self.config.runtime_dir.join("supervise").join(name);
        fs::create_dir_all(&supervise_dir)?;

        let status_dir = supervise_dir.join("status");
        fs::create_dir_all(&status_dir)?;

        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                eprintln!(
                    "vigil-scan: started supervisor for '{}' as PID {}",
                    name, child
                );
                state.state = "running".into();
                state.pid = Some(child.as_raw() as u32);
                state.start_time = Some(Instant::now());
                Ok(())
            }
            Ok(ForkResult::Child) => {
                let exe_path = std::env::current_exe().ok();
                let supervise_name = CString::new("vigil-supervise").unwrap();

                let service_name_c = CString::new(name.to_string()).unwrap();
                let config_path_c = CString::new(
                    state.config_path.to_string_lossy().to_string(),
                )
                .unwrap();
                let log_dir_c = CString::new(
                    self.config.log_dir.to_string_lossy().to_string(),
                )
                .unwrap();

                let search_paths = if let Some(ref exe) = exe_path {
                    if let Some(dir) = exe.parent() {
                        let mut paths: Vec<String> = vec![dir
                            .join("vigil-supervise")
                            .to_string_lossy()
                            .to_string()];
                        paths.extend(
                            ["/usr/local/bin", "/usr/bin", "/bin"]
                                .iter()
                                .map(|p| {
                                    format!("{}/vigil-supervise", p)
                                }),
                        );
                        paths
                    } else {
                        default_supervise_paths()
                    }
                } else {
                    default_supervise_paths()
                };

                for path in &search_paths {
                    if let Ok(c_path) = CString::new(path.as_str()) {
                        if execvp(
                            &c_path,
                            &[
                                supervise_name.clone(),
                                service_name_c.clone(),
                                config_path_c.clone(),
                                log_dir_c.clone(),
                            ],
                        )
                        .is_ok()
                        {
                            unreachable!();
                        }
                    }
                }

                eprintln!(
                    "vigil-scan: failed to exec vigil-supervise for {}",
                    name
                );
                exit(1);
            }
            Err(e) => {
                Err(anyhow::anyhow!(
                    "fork failed for service '{}': {}",
                    name,
                    e
                ))
            }
        }
    }

    fn stop_service(&mut self, name: &str) -> Result<()> {
        let state = match self.services.get_mut(name) {
            Some(s) => s,
            None => return Err(anyhow::anyhow!("service '{}' not found", name)),
        };

        if state.state == "stopped" || state.state == "failed" {
            return Ok(());
        }

        if let Some(pid) = state.pid {
            let nix_pid = Pid::from_raw(pid as i32);

            let shutdown_signal = match state.config.service.shutdown.signal.as_str() {
                "HUP" => Signal::SIGHUP,
                "INT" => Signal::SIGINT,
                "QUIT" => Signal::SIGQUIT,
                "USR1" => Signal::SIGUSR1,
                "USR2" => Signal::SIGUSR2,
                _ => Signal::SIGTERM,
            };

            let _ = nix::sys::signal::kill(nix_pid, shutdown_signal);

            let timeout = Duration::from_millis(state.config.service.shutdown.timeout_ms);
            let start = Instant::now();

            loop {
                match waitpid(nix_pid, Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::StillAlive) => {
                        if start.elapsed() >= timeout {
                            eprintln!(
                                "vigil-scan: force killing '{}' (PID {})",
                                name, pid
                            );
                            let _ = nix::sys::signal::kill(nix_pid, Signal::SIGKILL);
                            let _ = waitpid(nix_pid, None);
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    _ => break,
                }
            }

            state.state = "stopped".into();
            state.pid = None;
            state.start_time = None;

            let status_dir = self
                .config
                .runtime_dir
                .join("supervise")
                .join(name)
                .join("status");
            let _ = fs::remove_file(status_dir.join("pid"));
            let _ = fs::write(status_dir.join("state"), "stopped");
        }

        eprintln!("vigil-scan: stopped service '{}'", name);
        Ok(())
    }

    fn open_control_socket(&mut self) -> Result<()> {
        let _ = fs::remove_file(&self.config.control_socket);

        let listener = UnixListener::bind(&self.config.control_socket)
            .with_context(|| {
                format!(
                    "failed to bind control socket: {}",
                    self.config.control_socket.display()
                )
            })?;

        listener.set_nonblocking(true)?;
        self.control_listener = Some(listener);

        eprintln!(
            "vigil-scan: control socket ready at {}",
            self.config.control_socket.display()
        );
        Ok(())
    }

    fn run_event_loop(&mut self) -> Result<()> {
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

        let mut last_reap = Instant::now();

        while self.running {
            let mut poll_fds = if let Some(ref listener) = self.control_listener {
                vec![nix::poll::PollFd::new(
                    unsafe { std::os::unix::io::BorrowedFd::borrow_raw(listener.as_raw_fd()) },
                    nix::poll::PollFlags::POLLIN,
                )]
            } else {
                vec![]
            };

            let _ = nix::poll::poll(
                &mut poll_fds,
                nix::poll::PollTimeout::try_from(100).unwrap(),
            );

            if !poll_fds.is_empty()
                && poll_fds[0]
                    .revents()
                    .unwrap_or(nix::poll::PollFlags::empty())
                    .contains(nix::poll::PollFlags::POLLIN)
            {
                if let Some(ref listener) = self.control_listener {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            self.handle_control_connection(stream);
                        }
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(e) => {
                            eprintln!("vigil-scan: accept error: {}", e);
                        }
                    }
                }
            }

            if last_reap.elapsed() >= Duration::from_millis(100) {
                self.reap_supervisors();
                last_reap = Instant::now();
            }

            let timeout = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            let signo = unsafe {
                libc::sigtimedwait(
                    &sigset,
                    std::ptr::null_mut(),
                    &timeout,
                )
            };

            if signo > 0 {
                match signo {
                    libc::SIGCHLD => {
                        self.reap_supervisors();
                    }
                    libc::SIGTERM | libc::SIGINT => {
                        eprintln!("vigil-scan: shutdown signal received");
                        self.running = false;
                    }
                    libc::SIGHUP => {
                        eprintln!("vigil-scan: reload requested (SIGHUP)");
                        self.reload_services()?;
                    }
                    _ => {}
                }
            }
        }

        self.shutdown()?;
        Ok(())
    }

    fn reap_supervisors(&mut self) {
        loop {
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) => break,
                Ok(WaitStatus::Exited(pid, _)) | Ok(WaitStatus::Signaled(pid, _, _)) => {
                    let pid_val = pid.as_raw() as u32;
                    for (name, state) in self.services.iter_mut() {
                        if state.pid == Some(pid_val) {
                            eprintln!(
                                "vigil-scan: supervisor for '{}' (PID {}) terminated",
                                name, pid_val
                            );
                            state.pid = None;
                            state.state = "stopped".into();
                            state.restart_count += 1;
                            break;
                        }
                    }
                }
                Ok(_) => {}
                Err(nix::errno::Errno::ECHILD) => break,
                Err(_) => break,
            }
        }
    }

    fn handle_control_connection(&mut self, mut stream: UnixStream) {
        let request: Request = match protocol::read_message(&mut stream) {
            Ok(r) => r,
            Err(e) => {
                let _ = protocol::write_message(
                    &mut stream,
                    &Response::Error {
                        message: format!("failed to read request: {}", e),
                    },
                );
                return;
            }
        };

        let response = self.process_request(request);

        if let Err(e) = protocol::write_message(&mut stream, &response) {
            eprintln!("vigil-scan: failed to write response: {}", e);
        }
    }

    fn process_request(&mut self, request: Request) -> Response {
        match request {
            Request::Ping => Response::Pong,

            Request::List => {
                let infos: Vec<ServiceInfo> = self
                    .services
                    .iter()
                    .map(|(name, state)| ServiceInfo {
                        name: name.clone(),
                        state: state.state.clone(),
                        pid: state.pid,
                        description: state.config.service.description.clone(),
                    })
                    .collect();
                Response::List(infos)
            }

            Request::Status { service } => match service {
                Some(name) => {
                    if let Some(state) = self.services.get(&name) {
                        Response::Status(ServiceStatus {
                            name: name.clone(),
                            state: state.state.clone(),
                            pid: state.pid,
                            uptime_secs: state
                                .start_time
                                .map(|t| t.elapsed().as_secs())
                                .unwrap_or(0),
                            restart_count: state.restart_count,
                            description: state.config.service.description.clone(),
                            command: state.config.service.command.clone(),
                        })
                    } else {
                        Response::Error {
                            message: format!("service '{}' not found", name),
                        }
                    }
                }
                None => {
                    let infos: Vec<ServiceInfo> = self
                        .services
                        .iter()
                        .map(|(name, state)| ServiceInfo {
                            name: name.clone(),
                            state: state.state.clone(),
                            pid: state.pid,
                            description: state.config.service.description.clone(),
                        })
                        .collect();
                    Response::List(infos)
                }
            },

            Request::Start { service } => {
                if !self.services.contains_key(&service) {
                    return Response::Error {
                        message: format!("service '{}' not found", service),
                    };
                }

                match self.start_service(&service) {
                    Ok(()) => Response::Ok {
                        message: format!("service '{}' started", service),
                    },
                    Err(e) => Response::Error {
                        message: format!("failed to start '{}': {}", service, e),
                    },
                }
            }

            Request::Stop { service } => {
                if !self.services.contains_key(&service) {
                    return Response::Error {
                        message: format!("service '{}' not found", service),
                    };
                }

                match self.stop_service(&service) {
                    Ok(()) => Response::Ok {
                        message: format!("service '{}' stopped", service),
                    },
                    Err(e) => Response::Error {
                        message: format!("failed to stop '{}': {}", service, e),
                    },
                }
            }

            Request::Restart { service } => {
                if !self.services.contains_key(&service) {
                    return Response::Error {
                        message: format!("service '{}' not found", service),
                    };
                }

                match self.stop_service(&service) {
                    Ok(()) => match self.start_service(&service) {
                        Ok(()) => Response::Ok {
                            message: format!("service '{}' restarted", service),
                        },
                        Err(e) => Response::Error {
                            message: format!(
                                "stopped '{}' but failed to restart: {}",
                                service, e
                            ),
                        },
                    },
                    Err(e) => Response::Error {
                        message: format!("failed to stop '{}': {}", service, e),
                    },
                }
            }

            Request::Log { service, lines } => {
                let log_path = self.config.log_dir.join(&service).join("current");
                match read_log_lines(&log_path, lines) {
                    Ok(log_lines) => Response::LogLines(log_lines),
                    Err(e) => Response::Error {
                        message: format!(
                            "failed to read logs for '{}': {}",
                            service, e
                        ),
                    },
                }
            }

            Request::Reload => match self.reload_services() {
                Ok(()) => Response::Ok {
                    message: "services reloaded".into(),
                },
                Err(e) => Response::Error {
                    message: format!("reload failed: {}", e),
                },
            },

            Request::Shutdown { action } => {
                self.running = false;
                Response::Ok {
                    message: format!("shutdown initiated: {:?}", action),
                }
            }
        }
    }

    fn reload_services(&mut self) -> Result<()> {
        let old_services: HashMap<String, ServiceState> =
            std::mem::take(&mut self.services);
        self.dep_graph = DepGraph::new();

        self.load_services()?;

        for (name, state) in &self.services {
            self.dep_graph.add_service(name.clone());
            for dep in &state.config.dependencies {
                self.dep_graph.add_dependency(name, dep);
            }
        }

        let new_names: HashSet<String> = self.services.keys().cloned().collect();
        let old_names: HashSet<String> = old_services.keys().cloned().collect();

        for name in &old_names {
            if !new_names.contains(name) {
                eprintln!("vigil-scan: service '{}' removed, stopping", name);
                if let Some(old_state) = old_services.get(name) {
                    if let Some(pid) = old_state.pid {
                        unsafe {
                            libc::kill(pid as i32, libc::SIGTERM);
                        }
                    }
                }
            }
        }

        for (name, state) in self.services.iter_mut() {
            if let Some(old_state) = old_services.get(name) {
                if old_state.state == "running" {
                    state.state = "running".into();
                    state.pid = old_state.pid;
                    state.restart_count = old_state.restart_count;
                    state.start_time = old_state.start_time;
                }
            }
        }

        eprintln!("vigil-scan: configuration reloaded");
        Ok(())
    }

    fn shutdown(&mut self) -> Result<()> {
        eprintln!("vigil-scan: shutting down services");

        let names: Vec<String> = self.services.keys().cloned().collect();

        let order = self
            .dep_graph
            .resolve_order(&names)
            .unwrap_or_else(|_| names.clone());

        let reverse_order: Vec<String> = order.into_iter().rev().collect();

        for name in &reverse_order {
            if let Some(state) = self.services.get(name.as_str()) {
                if state.state == "running" || state.state == "starting" {
                    eprintln!("vigil-scan: stopping '{}'", name);
                    if let Err(e) = self.stop_service(name) {
                        eprintln!(
                            "vigil-scan: failed to stop '{}': {}",
                            name, e
                        );
                    }
                }
            }
        }

        let _ = fs::remove_file(&self.config.control_socket);

        eprintln!("vigil-scan: shutdown complete");
        Ok(())
    }
}

fn default_supervise_paths() -> Vec<String> {
    vec![
        "/usr/local/bin/vigil-supervise".into(),
        "/usr/bin/vigil-supervise".into(),
        "/bin/vigil-supervise".into(),
    ]
}

fn read_log_lines(path: &Path, max_lines: usize) -> Result<Vec<String>> {
    let file = fs::File::open(path)
        .with_context(|| format!("log file not found: {}", path.display()))?;

    let reader = std::io::BufReader::new(file);
    let lines: Vec<String> = reader
        .lines()
        .collect::<Result<Vec<_>, _>>()?;

    let start = if lines.len() > max_lines {
        lines.len() - max_lines
    } else {
        0
    };

    Ok(lines[start..].to_vec())
}

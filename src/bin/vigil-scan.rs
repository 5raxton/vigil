use anyhow::{Context, Result};
use nix::sys::signal::Signal;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{execvp, fork, ForkResult, Pid};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::CString;
use std::fs;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::time::{Duration, Instant};

use vigil::config::{GlobalConfig, LogType, ServiceConfig, TargetConfig};
use vigil::dep::DepGraph;
use vigil::protocol::{self, Request, Response, ServiceInfo, ServiceStatus};

/// A supervisor that survived this long resets the crash budget, mirroring
/// the supervisor-side backoff cooldown so a long-stable service is never
/// refused a respawn because of an ancient crash loop.
const RESPAWN_COOLDOWN: Duration = Duration::from_secs(600);
/// There is no SIGKILL'able control-client timeout worth waiting for: a
/// control request is tiny and arrives in one write, so a client that stalls
/// mid-request is handled quickly to keep child reaping timely.
const CONTROL_READ_TIMEOUT: Duration = Duration::from_secs(5);

struct ServiceState {
    config: ServiceConfig,
    config_path: PathBuf,
    state: String,
    supervisor_pid: Option<u32>,
    restart_count: u32,
    start_time: Option<Instant>,
    enabled: bool,
    last_respawn: Option<Instant>,
}

struct Scanner {
    config: GlobalConfig,
    services: HashMap<String, ServiceState>,
    dep_graph: DepGraph,
    control_listener: Option<UnixListener>,
    /// Services that must come up with the boot target: those named in
    /// `[target] requires` plus enabled, non-optional target entries and
    /// everything pulled in by their required dependencies.
    required_services: HashSet<String>,
    /// Set when a required service gives up, so operators can see the boot
    /// did not complete cleanly (persisted to `<runtime>/degraded`).
    degraded: bool,
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
        required_services: HashSet::new(),
        degraded: false,
        running: true,
    };

    eprintln!("vigil-scan: loading service definitions");
    scanner.load_services()?;

    eprintln!("vigil-scan: building dependency graph");
    scanner.build_dep_graph()?;

    eprintln!("vigil-scan: resolving boot target");
    scanner.apply_target(&config.default_target)?;

    eprintln!(
        "vigil-scan: starting {} services",
        scanner.services.values().filter(|s| s.enabled).count()
    );
    scanner.start_services()?;

    eprintln!("vigil-scan: opening control socket");
    scanner.open_control_socket()?;

    eprintln!("vigil-scan: entering event loop");
    scanner.run_event_loop()?;

    Ok(())
}

fn load_global_config() -> Result<GlobalConfig> {
    let mut config_path: Option<PathBuf> = None;

    let mut args = std::env::args();
    let _program = args.next();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-c" | "--config" => {
                let value = args
                    .next()
                    .context("--config requires a path argument")?;
                config_path = Some(PathBuf::from(value));
            }
            other => anyhow::bail!("unsupported vigil-scan argument '{}'", other),
        }
    }

    // Precedence: an explicit --config beats VIGIL_CONFIG, which beats the
    // compiled-in default path.
    let config_path = match config_path {
        Some(p) => p,
        None => std::env::var_os("VIGIL_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/etc/vigil/vigil.toml")),
    };

    if Path::new(&config_path).exists() {
        let content = fs::read_to_string(&config_path)
            .with_context(|| format!("failed to read {}", config_path.display()))?;
        let config: GlobalConfig =
            toml::from_str(&content).with_context(|| "failed to parse vigil.toml")?;
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
        fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
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

        for entry in
            fs::read_dir(&self.config.service_dir).context("failed to read service directory")?
        {
            let entry = entry?;
            let path = entry.path();

            if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string();

                if !is_valid_service_name(&name) {
                    eprintln!(
                        "vigil-scan: skipping '{}' (invalid service name; must be alphanumeric, \
                         '.', '_' or '-')",
                        path.display()
                    );
                    continue;
                }

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
                                    supervisor_pid: None,
                                    restart_count: 0,
                                    start_time: None,
                                    enabled: true,
                                    last_respawn: None,
                                },
                            );
                        }
                        Err(e) => {
                            eprintln!("vigil-scan: failed to parse {}: {}", path.display(), e);
                        }
                    },
                    Err(e) => {
                        eprintln!("vigil-scan: failed to read {}: {}", path.display(), e);
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

    fn apply_target(&mut self, target_name: &str) -> Result<()> {
        let target_path = self.config.target_dir.join(format!("{}.toml", target_name));

        // A missing or empty target means "bring up everything"; every
        // configured service is considered required.
        let mut enable_all = || {
            for state in self.services.values_mut() {
                state.enabled = true;
            }
            self.required_services = self.services.keys().cloned().collect();
        };

        if !target_path.exists() {
            eprintln!(
                "vigil-scan: boot target '{}' not found at {}; enabling all services",
                target_name,
                target_path.display()
            );
            enable_all();
            self.degraded = false;
            let _ = fs::remove_file(self.config.runtime_dir.join("degraded"));
            return Ok(());
        }

        let content = fs::read_to_string(&target_path)
            .with_context(|| format!("failed to read target: {}", target_path.display()))?;
        let target: TargetConfig =
            toml::from_str(&content).with_context(|| "failed to parse target")?;

        // A fresh resolution of the target starts from a clean slate: clear any
        // degradation recorded by a previous boot/reload so that a now-healthy
        // target does not keep a stale degraded marker. Any problem detected
        // while resolving *this* target (e.g. a missing required service below)
        // re-arms the flag and is NOT clobbered.
        self.degraded = false;
        let _ = fs::remove_file(self.config.runtime_dir.join("degraded"));

        let target_services = target.services;
        let mut explicitly_disabled: HashSet<String> = HashSet::new();

        if target_services.is_empty() {
            eprintln!(
                "vigil-scan: target '{}' defines no services; enabling all",
                target_name
            );
            enable_all();
        } else {
            let mut required: HashSet<String> = target.target.requires.iter().cloned().collect();
            for (name, state) in self.services.iter_mut() {
                match target_services.get(name) {
                    Some(entry) => {
                        state.enabled = entry.enabled;
                        if !entry.enabled {
                            explicitly_disabled.insert(name.clone());
                        }
                        if entry.enabled && !entry.optional {
                            required.insert(name.clone());
                        }
                    }
                    None => state.enabled = false,
                }
            }

            // `[target] requires` always pulls services in, even if they are
            // absent from (or disabled in) the services map.
            for req in &target.target.requires {
                if let Some(state) = self.services.get_mut(req) {
                    if !state.enabled {
                        state.enabled = true;
                        eprintln!("vigil-scan: enabling required service '{}'", req);
                    }
                } else {
                    eprintln!(
                        "vigil-scan: WARNING: target requires '{}' which is not available",
                        req
                    );
                    self.degraded = true;
                }
            }

            // Close the "required" relation under required dependencies: if a
            // required service needs another service to come up first, that
            // dependency must come up too (and its failure degrades the target).
            let available: HashSet<String> = self.services.keys().cloned().collect();
            let mut queue: VecDeque<String> = required.iter().cloned().collect();
            while let Some(name) = queue.pop_front() {
                let deps: Vec<(String, bool)> = self
                    .services
                    .get(&name)
                    .map(|s| {
                        s.config
                            .dependencies
                            .iter()
                            .map(|d| (d.service.clone(), d.required))
                            .collect()
                    })
                    .unwrap_or_default();
                for (dep, dep_required) in deps {
                    if !available.contains(&dep) {
                        continue;
                    }
                    if !self.services[&dep].enabled {
                        self.services.get_mut(&dep).unwrap().enabled = true;
                        eprintln!(
                            "vigil-scan: enabling dependency '{}' of required service '{}'",
                            dep, name
                        );
                    }
                    if dep_required && !required.contains(&dep) {
                        required.insert(dep.clone());
                        queue.push_back(dep);
                    }
                }
            }

            // `wants` services come up unless explicitly disabled in the target.
            let enabled_services: Vec<String> = self
                .services
                .iter()
                .filter(|(_, s)| s.enabled)
                .map(|(n, _)| n.clone())
                .collect();
            for name in enabled_services {
                for wanted in self.dep_graph.get_wanted_services(&name) {
                    if !explicitly_disabled.contains(&wanted)
                        && available.contains(&wanted)
                        && !self.services[&wanted].enabled
                    {
                        self.services.get_mut(&wanted).unwrap().enabled = true;
                        eprintln!("vigil-scan: enabling wanted service '{}'", wanted);
                    }
                }
            }

            // Only services we actually know about can be required.
            self.required_services = required
                .into_iter()
                .filter(|n| self.services.contains_key(n))
                .collect();
        }

        let enabled: Vec<String> = self
            .services
            .iter()
            .filter(|(_, s)| s.enabled)
            .map(|(n, _)| n.clone())
            .collect();
        eprintln!(
            "vigil-scan: target '{}' enables {} service(s)",
            target_name,
            enabled.len()
        );

        if !self.required_services.is_empty() {
            eprintln!(
                "vigil-scan: {} service(s) are required by this target",
                self.required_services.len()
            );
        }

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

        // Build the exec arguments up front (in the parent) so a malformed
        // config value (e.g. a NUL byte smuggled into log_dir/runtime_dir)
        // is rejected with a clean error rather than panicking — with
        // `panic = "abort"` a panic would take down the whole supervisor.
        let service_name_c = to_cstring(name, "service name")?;
        let config_path_c = to_cstring(
            &state.config_path.to_string_lossy(),
            "config path",
        )?;
        let log_dir_c = to_cstring(&self.config.log_dir.to_string_lossy(), "log dir")?;
        let supervise_dir_c = to_cstring(
            &self.config.runtime_dir.join("supervise").to_string_lossy(),
            "supervise dir",
        )?;

        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                eprintln!(
                    "vigil-scan: started supervisor for '{}' as PID {}",
                    name, child
                );
                state.state = "running".into();
                state.supervisor_pid = Some(child.as_raw() as u32);
                state.start_time = Some(Instant::now());
                Ok(())
            }
            Ok(ForkResult::Child) => {
                let argv: Vec<CString> = vec![
                    CString::new("vigil-supervise").unwrap(),
                    service_name_c,
                    config_path_c,
                    log_dir_c,
                    supervise_dir_c,
                ];

                for path in vigil::util::exec_search_paths("vigil-supervise") {
                    if let Ok(path_c) = CString::new(path.to_string_lossy().as_bytes()) {
                        let _ = execvp(&path_c, &argv);
                    }
                }

                eprintln!("vigil-scan: failed to exec vigil-supervise for {}", name);
                exit(1);
            }
            Err(e) => Err(anyhow::anyhow!("fork failed for service '{}': {}", name, e)),
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

        if let Some(pid) = state.supervisor_pid {
            let nix_pid = Pid::from_raw(pid as i32);

            let grace = Duration::from_millis(state.config.service.shutdown.timeout_ms)
                + Duration::from_secs(2);
            let start = Instant::now();

            let _ = nix::sys::signal::kill(nix_pid, Signal::SIGTERM);

            while let Ok(WaitStatus::StillAlive) = waitpid(nix_pid, Some(WaitPidFlag::WNOHANG)) {
                if start.elapsed() >= grace {
                    eprintln!(
                        "vigil-scan: supervisor for '{}' (PID {}) did not exit; force killing",
                        name, pid
                    );
                    let _ = nix::sys::signal::kill(nix_pid, Signal::SIGKILL);
                    let _ = waitpid(nix_pid, None);
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }

            state.state = "stopped".into();
            state.supervisor_pid = None;
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

        let listener = UnixListener::bind(&self.config.control_socket).with_context(|| {
            format!(
                "failed to bind control socket: {}",
                self.config.control_socket.display()
            )
        })?;

        listener.set_nonblocking(true)?;

        // Restrict the control socket to root (mode 0600). Without an explicit
        // chmod the socket would inherit the process umask (typically 022),
        // leaving it writable by any local user — who could then start/stop
        // services or trigger a reboot/poweroff. The default umask can be
        // changed, so always force the safe mode here.
        let path = CString::new(self.config.control_socket.as_os_str().as_encoded_bytes())?;
        if unsafe { libc::chmod(path.as_ptr(), 0o600) } != 0 {
            anyhow::bail!(
                "failed to set control socket permissions: {}",
                std::io::Error::last_os_error()
            );
        }

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
            libc::sigprocmask(libc::SIG_BLOCK, &sigset, std::ptr::null_mut());
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
                    // Drain every pending connection (not just one) so a
                    // burst of control requests cannot push child reaping
                    // behind an ever-growing accept queue.
                    let mut streams = Vec::new();
                    loop {
                        match listener.accept() {
                            Ok((stream, _)) => streams.push(stream),
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                            Err(e) => {
                                eprintln!("vigil-scan: accept error: {}", e);
                                break;
                            }
                        }
                    }
                    for stream in streams {
                        self.handle_control_connection(stream);
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
            let signo = unsafe { libc::sigtimedwait(&sigset, std::ptr::null_mut(), &timeout) };

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
                    let name = self
                        .services
                        .iter()
                        .find(|(_, s)| s.supervisor_pid == Some(pid_val))
                        .map(|(n, _)| n.clone());

                    if let Some(name) = name {
                        eprintln!(
                            "vigil-scan: supervisor for '{}' (PID {}) terminated",
                            name, pid_val
                        );
                        let was_supervising = self.supervisor_was_live(&name);
                        let mut respawn = false;
                        if let Some(state) = self.services.get_mut(&name) {
                            // A required, enabled service is considered failed
                            // (degrading the boot) when its supervisor is gone
                            // AND either the service had already given up on its
                            // own (status != running) or the supervisor has been
                            // respawned up to the restart ceiling (crash loop).
                            let degraded = state.enabled
                                && self.required_services.contains(&name)
                                && (!was_supervising
                                    || state.restart_count
                                        >= state.config.service.restart.max_restarts);
                            state.supervisor_pid = None;
                            state.state = "stopped".into();
                            if state.enabled
                                && was_supervising
                                && state.restart_count < state.config.service.restart.max_restarts
                            {
                                // The crash counter decays: a supervisor that
                                // survived RESPAWN_COOLDOWN resets the budget
                                // rather than carrying an old crash loop
                                // against a long-stable service.
                                let cooldown_elapsed = state
                                    .last_respawn
                                    .map(|t| t.elapsed() >= RESPAWN_COOLDOWN)
                                    .unwrap_or(true);
                                state.restart_count = if cooldown_elapsed {
                                    1
                                } else {
                                    state.restart_count.saturating_add(1)
                                };
                                state.last_respawn = Some(Instant::now());
                                respawn = true;
                            } else {
                                state.restart_count = 0;
                            }
                            if degraded {
                                self.mark_degraded(&name);
                            }
                        }
                        if respawn {
                            eprintln!(
                                "vigil-scan: respawning supervisor for '{}' (second chance)",
                                name
                            );
                            if let Err(e) = self.start_service(&name) {
                                eprintln!(
                                    "vigil-scan: failed to respawn supervisor for '{}': {}",
                                    name, e
                                );
                                if let Some(state) = self.services.get_mut(&name) {
                                    state.restart_count = 0;
                                }
                            }
                        }
                    }
                }
                Ok(_) => {}
                Err(nix::errno::Errno::ECHILD) => break,
                Err(_) => break,
            }
        }
    }

    fn supervisor_was_live(&self, name: &str) -> bool {
        let path = self
            .config
            .runtime_dir
            .join("supervise")
            .join(name)
            .join("status")
            .join("state");
        match fs::read_to_string(&path) {
            Ok(s) => {
                let s = s.trim();
                s != "terminated" && s != "failed" && s != "stopped"
            }
            Err(_) => true,
        }
    }

    fn read_service_pid(&self, name: &str) -> Option<u32> {
        let pid_path = self
            .config
            .runtime_dir
            .join("supervise")
            .join(name)
            .join("status")
            .join("pid");
        fs::read_to_string(&pid_path)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
    }

    fn read_service_restarts(&self, name: &str) -> Option<u32> {
        let path = self
            .config
            .runtime_dir
            .join("supervise")
            .join(name)
            .join("status")
            .join("restarts");
        fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
    }

    /// Mark the boot as degraded: a service that is required by the active
    /// target has given up. Persists `<runtime>/degraded` so operators can see
    /// a boot did not complete cleanly.
    fn mark_degraded(&mut self, name: &str) {
        if self.degraded {
            return;
        }
        self.degraded = true;
        eprintln!(
            "vigil-scan: DEGRADED: required service '{}' failed or gave up",
            name
        );
        let _ = fs::write(
            self.config.runtime_dir.join("degraded"),
            format!("{}\n", name),
        );
    }

    /// Resolve the on-disk log file for a service, taking its configured
    /// logging kind into account. Services with `logging.kind = file` write
    /// to `logging.path` directly (or `<log_dir>/<service>`, mirrored by the
    /// vigillog file rotation inside it); `pipe` services write to
    /// `<path>/current`. syslog/none services have no scannable log file.
    fn log_file_for(&self, name: &str) -> Option<PathBuf> {
        let state = self.services.get(name)?;
        match state.config.logging.kind {
            LogType::Pipe => Some(
                state
                    .config
                    .logging
                    .path
                    .as_ref()
                    .map(|p| p.join("current"))
                    .unwrap_or_else(|| self.config.log_dir.join(name).join("current")),
            ),
            LogType::File => Some(
                state
                    .config
                    .logging
                    .path
                    .clone()
                    .unwrap_or_else(|| self.config.log_dir.join(name).join("current")),
            ),
            LogType::Syslog | LogType::None => None,
        }
    }

    fn handle_control_connection(&mut self, mut stream: UnixStream) {
        stream.set_read_timeout(Some(CONTROL_READ_TIMEOUT)).ok();
        stream.set_write_timeout(Some(CONTROL_READ_TIMEOUT)).ok();

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
                        pid: self.read_service_pid(name),
                        description: state.config.service.description.clone(),
                    })
                    .collect();
                Response::List(infos)
            }

            Request::Status { service } => match service {
                Some(name) => {
                    if let Some(state) = self.services.get(&name) {
                        let service_pid = self.read_service_pid(&name);
                        Response::Status(ServiceStatus {
                            name: name.clone(),
                            state: state.state.clone(),
                            pid: service_pid,
                            uptime_secs: state
                                .start_time
                                .map(|t| t.elapsed().as_secs())
                                .unwrap_or(0),
                            restart_count: self
                                .read_service_restarts(&name)
                                .unwrap_or(state.restart_count),
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
                            pid: self.read_service_pid(name),
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

                // A deliberate start from the control plane resets the crash
                // budget: an operator asking for a fresh start must not inherit
                // a stale restart counter that could instantly deny further
                // respawns.
                if let Some(state) = self.services.get_mut(&service) {
                    state.restart_count = 0;
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

                // Deliberate restart: reset the crash budget so a clean manual
                // restart is not refused moments later by a stale counter.
                if let Some(state) = self.services.get_mut(&service) {
                    state.restart_count = 0;
                }

                match self.stop_service(&service) {
                    Ok(()) => match self.start_service(&service) {
                        Ok(()) => Response::Ok {
                            message: format!("service '{}' restarted", service),
                        },
                        Err(e) => Response::Error {
                            message: format!("stopped '{}' but failed to restart: {}", service, e),
                        },
                    },
                    Err(e) => Response::Error {
                        message: format!("failed to stop '{}': {}", service, e),
                    },
                }
            }

            Request::Log { service, lines } => match self.log_file_for(&service) {
                Some(log_path) => match read_log_lines(&log_path, lines) {
                    Ok(log_lines) => Response::LogLines(log_lines),
                    Err(e) => Response::Error {
                        message: format!("failed to read logs for '{}': {}", service, e),
                    },
                },
                None => Response::Error {
                    message: format!(
                        "service '{}' has no log file (logging.kind is syslog/none)",
                        service
                    ),
                },
            },

            Request::Reload => match self.reload_services() {
                Ok(()) => Response::Ok {
                    message: "services reloaded".into(),
                },
                Err(e) => Response::Error {
                    message: format!("reload failed: {}", e),
                },
            },

            Request::Shutdown { action } => {
                let sig = match action {
                    vigil::protocol::ShutdownAction::Poweroff => libc::SIGUSR2,
                    vigil::protocol::ShutdownAction::Reboot => libc::SIGTERM,
                    vigil::protocol::ShutdownAction::Halt => libc::SIGUSR1,
                };
                eprintln!(
                    "vigil-scan: shutdown requested ({:?}); notifying PID 1",
                    action
                );
                unsafe {
                    libc::kill(1, sig);
                }
                self.running = false;
                Response::Ok {
                    message: format!("shutdown initiated: {:?}", action),
                }
            }
        }
    }

    fn reload_services(&mut self) -> Result<()> {
        let old_services: HashMap<String, ServiceState> = std::mem::take(&mut self.services);
        self.dep_graph = DepGraph::new();

        self.load_services()?;

        for (name, state) in &self.services {
            self.dep_graph.add_service(name.clone());
            for dep in &state.config.dependencies {
                self.dep_graph.add_dependency(name, dep);
            }
        }

        let new_names: HashSet<String> = self.services.keys().cloned().collect();

        // Services that were removed from the configuration are torn down.
        for (name, old_state) in &old_services {
            if !new_names.contains(name) {
                eprintln!("vigil-scan: service '{}' removed from config, stopping", name);
                if let Some(pid) = old_state.supervisor_pid {
                    unsafe {
                        libc::kill(pid as i32, libc::SIGTERM);
                    }
                }
            }
        }

        let target = self.config.default_target.clone();
        self.apply_target(&target)?;

        // Restore the runtime bookkeeping of every service that was running so
        // its supervisor is still tracked (needed both to keep it running and
        // to stop it cleanly below). `enabled` is NOT forced back to true: the
        // freshly re-applied target is authoritative.
        for (name, state) in self.services.iter_mut() {
            if let Some(old_state) = old_services.get(name) {
                if old_state.state == "running" {
                    state.state = "running".into();
                    state.supervisor_pid = old_state.supervisor_pid;
                    state.restart_count = old_state.restart_count;
                    state.start_time = old_state.start_time;
                    state.last_respawn = old_state.last_respawn;
                }
            }
        }

        // Stop services that were running but are no longer enabled by the
        // (possibly changed) target — without this, disabling a service in the
        // target and reloading would leave it running forever.
        let to_stop: Vec<String> = self
            .services
            .iter()
            .filter(|(_, s)| s.state == "running" && !s.enabled)
            .map(|(n, _)| n.clone())
            .collect();
        for name in to_stop {
            eprintln!("vigil-scan: service '{}' disabled on reload, stopping", name);
            if let Err(e) = self.stop_service(&name) {
                eprintln!("vigil-scan: failed to stop '{}' on reload: {}", name, e);
            }
        }

        // Start newly added services that should be enabled.
        let new_enabled: Vec<String> = self
            .services
            .iter()
            .filter(|(n, s)| s.enabled && !old_services.contains_key(n.as_str()))
            .map(|(n, _)| n.clone())
            .collect();
        for name in new_enabled {
            eprintln!("vigil-scan: starting newly added service '{}'", name);
            if let Err(e) = self.start_service(&name) {
                eprintln!("vigil-scan: failed to start '{}' on reload: {}", name, e);
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
                        eprintln!("vigil-scan: failed to stop '{}': {}", name, e);
                    }
                }
            }
        }

        let _ = fs::remove_file(&self.config.control_socket);

        eprintln!("vigil-scan: shutdown complete");
        Ok(())
    }
}

/// Build a NUL-terminated C string, returning a clean error (instead of
/// panicking / aborting) when the input contains an embedded NUL byte.
fn to_cstring(value: &str, what: &str) -> Result<CString> {
    CString::new(value).with_context(|| format!("{} contains a NUL byte", what))
}

fn read_log_lines(path: &Path, max_lines: usize) -> Result<Vec<String>> {
    use std::io::{Read, Seek, SeekFrom};

    let file =
        fs::File::open(path).with_context(|| format!("log file not found: {}", path.display()))?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);

    if size == 0 {
        return Ok(Vec::new());
    }

    let mut window: u64 = 0;
    loop {
        window = window.saturating_mul(4).max(16 * 1024).min(size);
        let from = size - window;

        let mut f = file.try_clone()?;
        f.seek(SeekFrom::Start(from))?;
        let mut buf = vec![0u8; window as usize];
        f.read_exact(&mut buf)?;

        let raw = String::from_utf8_lossy(&buf);
        let mut parts: Vec<&str> = raw.split('\n').collect();
        if raw.ends_with('\n') {
            parts.pop();
        }
        if from > 0 {
            parts.remove(0);
        }

        if parts.len() >= max_lines || from == 0 {
            let take = max_lines.min(parts.len());
            let start = parts.len() - take;
            return Ok(parts[start..].iter().map(|s| s.to_string()).collect());
        }
    }
}

/// Service names are used to build filesystem paths (supervise/status/log
/// dirs) and cgroup names, so they must be restricted to a safe identifier
/// set. Rejecting anything else prevents path traversal / directory escapes
/// via a maliciously-named service file.
fn is_valid_service_name(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." {
        return false;
    }
    name.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::is_valid_service_name;

    #[test]
    fn valid_names_accepted() {
        for n in ["sshd", "foo-bar_baz", "network.eth0", "a1"] {
            assert!(is_valid_service_name(n), "{} should be valid", n);
        }
    }

    #[test]
    fn invalid_names_rejected() {
        for n in ["", ".", "..", "a/b", "../etc", "a b", "a\tb", "a\nb", "a:b"] {
            assert!(!is_valid_service_name(n), "{} should be rejected", n);
        }
    }
}

use std::ffi::CString;
use std::fs;
use std::path::Path;

fn main() {
    let force = std::env::args().any(|a| a == "--force");
    let pid = unsafe { libc::getpid() };
    if !force && pid != 1 {
        eprintln!("vigil: must be run as PID 1 (use --force for testing)");
        std::process::exit(1);
    }

    eprintln!("vigil: PID 1 starting");

    setup_console();
    mount_filesystems();
    set_hostname();
    setup_runtime();

    unsafe {
        libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
    }

    run_init_loop();
}

fn setup_console() {
    let dev_console = CString::new("/dev/console").unwrap();
    let fd = unsafe {
        libc::open(
            dev_console.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY,
        )
    };
    if fd >= 0 {
        unsafe {
            libc::dup2(fd, 0);
            libc::dup2(fd, 1);
            libc::dup2(fd, 2);
            libc::close(fd);
        }
    } else {
        let dev_null = CString::new("/dev/null").unwrap();
        let fd = unsafe {
            libc::open(
                dev_null.as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY,
            )
        };
        if fd >= 0 {
            unsafe {
                libc::dup2(fd, 0);
                libc::dup2(fd, 1);
                libc::dup2(fd, 2);
                libc::close(fd);
            }
        }
    }
}

fn mount_filesystems() {
    use nix::mount::{mount, MsFlags};

    let mounts: &[(&str, &str, &str, MsFlags, Option<&str>)] = &[
        (
            "proc",
            "/proc",
            "proc",
            MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV,
            None,
        ),
        (
            "sysfs",
            "/sys",
            "sysfs",
            MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV,
            None,
        ),
        (
            "devtmpfs",
            "/dev",
            "devtmpfs",
            MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
            None,
        ),
        (
            "tmpfs",
            "/run",
            "tmpfs",
            MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV,
            Some("mode=0755"),
        ),
    ];

    for &(source, target, fstype, flags, data) in mounts {
        if !Path::new(target).exists() {
            let _ = fs::create_dir_all(target);
        }

        let source_c = CString::new(source).unwrap();
        let target_c = CString::new(target).unwrap();
        let fstype_c = CString::new(fstype).unwrap();
        let data_c = data.map(|d| CString::new(d).unwrap());

        let result = match data_c {
            Some(ref d) => mount(
                Some(source_c.as_c_str()),
                target_c.as_c_str(),
                Some(fstype_c.as_c_str()),
                flags,
                Some(d.as_c_str()),
            ),
            None => mount(
                Some(source_c.as_c_str()),
                target_c.as_c_str(),
                Some(fstype_c.as_c_str()),
                flags,
                None::<&std::ffi::CStr>,
            ),
        };
        match result {
            Ok(()) => eprintln!("vigil: mounted {}", target),
            Err(e) => eprintln!("vigil: mount {} failed (may already be mounted): {}", target, e),
        }
    }
}

fn set_hostname() {
    let config_path = "/etc/vigil/vigil.toml";
    if let Ok(content) = fs::read_to_string(config_path) {
        if let Ok(config) = toml::from_str::<vigil::config::GlobalConfig>(&content) {
            if let Some(ref hostname) = config.hostname {
                if let Ok(c_hostname) = CString::new(hostname.as_str()) {
                    unsafe {
                        libc::sethostname(c_hostname.as_ptr(), hostname.len());
                    }
                    eprintln!("vigil: hostname set to {}", hostname);
                }
            }
        }
    }
}

fn setup_runtime() {
    for dir in &[
        "/run/vigil",
        "/run/vigil/supervise",
        "/var/log/vigil",
    ] {
        let _ = fs::create_dir_all(dir);
    }
}

fn run_init_loop() {
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};

    let mut sigset: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut sigset);
        libc::sigaddset(&mut sigset, libc::SIGCHLD);
        libc::sigaddset(&mut sigset, libc::SIGTERM);
        libc::sigaddset(&mut sigset, libc::SIGINT);
        libc::sigaddset(&mut sigset, libc::SIGUSR1);
        libc::sigaddset(&mut sigset, libc::SIGUSR2);
        libc::sigaddset(&mut sigset, libc::SIGPWR);
        libc::sigprocmask(
            libc::SIG_BLOCK,
            &sigset,
            std::ptr::null_mut(),
        );
    }

    let mut scanner_pid: Option<nix::unistd::Pid>;
    let mut shutdown_requested = false;

    scanner_pid = Some(spawn_scanner());
    eprintln!("vigil: entering init loop");

    while !shutdown_requested {
        let mut signo: libc::c_int = 0;
        unsafe {
            libc::sigwait(&sigset, &mut signo);
        }

        match signo {
            libc::SIGCHLD => {
                reap_children();
                if let Some(pid) = scanner_pid {
                    match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
                        Ok(WaitStatus::StillAlive) => {}
                        Ok(status) => {
                            eprintln!("vigil: scanner exited with {:?}", status);
                            if !shutdown_requested {
                                eprintln!("vigil: restarting scanner (second-chance)");
                                kill_all_children();
                                scanner_pid = Some(spawn_scanner());
                            }
                        }
                        Err(nix::errno::Errno::ECHILD) => {
                            if !shutdown_requested {
                                eprintln!("vigil: scanner gone, restarting");
                                scanner_pid = Some(spawn_scanner());
                            }
                        }
                        Err(_) => {}
                    }
                }
            }
            libc::SIGTERM | libc::SIGINT => {
                eprintln!("vigil: shutdown signal received");
                shutdown_requested = true;
                stop_scanner(scanner_pid);
                exec_shutdown("reboot");
            }
            libc::SIGUSR1 => {
                eprintln!("vigil: halt requested (SIGUSR1)");
                shutdown_requested = true;
                stop_scanner(scanner_pid);
                exec_shutdown("halt");
            }
            libc::SIGUSR2 => {
                eprintln!("vigil: poweroff requested (SIGUSR2)");
                shutdown_requested = true;
                stop_scanner(scanner_pid);
                exec_shutdown("poweroff");
            }
            libc::SIGPWR => {
                eprintln!("vigil: power failure (SIGPWR)");
                shutdown_requested = true;
                stop_scanner(scanner_pid);
                exec_shutdown("poweroff");
            }
            _ => {}
        }
    }
}

fn spawn_scanner() -> nix::unistd::Pid {
    use nix::unistd::{execvp, fork, ForkResult};

    match unsafe { fork() } {
        Ok(ForkResult::Parent { child }) => {
            eprintln!("vigil: scanner spawned as PID {}", child);
            child
        }
        Ok(ForkResult::Child) => {
            let exe_path = std::env::current_exe().ok();
            let scan_name = CString::new("vigil-scan").unwrap();

            let search_paths = if let Some(ref exe) = exe_path {
                if let Some(dir) = exe.parent() {
                    let mut paths: Vec<String> = vec![dir.join("vigil-scan").to_string_lossy().to_string()];
                    paths.extend(
                        ["/usr/local/bin", "/usr/bin", "/bin"]
                            .iter()
                            .map(|p| format!("{}/vigil-scan", p)),
                    );
                    paths
                } else {
                    vec![
                        "/usr/local/bin/vigil-scan".into(),
                        "/usr/bin/vigil-scan".into(),
                        "/bin/vigil-scan".into(),
                    ]
                }
            } else {
                vec![
                    "/usr/local/bin/vigil-scan".into(),
                    "/usr/bin/vigil-scan".into(),
                    "/bin/vigil-scan".into(),
                ]
            };

            for path in &search_paths {
                if let Ok(c_path) = CString::new(path.as_str()) {
                    if execvp(&c_path, std::slice::from_ref(&scan_name)).is_ok() {
                        unreachable!();
                    }
                }
            }

            eprintln!("vigil: failed to exec vigil-scan");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("vigil: fork failed: {}", e);
            std::process::exit(1);
        }
    }
}

fn stop_scanner(pid: Option<nix::unistd::Pid>) {
    use nix::sys::signal::{kill, Signal};
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};

    if let Some(pid) = pid {
        let _ = kill(pid, Signal::SIGTERM);
        for _ in 0..50 {
            match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) => {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                _ => break,
            }
        }
        let _ = kill(pid, Signal::SIGKILL);
        let _ = waitpid(pid, None);
    }
}

fn reap_children() {
    use nix::sys::wait::{waitpid, WaitPidFlag};

    loop {
        match waitpid(nix::unistd::Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(nix::sys::wait::WaitStatus::StillAlive) => break,
            Ok(_) => {}
            Err(nix::errno::Errno::ECHILD) => break,
            Err(_) => break,
        }
    }
}

fn kill_all_children() {
    let our_pid = unsafe { libc::getpid() };

    for _ in 0..20 {
        let children = get_children_of(our_pid);
        if children.is_empty() {
            break;
        }
        for pid in &children {
            unsafe {
                libc::kill(*pid, libc::SIGTERM);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    std::thread::sleep(std::time::Duration::from_secs(1));

    let children = get_children_of(our_pid);
    for pid in &children {
        unsafe {
            libc::kill(*pid, libc::SIGKILL);
        }
    }

    reap_children();
}

fn get_children_of(ppid: i32) -> Vec<i32> {
    let mut children = Vec::new();
    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if let Ok(pid) = name.parse::<i32>() {
                    let status_path = format!("/proc/{}/status", pid);
                    if let Ok(status) = fs::read_to_string(&status_path) {
                        for line in status.lines() {
                            if let Some(rest) = line.strip_prefix("PPid:") {
                                if let Ok(p) = rest.trim().parse::<i32>() {
                                    if p == ppid {
                                        children.push(pid);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    children
}

fn exec_shutdown(action: &str) -> ! {
    use nix::unistd::execvp;

    let reboot_flag = match action {
        "halt" => libc::RB_HALT_SYSTEM,
        "poweroff" => libc::RB_POWER_OFF,
        _ => libc::RB_AUTOBOOT,
    };

    for cmd in &["/sbin/shutdown", "/usr/sbin/shutdown"] {
        if let Ok(c_path) = CString::new(*cmd) {
            let c_action = CString::new(match action {
                "halt" => "-H",
                "poweroff" => "-P",
                _ => "-r",
            })
            .unwrap();
            let c_now = CString::new("now").unwrap();
            if execvp(&c_path, &[c_path.clone(), c_action, c_now]).is_ok() {
                unreachable!();
            }
        }
    }

    for cmd in &["/sbin/reboot", "/usr/sbin/reboot"] {
        if let Ok(c_path) = CString::new(*cmd) {
            if execvp(&c_path, std::slice::from_ref(&c_path)).is_ok() {
                unreachable!();
            }
        }
    }

    eprintln!("vigil: executing reboot(2) syscall");
    unsafe {
        libc::reboot(reboot_flag);
    }

    std::process::exit(0);
}

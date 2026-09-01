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
    let fd = unsafe { libc::open(dev_console.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if fd >= 0 {
        unsafe {
            libc::dup2(fd, 0);
            libc::dup2(fd, 1);
            libc::dup2(fd, 2);
            // If open() returned an fd that collided with 0/1/2 (they were
            // already closed), dup2 is a no-op for that slot and closing fd
            // would wrongly close the very stream we just wired up.
            if fd > 2 {
                libc::close(fd);
            }
        }
    } else {
        let dev_null = CString::new("/dev/null").unwrap();
        let fd = unsafe { libc::open(dev_null.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
        if fd >= 0 {
            unsafe {
                libc::dup2(fd, 0);
                libc::dup2(fd, 1);
                libc::dup2(fd, 2);
                if fd > 2 {
                    libc::close(fd);
                }
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
        (
            "tmpfs",
            "/dev/shm",
            "tmpfs",
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
            Some("mode=1777"),
        ),
        (
            "devpts",
            "/dev/pts",
            "devpts",
            MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
            Some("gid=5,mode=620,ptmxmode=0666"),
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
            Err(e) => eprintln!(
                "vigil: mount {} failed (may already be mounted): {}",
                target, e
            ),
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
        vigil::VIGIL_RUNTIME_DIR,
        vigil::VIGIL_SUPERVISE_DIR,
        vigil::VIGIL_LOG_DIR,
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
        libc::sigprocmask(libc::SIG_BLOCK, &sigset, std::ptr::null_mut());
    }

    let mut scanner_pid: Option<nix::unistd::Pid>;

    scanner_pid = Some(spawn_scanner());
    eprintln!("vigil: entering init loop");

    loop {
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
                            eprintln!("vigil: restarting scanner (second-chance)");
                            kill_all_children();
                            scanner_pid = Some(spawn_scanner());
                        }
                        Err(nix::errno::Errno::ECHILD) => {
                            // The scanner was already reaped above (or its
                            // exit status arrived after the generic reap): it
                            // is gone regardless. Without this branch the old
                            // supervisors it left behind would keep running
                            // under PID 1 while a fresh scanner starts a
                            // second copy of every service.
                            eprintln!("vigil: scanner gone, restarting");
                            kill_all_children();
                            scanner_pid = Some(spawn_scanner());
                        }
                        Err(_) => {}
                    }
                }
            }
            libc::SIGTERM | libc::SIGINT => {
                eprintln!("vigil: shutdown signal received");
                stop_scanner(scanner_pid);
                exec_shutdown("reboot");
            }
            libc::SIGUSR1 => {
                eprintln!("vigil: halt requested (SIGUSR1)");
                stop_scanner(scanner_pid);
                exec_shutdown("halt");
            }
            libc::SIGUSR2 => {
                eprintln!("vigil: poweroff requested (SIGUSR2)");
                stop_scanner(scanner_pid);
                exec_shutdown("poweroff");
            }
            libc::SIGPWR => {
                eprintln!("vigil: power failure (SIGPWR)");
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
            let scan_name = CString::new("vigil-scan").unwrap();
            let args = std::slice::from_ref(&scan_name);

            for path in vigil::util::exec_search_paths("vigil-scan") {
                if let Ok(c_path) = CString::new(path.to_string_lossy().as_bytes()) {
                    let _ = execvp(&c_path, args);
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

    // The init loop runs with a blocked signal set (SIGCHLD/SIGTERM/…).
    // `execvp` preserves that mask in the new image, which would leave
    // `/sbin/shutdown` unable to catch the very signals it needs (e.g.
    // SIGTERM on timeout, SIGCHLD to reap children). Reset to the default
    // empty mask before chain-loading so the shutdown program behaves
    // normally.
    let mut empty: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut empty);
        libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
    }

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

    // Direct action binaries. A plain bare `/sbin/reboot` would always reboot,
    // silently turning a halt/poweroff request into a reboot, so try the
    // action-specific binary first and pass the util-linux flag to reboot(8)
    // where the two are the same program.
    let direct_bin = match action {
        "halt" => "halt",
        "poweroff" => "poweroff",
        _ => "reboot",
    };
    for cmd in &["/sbin", "/usr/sbin"] {
        if let Ok(c_path) = CString::new(format!("{}/{}", cmd, direct_bin)) {
            let argv: Vec<CString> = if direct_bin == "reboot" && action != "reboot" {
                let c_flag = CString::new(if action == "halt" { "-h" } else { "-p" }).unwrap();
                vec![c_path.clone(), c_flag]
            } else {
                vec![c_path.clone()]
            };
            if execvp(&c_path, &argv).is_ok() {
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

use anyhow::{Context, Result};
use nix::sys::resource::{setrlimit, Resource};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{chdir, execvp, fork, ForkResult, Gid, Uid};
use std::env;
use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::time::{Duration, Instant};

use vigil::config::ServiceConfig;

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
    let log_dir = args.get(3).map(|s| s.as_str()).unwrap_or("/var/log/vigil");

    let config_content = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read config: {}", config_path))?;
    let config: ServiceConfig =
        toml::from_str(&config_content).with_context(|| "failed to parse config")?;

    let supervise_dir = PathBuf::from(format!(
        "/run/vigil/supervise/{}",
        service_name
    ));
    fs::create_dir_all(&supervise_dir)?;

    let status_dir = supervise_dir.join("status");
    fs::create_dir_all(&status_dir)?;

    eprintln!(
        "vigil-supervise [{}]: starting service supervisor",
        service_name
    );

    let mut restart_count: u32 = 0;
    let mut current_backoff = config.service.restart.backoff_initial_ms;
    let mut consecutive_success_time: Option<Instant> = None;

    loop {
        let now = Instant::now();

        let log_path = PathBuf::from(log_dir).join(service_name);
        fs::create_dir_all(&log_path)?;

        let (mut stdout_file, mut stderr_file) = match config.logging.kind {
            vigil::config::LogType::None => (None, None),
            _ => {
                let current = log_path.join("current");
                let f = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&current)?;
                let f2 = f.try_clone()?;
                (Some(f), Some(f2))
            }
        };

        write_state(&status_dir, "starting", None)?;

        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                eprintln!(
                    "vigil-supervise [{}]: service forked as PID {}",
                    service_name, child
                );
                write_state(&status_dir, "running", Some(child.as_raw() as u32))?;
                write_pid(&status_dir, child.as_raw() as u32)?;

                let status = waitpid(child, None);

                match status {
                    Ok(WaitStatus::Exited(_, code)) => {
                        eprintln!(
                            "vigil-supervise [{}]: service exited with code {}",
                            service_name, code
                        );
                        write_state(&status_dir, "stopped", None)?;

                        if consecutive_success_time.is_none() {
                            consecutive_success_time = Some(now);
                        }

                        match config.service.restart.policy {
                            vigil::config::RestartPolicy::Never => {
                                eprintln!(
                                    "vigil-supervise [{}]: restart policy is 'never', giving up",
                                    service_name
                                );
                                write_state(&status_dir, "failed", None)?;
                                break;
                            }
                            vigil::config::RestartPolicy::OnFailure => {
                                if code == 0 {
                                    eprintln!(
                                        "vigil-supervise [{}]: exited cleanly, not restarting",
                                        service_name
                                    );
                                    break;
                                }
                            }
                            vigil::config::RestartPolicy::OnAbnormal => {
                                if code == 0 || code == 1 {
                                    break;
                                }
                            }
                            vigil::config::RestartPolicy::Always => {}
                        }

                        if restart_count >= config.service.restart.max_restarts {
                            eprintln!(
                                "vigil-supervise [{}]: max restarts ({}) reached",
                                service_name, config.service.restart.max_restarts
                            );
                            write_state(&status_dir, "failed", None)?;
                            break;
                        }
                    }
                    Ok(WaitStatus::Signaled(_, signal, _)) => {
                        eprintln!(
                            "vigil-supervise [{}]: service killed by signal {:?}",
                            service_name, signal
                        );
                        write_state(&status_dir, "stopped", None)?;

                        match config.service.restart.policy {
                            vigil::config::RestartPolicy::Never => {
                                write_state(&status_dir, "failed", None)?;
                                break;
                            }
                            vigil::config::RestartPolicy::OnFailure
                            | vigil::config::RestartPolicy::OnAbnormal => {
                                break;
                            }
                            vigil::config::RestartPolicy::Always => {}
                        }

                        if restart_count >= config.service.restart.max_restarts {
                            eprintln!(
                                "vigil-supervise [{}]: max restarts reached after signal kill",
                                service_name
                            );
                            write_state(&status_dir, "failed", None)?;
                            break;
                        }
                    }
                    Ok(WaitStatus::Stopped(_, signal)) => {
                        eprintln!(
                            "vigil-supervise [{}]: service stopped by signal {:?}",
                            service_name, signal
                        );
                        write_state(&status_dir, "stopped", None)?;
                    }
                    Err(e) => {
                        eprintln!(
                            "vigil-supervise [{}]: waitpid error: {}",
                            service_name, e
                        );
                        write_state(&status_dir, "failed", None)?;
                        break;
                    }
                    _ => {}
                }

                run_finish_script(service_name, &config, &log_path);

                restart_count += 1;
                eprintln!(
                    "vigil-supervise [{}]: restarting in {}ms (attempt {}/{})",
                    service_name,
                    current_backoff,
                    restart_count,
                    config.service.restart.max_restarts
                );
                write_state(&status_dir, "restarting", None)?;
                std::thread::sleep(Duration::from_millis(current_backoff));
                current_backoff = (current_backoff as f64
                    * config.service.restart.backoff_multiplier) as u64;
                if current_backoff > config.service.restart.backoff_max_ms {
                    current_backoff = config.service.restart.backoff_max_ms;
                }
            }
            Ok(ForkResult::Child) => {
                prepare_service_environment(&config)?;

                if let Some(ref work_dir) = config.service.working_dir {
                    chdir(work_dir)?;
                }

                if let Some(ref mut stdout_f) = stdout_file {
                    use nix::unistd::dup2;
                    use std::os::unix::io::AsRawFd;
                    let _ = dup2(stdout_f.as_raw_fd(), 1);
                } else {
                    let dev_null = CString::new("/dev/null").unwrap();
                    let fd = unsafe {
                        libc::open(
                            dev_null.as_ptr(),
                            libc::O_WRONLY,
                        )
                    };
                    if fd >= 0 {
                        unsafe {
                            libc::dup2(fd, 1);
                            libc::close(fd);
                        }
                    }
                }

                if let Some(ref mut stderr_f) = stderr_file {
                    use nix::unistd::dup2;
                    use std::os::unix::io::AsRawFd;
                    let _ = dup2(stderr_f.as_raw_fd(), 2);
                } else {
                    let dev_null = CString::new("/dev/null").unwrap();
                    let fd = unsafe {
                        libc::open(
                            dev_null.as_ptr(),
                            libc::O_WRONLY,
                        )
                    };
                    if fd >= 0 {
                        unsafe {
                            libc::dup2(fd, 2);
                            libc::close(fd);
                        }
                    }
                }

                let null_stdin = CString::new("/dev/null").unwrap();
                let fd = unsafe {
                    libc::open(
                        null_stdin.as_ptr(),
                        libc::O_RDONLY,
                    )
                };
                if fd >= 0 {
                    unsafe {
                        libc::dup2(fd, 0);
                        libc::close(fd);
                    }
                }

                for (key, value) in &config.service.environment {
                    env::set_var(key, value);
                }

                let cmd = CString::new(config.service.command.clone())
                    .context("invalid command string")?;
                let mut argv: Vec<CString> = vec![cmd.clone()];
                for arg in &config.service.args {
                    argv.push(
                        CString::new(arg.as_str())
                            .context("invalid argument string")?,
                    );
                }

                let argv_refs: Vec<&CString> = argv.iter().collect();
                let _ = execvp(&cmd, &argv_refs);

                eprintln!(
                    "vigil-supervise [{}]: exec failed: {}",
                    service_name, config.service.command
                );
                exit(127);
            }
            Err(e) => {
                eprintln!(
                    "vigil-supervise [{}]: fork failed: {}",
                    service_name, e
                );
                exit(1);
            }
        }
    }

    Ok(())
}

fn prepare_service_environment(config: &ServiceConfig) -> Result<()> {
    if config.service.user != "root" {
        if let Some(user) =
            users::get_user_by_name(&config.service.user)
        {
            let uid = Uid::from_raw(user.uid());
            nix::unistd::setuid(uid)?;
        }
    }

    if config.service.group != "root" {
        if let Some(group) =
            users::get_group_by_name(&config.service.group)
        {
            let gid = Gid::from_raw(group.gid());
            nix::unistd::setgid(gid)?;
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

fn write_state(
    status_dir: &Path,
    state: &str,
    pid: Option<u32>,
) -> Result<()> {
    let state_path = status_dir.join("state");
    let mut f = fs::File::create(&state_path)?;
    writeln!(f, "{}", state)?;

    if let Some(pid) = pid {
        let pid_path = status_dir.join("pid");
        fs::write(&pid_path, pid.to_string())?;
    }

    Ok(())
}

fn write_pid(status_dir: &Path, pid: u32) -> Result<()> {
    let pid_path = status_dir.join("pid");
    fs::write(&pid_path, pid.to_string())?;
    Ok(())
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
            .output();
    }
}

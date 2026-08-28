use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use vigil::protocol::{self, Request, Response, ShutdownAction};

#[derive(Parser)]
#[command(name = "vigil-ctl", about = "Vigil init system control tool", version)]
struct Cli {
    #[arg(
        short,
        long,
        default_value = "/run/vigil/control.sock",
        env = "VIGIL_SOCK"
    )]
    socket: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Ping,
    Status {
        #[arg()]
        service: Option<String>,
    },
    List,
    Start {
        #[arg()]
        service: String,
    },
    Stop {
        #[arg()]
        service: String,
    },
    Restart {
        #[arg()]
        service: String,
    },
    Log {
        #[arg()]
        service: String,
        #[arg(short = 'n', long, default_value = "50")]
        lines: usize,
    },
    Reload,
    Poweroff,
    Reboot,
    Halt,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let request = match &cli.command {
        Command::Ping => Request::Ping,
        Command::Status { service } => Request::Status {
            service: service.clone(),
        },
        Command::List => Request::List,
        Command::Start { service } => Request::Start {
            service: service.clone(),
        },
        Command::Stop { service } => Request::Stop {
            service: service.clone(),
        },
        Command::Restart { service } => Request::Restart {
            service: service.clone(),
        },
        Command::Log { service, lines } => Request::Log {
            service: service.clone(),
            lines: *lines,
        },
        Command::Reload => Request::Reload,
        Command::Poweroff => Request::Shutdown {
            action: ShutdownAction::Poweroff,
        },
        Command::Reboot => Request::Shutdown {
            action: ShutdownAction::Reboot,
        },
        Command::Halt => Request::Shutdown {
            action: ShutdownAction::Halt,
        },
    };

    let response = send_request(&cli.socket, &request)?;

    match response {
        Response::Pong => {
            println!("pong");
        }
        Response::Ok { message } => {
            println!("{}", message);
        }
        Response::Error { message } => {
            eprintln!("error: {}", message);
            std::process::exit(1);
        }
        Response::Status(status) => {
            println!("Service: {}", status.name);
            println!("  State:       {}", status.state);
            println!(
                "  PID:         {}",
                status.pid.map_or("n/a".into(), |p| p.to_string())
            );
            println!("  Uptime:      {}s", status.uptime_secs);
            println!("  Restarts:    {}", status.restart_count);
            println!("  Description: {}", status.description);
            println!("  Command:     {}", status.command);
        }
        Response::List(services) => {
            if services.is_empty() {
                println!("No services loaded.");
            } else {
                println!("{:<20} {:<12} {:<8} DESCRIPTION", "NAME", "STATE", "PID");
                println!("{}", "-".repeat(60));
                for svc in &services {
                    println!(
                        "{:<20} {:<12} {:<8} {}",
                        svc.name,
                        svc.state,
                        svc.pid.map_or("n/a".into(), |p| p.to_string()),
                        svc.description
                    );
                }
            }
        }
        Response::LogLines(lines) => {
            for line in &lines {
                println!("{}", line);
            }
        }
    }

    Ok(())
}

fn send_request(socket_path: &PathBuf, request: &Request) -> Result<Response> {
    let mut stream = UnixStream::connect(socket_path).with_context(|| {
        format!(
            "failed to connect to vigil at {}\nIs vigil-scan running?",
            socket_path.display()
        )
    })?;

    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .ok();
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(10)))
        .ok();

    protocol::write_message(&mut stream, request).context("failed to send request")?;

    let response: Response =
        protocol::read_message(&mut stream).context("failed to read response")?;

    Ok(response)
}

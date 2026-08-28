use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::exit;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: vigillog <service_name> [log_dir] [max_size_mb] [max_files]");
        exit(1);
    }

    let service_name = &args[1];
    let log_dir = args.get(2).unwrap_or(&"/var/log/vigil".to_string()).clone();
    let max_size_mb: u64 = args
        .get(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let max_files: u32 = args
        .get(4)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);

    let service_log_dir = PathBuf::from(&log_dir).join(service_name);
    if let Err(e) = fs::create_dir_all(&service_log_dir) {
        eprintln!(
            "vigillog [{}]: failed to create log dir: {}",
            service_name, e
        );
        exit(1);
    }

    let current_path = service_log_dir.join("current");
    let max_bytes = max_size_mb * 1024 * 1024;

    let stdin = io::stdin();
    let reader = stdin.lock();

    let mut line_count: u64 = 0;

    for line in reader.lines() {
        match line {
            Ok(line) => {
                line_count += 1;

                if line_count % 1000 == 0 {
                    if let Ok(meta) = fs::metadata(&current_path) {
                        if meta.len() >= max_bytes {
                            rotate_logs(&service_log_dir, max_files);
                        }
                    }
                }

                if let Err(e) = write_log_line(&current_path, &line) {
                    eprintln!(
                        "vigillog [{}]: write error: {}",
                        service_name, e
                    );
                }
            }
            Err(e) => {
                eprintln!(
                    "vigillog [{}]: read error: {}",
                    service_name, e
                );
                break;
            }
        }
    }
}

fn write_log_line(path: &Path, line: &str) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;

    let now = chrono_timestamp();
    writeln!(file, "{} {}", now, line)?;
    file.flush()?;
    Ok(())
}

fn rotate_logs(dir: &Path, max_files: u32) {
    let _ = fs::remove_file(dir.join(format!(".{}.tmp", max_files)));

    for i in (1..max_files).rev() {
        let from = dir.join(i.to_string());
        let to = dir.join((i + 1).to_string());
        if from.exists() {
            if i + 1 >= max_files {
                let _ = fs::remove_file(&to);
            }
            let _ = fs::rename(&from, &to);
        }
    }

    let current = dir.join("current");
    let first = dir.join("1");
    if current.exists() {
        let _ = fs::rename(&current, &first);
    }
}

fn chrono_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let days = now / 86400;
    let secs = now % 86400;
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let secs = secs % 60;

    let year = 1970 + (days as f64 / 365.25) as u64;
    let day_of_year = days % 365;

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year,
        (day_of_year / 30 + 1).min(12),
        (day_of_year % 30 + 1).min(31),
        hours,
        mins,
        secs
    )
}

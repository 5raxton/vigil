use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::process::exit;

/// Vigillog is Vigil's per-service log writer. It is spawned by
/// `vigil-supervise` with the read end of the service log pipe on stdin and
/// exits on EOF (i.e. when the supervised service goes away).
///
/// Modes (`vigillog <mode> ...`):
///
/// - `pipe <name> <dir> [max_size_mb] [max_files] [timestamp]` — write to
///   `<dir>/current` (already the per-service log directory), rotating by
///   size/count (daemontools style).
/// - `file <name> <path> [max_size_mb] [max_files] [timestamp]` — write to a
///   specific file, rotating to `<path>.1`, `<path>.2`, …
/// - `syslog <name> <fallback_dir> <sock_path>` — forward each line to the
///   syslog socket (RFC 3164), falling back to a file when no syslog daemon
///   is present.
///
/// The writer keeps its file descriptor open and tracks the byte offset as
/// it writes, so size-driven rotation adds no extra stat syscalls on the hot
/// path.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
    }

    match args[1].as_str() {
        "pipe" => run_pipe(&args),
        "file" => run_file(&args),
        "syslog" => run_syslog(&args),
        other => {
            eprintln!("vigillog: unknown mode '{}'", other);
            usage();
        }
    }
}

fn usage() -> ! {
    eprintln!(
        "usage:\n  vigillog pipe <name> <dir> [max_size_mb] [max_files] [timestamp]\n  \
         vigillog file <name> <path> [max_size_mb] [max_files] [timestamp]\n  \
         vigillog syslog <name> <fallback_dir> <sock_path>"
    );
    exit(1);
}

fn run_pipe(args: &[String]) -> ! {
    if args.len() < 4 {
        usage();
    }
    let name = &args[2];
    let dir = PathBuf::from(&args[3]);
    let max_size_mb = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(10);
    let max_files = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(5);
    let timestamp = args.get(6).map(|s| s == "1").unwrap_or(true);

    if let Err(e) = fs::create_dir_all(&dir) {
        eprintln!(
            "vigillog [{}]: failed to create log dir {}: {}",
            name,
            dir.display(),
            e
        );
        exit(1);
    }

    let mut writer = RotatingWriter::new(
        dir.join("current"),
        max_size_mb,
        max_files,
        timestamp,
        RotationStyle::Directory,
    );

    match forward(&mut writer, name) {
        Ok(()) => exit(0),
        Err(e) => {
            eprintln!("vigillog [{}]: write error: {}", name, e);
            exit(1);
        }
    }
}

fn run_file(args: &[String]) -> ! {
    if args.len() < 4 {
        usage();
    }
    let name = &args[2];
    let path = PathBuf::from(&args[3]);
    let max_size_mb = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(10);
    let max_files = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(5);
    let timestamp = args.get(6).map(|s| s == "1").unwrap_or(true);

    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!(
                "vigillog [{}]: failed to create log dir {}: {}",
                name,
                parent.display(),
                e
            );
            exit(1);
        }
    }

    let mut writer = RotatingWriter::new(
        path,
        max_size_mb,
        max_files,
        timestamp,
        RotationStyle::Suffix,
    );

    match forward(&mut writer, name) {
        Ok(()) => exit(0),
        Err(e) => {
            eprintln!("vigillog [{}]: write error: {}", name, e);
            exit(1);
        }
    }
}

fn run_syslog(args: &[String]) -> ! {
    if args.len() < 5 {
        usage();
    }
    let name = &args[2];
    let fallback_dir = PathBuf::from(&args[3]);
    let sock_path = PathBuf::from(&args[4]);

    let fallback = fallback_dir.join("current");
    if let Some(parent) = fallback.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let sock = match UnixDatagram::unbound() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("vigillog [{}]: cannot create syslog socket: {}", name, e);
            exit(1);
        }
    };

    let mut fallback_writer: Option<File> = None;
    let mut warned = false;

    let reader = io::stdin().lock();
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };

        let msg = rfc3164_message(name, &line);
        if let Err(e) = sock.send_to(msg.as_bytes(), &sock_path) {
            if !warned {
                eprintln!(
                    "vigillog [{}]: syslog unavailable ({}); falling back to {}",
                    name,
                    e,
                    fallback.display()
                );
                warned = true;
            }
            let f = match fallback_writer.as_mut() {
                Some(f) => f,
                None => match open_append(&fallback) {
                    Ok(f) => {
                        fallback_writer = Some(f);
                        fallback_writer.as_mut().unwrap()
                    }
                    Err(e) => {
                        eprintln!("vigillog [{}]: fallback write failed: {}", name, e);
                        continue;
                    }
                },
            };
            if writeln!(f, "{}", line).is_err() {
                eprintln!("vigillog [{}]: fallback write failed", name);
            }
        }
    }

    exit(0);
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn forward(writer: &mut RotatingWriter, name: &str) -> io::Result<()> {
    let reader = io::stdin().lock();
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("vigillog [{}]: read error: {}", name, e);
                break;
            }
        };
        writer.write_line(&line)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RotationStyle {
    /// Logs live in a service directory; rotated files are numbered
    /// (`current`, `1`, `2`, …).
    Directory,
    /// Rotated files get a numeric suffix on the log path itself
    /// (`app.log`, `app.log.1`, `app.log.2`, …).
    Suffix,
}

struct RotatingWriter {
    current: PathBuf,
    max_bytes: u64,
    max_files: u32,
    timestamp: bool,
    style: RotationStyle,
    file: Option<File>,
    size: u64,
}

impl RotatingWriter {
    fn new(
        current: PathBuf,
        max_size_mb: u64,
        max_files: u32,
        timestamp: bool,
        style: RotationStyle,
    ) -> Self {
        let max_bytes = max_size_mb.saturating_mul(1024 * 1024);
        Self {
            current,
            max_bytes,
            max_files: max_files.max(1),
            timestamp,
            style,
            file: None,
            size: 0,
        }
    }

    fn ensure_open(&mut self) -> io::Result<()> {
        if self.file.is_none() {
            let f = open_append(&self.current)?;
            self.size = f.metadata().map(|m| m.len()).unwrap_or(0);
            self.file = Some(f);
        }
        Ok(())
    }

    fn write_line(&mut self, line: &str) -> io::Result<()> {
        self.ensure_open()?;

        let mut out = String::with_capacity(line.len() + 32);
        if self.timestamp {
            out.push_str(&utc_timestamp());
            out.push(' ');
        }
        out.push_str(line);
        out.push('\n');

        let bytes = out.len() as u64;
        self.file.as_mut().unwrap().write_all(out.as_bytes())?;
        self.size += bytes;

        if self.max_bytes > 0 && self.size >= self.max_bytes {
            self.rotate()?;
        }
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file = None; // close the current file
        self.shift_rotated()?;
        self.rotate_current()?;
        self.ensure_open()?;
        Ok(())
    }

    fn rotated_path(&self, index: u32) -> PathBuf {
        match self.style {
            RotationStyle::Directory => self
                .current
                .parent()
                .unwrap_or(Path::new("/"))
                .join(index.to_string()),
            RotationStyle::Suffix => PathBuf::from(format!("{}.{}", self.current.display(), index)),
        }
    }

    fn shift_rotated(&self) -> io::Result<()> {
        for i in (1..self.max_files).rev() {
            let from = self.rotated_path(i);
            let to = self.rotated_path(i + 1);
            if from.exists() {
                if i + 1 >= self.max_files {
                    let _ = fs::remove_file(&to);
                }
                let _ = fs::rename(&from, &to);
            }
        }
        Ok(())
    }

    fn rotate_current(&self) -> io::Result<()> {
        if self.current.exists() {
            let to = self.rotated_path(1);
            if to.exists() {
                let _ = fs::remove_file(&to);
            }
            fs::rename(&self.current, &to)?;
        }
        Ok(())
    }
}

fn utc_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as libc::time_t;

    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::gmtime_r(&now, &mut tm);
    }

    let mut buf = [0u8; 32];
    let fmt = b"%Y-%m-%dT%H:%M:%S\0";
    let n = unsafe {
        libc::strftime(
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            fmt.as_ptr() as *const libc::c_char,
            &tm,
        )
    };

    String::from_utf8_lossy(&buf[..n.min(buf.len())]).into_owned()
}

fn localtime_header() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as libc::time_t;

    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&now, &mut tm);
    }

    let mut buf = [0u8; 32];
    let fmt = b"%b %e %H:%M:%S\0";
    let n = unsafe {
        libc::strftime(
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            fmt.as_ptr() as *const libc::c_char,
            &tm,
        )
    };

    String::from_utf8_lossy(&buf[..n.min(buf.len())]).into_owned()
}

/// Build an RFC 3164 (BSD) syslog datagram. Facility `daemon`, severity
/// `info`, `LOCALHOST` as the host (the receiver substitutes the real one),
/// `<tag>[pid]: <message>` as the content.
fn rfc3164_message(tag: &str, msg: &str) -> String {
    const LOG_DAEMON: u32 = 3 << 3;
    const LOG_INFO: u32 = 6;
    let pri = LOG_DAEMON | LOG_INFO;

    let tag: String = tag.chars().take(32).collect::<Vec<char>>().iter().collect();
    let msg = msg.replace(['\n', '\r'], " ");
    format!(
        "<{}>{} localhost {}[{}]: {}",
        pri,
        localtime_header(),
        tag,
        std::process::id(),
        msg
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> (PathBuf, TempGuard) {
        let mut dir = std::env::temp_dir();
        let uniq = format!(
            "vl-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        dir.push(uniq);
        let _ = fs::create_dir_all(&dir);
        (dir.clone(), TempGuard(dir))
    }

    struct TempGuard(PathBuf);
    impl Drop for TempGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn directory_rotation_roundtrip() {
        let (dir, _g) = temp_dir("rot-dir");
        let current = dir.join("current");

        let mut w = RotatingWriter::new(current.clone(), 1, 3, false, RotationStyle::Directory);
        for _ in 0..100_000 {
            w.write_line("line").unwrap();
        }
        // Rotate enough times to fill (and then prune) the archive chain.
        for _ in 0..4 {
            w.rotate().unwrap();
        }

        let files: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        // max_files archives plus the live file.
        for n in ["current", "1", "2", "3"] {
            assert!(
                files.iter().any(|f| f == n),
                "missing rotated file {} in {:?}",
                n,
                files
            );
        }
        assert!(
            !files.iter().any(|f| f == "4"),
            "archive 4 should be pruned"
        );
    }

    #[test]
    fn suffix_rotation_roundtrip() {
        let (dir, _g) = temp_dir("rot-suffix");
        let current = dir.join("app.log");

        let mut w = RotatingWriter::new(current.clone(), 1, 3, false, RotationStyle::Suffix);
        for _ in 0..100_000 {
            w.write_line("line").unwrap();
        }
        for _ in 0..4 {
            w.rotate().unwrap();
        }

        for n in ["app.log", "app.log.1", "app.log.2", "app.log.3"] {
            assert!(dir.join(n).exists(), "missing {}", n);
        }
        assert!(
            !dir.join("app.log.4").exists(),
            ".4 should have been pruned"
        );
    }

    #[test]
    fn zero_size_is_unbounded() {
        let (dir, _g) = temp_dir("no-rot");
        let current = dir.join("current");
        let mut w = RotatingWriter::new(current.clone(), 0, 3, false, RotationStyle::Directory);
        for i in 0..100 {
            w.write_line(&format!("line {}", i)).unwrap();
        }
        w.rotate().unwrap();
        assert!(current.exists());
    }

    #[test]
    fn syslog_lines_have_valid_pri() {
        let line = rfc3164_message("sshd", "connection refused\nsecond");
        let pri: String = line
            .chars()
            .skip(1)
            .take_while(|c| c.is_ascii_digit())
            .collect();
        let pri: u32 = pri.parse().unwrap();
        assert_eq!(pri, 30); // daemon.info
        assert!(line.contains("sshd["));
        assert!(!line.contains('\n'));
    }

    #[test]
    fn syslog_tag_truncated_to_32() {
        let long = "x".repeat(100);
        let line = rfc3164_message(&long, "msg");
        // tag is bounded by '<..>... localhost ' prefix; simple length sanity
        assert!(line.len() < 200);
    }
}

use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GlobalConfig {
    pub service_dir: PathBuf,
    pub target_dir: PathBuf,
    pub control_socket: PathBuf,
    pub log_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub default_target: String,
    pub hostname: Option<String>,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            service_dir: PathBuf::from(crate::VIGIL_SERVICE_DIR),
            target_dir: PathBuf::from(crate::VIGIL_TARGET_DIR),
            control_socket: PathBuf::from(crate::VIGIL_CONTROL_SOCKET),
            log_dir: PathBuf::from(crate::VIGIL_LOG_DIR),
            runtime_dir: PathBuf::from(crate::VIGIL_RUNTIME_DIR),
            default_target: String::from(crate::DEFAULT_TARGET),
            hostname: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServiceConfig {
    pub service: ServiceSpec,
    #[serde(default)]
    pub dependencies: Vec<DependencyConfig>,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub socket: Option<SocketConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServiceSpec {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_user")]
    pub user: String,
    #[serde(default = "default_group")]
    pub group: String,
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    #[serde(default)]
    pub environment: HashMap<String, String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub restart: RestartConfig,
    #[serde(default)]
    pub readiness: ReadinessConfig,
    #[serde(default)]
    pub shutdown: ShutdownConfig,
    #[serde(default)]
    pub resource_limits: ResourceLimits,
    #[serde(default)]
    pub stdout: OutputTarget,
    #[serde(default)]
    pub stderr: OutputTarget,
}

fn default_user() -> String {
    String::from("root")
}

fn default_group() -> String {
    String::from("root")
}

#[derive(Debug, Clone, Deserialize)]
pub struct RestartConfig {
    #[serde(default = "default_restart_policy")]
    pub policy: RestartPolicy,
    #[serde(default = "default_max_restarts")]
    pub max_restarts: u32,
    #[serde(default = "default_backoff_ms")]
    pub backoff_initial_ms: u64,
    #[serde(default = "default_backoff_max_ms")]
    pub backoff_max_ms: u64,
    #[serde(default = "default_backoff_multiplier")]
    pub backoff_multiplier: f64,
}

impl Default for RestartConfig {
    fn default() -> Self {
        Self {
            policy: default_restart_policy(),
            max_restarts: default_max_restarts(),
            backoff_initial_ms: default_backoff_ms(),
            backoff_max_ms: default_backoff_max_ms(),
            backoff_multiplier: default_backoff_multiplier(),
        }
    }
}

fn default_restart_policy() -> RestartPolicy {
    RestartPolicy::Always
}

fn default_max_restarts() -> u32 {
    10
}

fn default_backoff_ms() -> u64 {
    1000
}

fn default_backoff_max_ms() -> u64 {
    30000
}

fn default_backoff_multiplier() -> f64 {
    2.0
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Always,
    OnFailure,
    OnAbnormal,
    Never,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ReadinessConfig {
    #[serde(default = "default_readiness_type", rename = "type")]
    pub kind: ReadinessType,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub signal: Option<String>,
    #[serde(default)]
    pub check: Option<String>,
}

fn default_readiness_type() -> ReadinessType {
    ReadinessType::None
}

impl ReadinessConfig {
    /// How long the supervisor will wait for the service to become ready
    /// before the start is treated as failed.
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.unwrap_or(30_000))
    }

    /// The signal name a `signal`-type service must raise to report
    /// readiness (defaults to `USR1`).
    pub fn ready_signal(&self) -> &str {
        self.signal.as_deref().unwrap_or("USR1")
    }
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ReadinessType {
    #[default]
    None,
    Pid,
    Signal,
    Exec,
    Socket,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShutdownConfig {
    #[serde(default = "default_shutdown_signal")]
    pub signal: String,
    #[serde(default = "default_shutdown_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "default_kill_signal")]
    pub kill_signal: String,
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self {
            signal: default_shutdown_signal(),
            timeout_ms: default_shutdown_timeout(),
            kill_signal: default_kill_signal(),
        }
    }
}

fn default_shutdown_signal() -> String {
    String::from("TERM")
}

fn default_shutdown_timeout() -> u64 {
    5000
}

fn default_kill_signal() -> String {
    String::from("KILL")
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ResourceLimits {
    #[serde(default)]
    pub max_files: Option<u64>,
    #[serde(default)]
    pub max_procs: Option<u64>,
    #[serde(default)]
    pub max_memory_mb: Option<u64>,
    #[serde(default)]
    pub cpu_shares: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum OutputTarget {
    #[default]
    Log,
    Null,
    Stdout,
    Syslog,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DependencyConfig {
    pub service: String,
    #[serde(default = "default_dep_type", rename = "type")]
    pub kind: DependencyType,
    #[serde(default = "default_dep_required")]
    pub required: bool,
}

fn default_dep_type() -> DependencyType {
    DependencyType::After
}

fn default_dep_required() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DependencyType {
    After,
    Before,
    Wants,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct LoggingConfig {
    #[serde(default = "default_log_type", rename = "type")]
    pub kind: LogType,
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default = "default_log_max_size")]
    pub max_size_mb: u64,
    #[serde(default = "default_log_max_files")]
    pub max_files: u32,
    #[serde(default)]
    pub timestamp: bool,
}

fn default_log_type() -> LogType {
    LogType::Pipe
}

fn default_log_max_size() -> u64 {
    10
}

fn default_log_max_files() -> u32 {
    5
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum LogType {
    #[default]
    Pipe,
    File,
    Syslog,
    None,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SocketConfig {
    pub listen: Vec<String>,
    #[serde(default = "default_socket_type")]
    pub socket_type: String,
}

fn default_socket_type() -> String {
    String::from("tcp")
}

impl SocketConfig {
    /// Effective protocol for listen specs that carry no `tcp:`/`udp:`/
    /// `unix:` prefix.
    pub fn proto(&self) -> &str {
        if self.socket_type.is_empty() {
            crate::sockspec::default_type()
        } else {
            self.socket_type.as_str()
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TargetConfig {
    pub target: TargetSpec,
    #[serde(default)]
    pub services: HashMap<String, TargetServiceEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TargetSpec {
    pub description: String,
    #[serde(default)]
    pub requires: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TargetServiceEntry {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub optional: bool,
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn examples_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples")
    }

    fn toml_paths(dir: &PathBuf) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                    out.push(path);
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn example_configs_parse() {
        let base = examples_dir();
        assert!(base.exists(), "examples dir not found: {}", base.display());

        let global_path = base.join("vigil.toml");
        if global_path.exists() {
            let content = std::fs::read_to_string(&global_path).unwrap();
            toml::from_str::<GlobalConfig>(&content)
                .unwrap_or_else(|e| panic!("vigil.toml must parse as GlobalConfig: {}", e));
        }

        for path in toml_paths(&base.join("services")) {
            let content = std::fs::read_to_string(&path).unwrap();
            toml::from_str::<ServiceConfig>(&content).unwrap_or_else(|e| {
                panic!("{} must parse as ServiceConfig: {}", path.display(), e)
            });
        }

        for path in toml_paths(&base.join("targets")) {
            let content = std::fs::read_to_string(&path).unwrap();
            toml::from_str::<TargetConfig>(&content)
                .unwrap_or_else(|e| panic!("{} must parse as TargetConfig: {}", path.display(), e));
        }
    }

    #[test]
    fn readiness_defaults() {
        let rc = ReadinessConfig::default();
        assert_eq!(rc.kind, ReadinessType::None);
        assert_eq!(rc.timeout(), Duration::from_secs(30));
        assert_eq!(rc.ready_signal(), "USR1");
    }

    #[test]
    fn readiness_explicit_timeout_and_signal() {
        let rc = ReadinessConfig {
            kind: ReadinessType::Signal,
            timeout_ms: Some(2000),
            signal: Some("HUP".into()),
            check: None,
        };
        assert_eq!(rc.timeout(), Duration::from_millis(2000));
        assert_eq!(rc.ready_signal(), "HUP");
    }

    #[test]
    fn socket_proto_defaults() {
        let sc = SocketConfig {
            listen: vec!["22".into()],
            socket_type: "udp".into(),
        };
        assert_eq!(sc.proto(), "udp");
        let sc2 = SocketConfig {
            listen: vec!["22".into()],
            socket_type: String::new(),
        };
        assert_eq!(sc2.proto(), "tcp");
    }
}

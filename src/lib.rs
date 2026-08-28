pub mod config;
pub mod dep;
pub mod protocol;

pub const VIGIL_SERVICE_DIR: &str = "/etc/vigil/services";
pub const VIGIL_TARGET_DIR: &str = "/etc/vigil/targets";
pub const VIGIL_CONTROL_SOCKET: &str = "/run/vigil/control.sock";
pub const VIGIL_LOG_DIR: &str = "/var/log/vigil";
pub const VIGIL_RUNTIME_DIR: &str = "/run/vigil";
pub const VIGIL_SUPERVISE_DIR: &str = "/run/vigil/supervise";
pub const DEFAULT_TARGET: &str = "default";
pub const SUPERVISOR_SOCKET_ENV: &str = "VIGIL_SOCK";

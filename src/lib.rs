pub mod config;
pub mod dep;
pub mod protocol;
pub mod sockspec;
pub mod util;

pub const VIGIL_SERVICE_DIR: &str = "/etc/vigil/services";
pub const VIGIL_TARGET_DIR: &str = "/etc/vigil/targets";
pub const VIGIL_CONTROL_SOCKET: &str = "/run/vigil/control.sock";
pub const VIGIL_LOG_DIR: &str = "/var/log/vigil";
pub const VIGIL_RUNTIME_DIR: &str = "/run/vigil";
pub const VIGIL_SUPERVISE_DIR: &str = "/run/vigil/supervise";
pub const DEFAULT_TARGET: &str = "default";

/// First file descriptor handed to a socket-activated service
/// (matches the `SD_LISTEN_FDS_START` convention so services built for
/// systemd socket activation work unchanged).
pub const LISTEN_FDS_START: i32 = 3;

/// Environment variable pointing a service at its supervising process.
pub const SUPERVISOR_PID_ENV: &str = "VIGIL_SUPERVISOR_PID";

/// Environment variable naming the signal a service must raise to report
/// readiness (only set when `[service.readiness] type = "signal"`).
pub const READY_SIGNAL_ENV: &str = "VIGIL_READY_SIGNAL";

/// Number of listening file descriptors passed to a socket-activated
/// service (same convention as systemd's `LISTEN_FDS`).
pub const LISTEN_FDS_ENV: &str = "LISTEN_FDS";

/// PID that owns the inherited listening descriptors (systemd convention).
pub const LISTEN_PID_ENV: &str = "LISTEN_PID";

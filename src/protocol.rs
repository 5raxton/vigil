use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Status { service: Option<String> },
    Start { service: String },
    Stop { service: String },
    Restart { service: String },
    List,
    Log { service: String, lines: usize },
    Reload,
    Shutdown { action: ShutdownAction },
    Ping,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ShutdownAction {
    Poweroff,
    Reboot,
    Halt,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Ok { message: String },
    Error { message: String },
    Status(ServiceStatus),
    List(Vec<ServiceInfo>),
    LogLines(Vec<String>),
    Pong,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub name: String,
    pub state: String,
    pub pid: Option<u32>,
    pub uptime_secs: u64,
    pub restart_count: u32,
    pub description: String,
    pub command: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ServiceInfo {
    pub name: String,
    pub state: String,
    pub pid: Option<u32>,
    pub description: String,
}

pub fn write_message(stream: &mut impl std::io::Write, msg: &impl Serialize) -> anyhow::Result<()> {
    let json = serde_json::to_vec(msg)?;
    let len = (json.len() as u32).to_le_bytes();
    stream.write_all(&len)?;
    stream.write_all(&json)?;
    stream.flush()?;
    Ok(())
}

pub fn read_message<T: for<'de> Deserialize<'de>>(
    stream: &mut impl std::io::Read,
) -> anyhow::Result<T> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;

    if len > 1024 * 1024 {
        anyhow::bail!("message too large: {} bytes", len);
    }

    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    let msg = serde_json::from_slice(&buf)?;
    Ok(msg)
}

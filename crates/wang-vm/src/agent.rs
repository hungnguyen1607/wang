//! Host side of the guest agent protocol: Firecracker exposes the guest's vsock as a Unix
//! socket; `CONNECT 52\n` reaches the agent, then one JSON request/reply per line.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub const AGENT_PORT: u32 = 52;
/// Guest 127.0.0.1:3128 is forwarded to this vsock port on the host (`<uds>_3128`).
pub const PROXY_PORT: u32 = 3128;

/// The result of a command in the VM.
#[derive(Debug, Clone, Default)]
pub struct Exec {
    pub exit: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub truncated: bool,
    pub ms: u64,
}

impl Exec {
    pub fn ok(&self) -> bool {
        self.exit == 0 && !self.timed_out
    }
}

/// One request on a fresh connection (connections don't survive snapshots, so none are kept).
pub async fn call(vsock: &Path, mut req: Value, timeout: Duration) -> Result<Value> {
    // The guest clock stops while a snapshot sits on disk: send ours so the agent can fix it.
    req["now_ms"] = json!(SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0));
    let fut = async {
        let s = UnixStream::connect(vsock).await?;
        let (r, mut w) = s.into_split();
        let mut r = BufReader::new(r);
        w.write_all(format!("CONNECT {AGENT_PORT}\n").as_bytes()).await?;
        let mut line = String::new();
        r.read_line(&mut line).await?;
        if !line.starts_with("OK") {
            bail!("vsock handshake failed: {}", line.trim());
        }
        w.write_all(format!("{req}\n").as_bytes()).await?;
        line.clear();
        r.read_line(&mut line).await?;
        if line.is_empty() {
            bail!("the VM agent closed the connection");
        }
        let v: Value = serde_json::from_str(&line).context("bad reply from the VM agent")?;
        if v["ok"] != true {
            bail!("{}", v["error"].as_str().unwrap_or("VM agent error"));
        }
        Ok(v)
    };
    tokio::time::timeout(timeout, fut).await.context("VM agent timed out")?
}

/// Wait until the agent answers (boot or restore).
pub async fn wait_ready(vsock: &Path, timeout: Duration) -> Result<Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last = String::new();
    while tokio::time::Instant::now() < deadline {
        match call(vsock, json!({"op": "ping"}), Duration::from_millis(500)).await {
            Ok(v) => return Ok(v),
            Err(e) => last = e.to_string(),
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    bail!("VM agent didn't answer within {:.0}s ({last})", timeout.as_secs_f64())
}

pub fn parse_exec(v: &Value) -> Exec {
    Exec {
        exit: v["exit"].as_i64().unwrap_or(-1) as i32,
        stdout: v["stdout"].as_str().unwrap_or("").to_string(),
        stderr: v["stderr"].as_str().unwrap_or("").to_string(),
        timed_out: v["timed_out"].as_bool().unwrap_or(false),
        truncated: v["truncated"].as_bool().unwrap_or(false),
        ms: v["ms"].as_u64().unwrap_or(0),
    }
}

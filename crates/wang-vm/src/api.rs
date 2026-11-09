//! Firecracker's REST API over its Unix socket: tiny HTTP/1.1 client (PUT/PATCH/GET with JSON).

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

pub async fn call(sock: &Path, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
    let fut = async {
        let mut s = UnixStream::connect(sock).await.with_context(|| format!("connecting to {}", sock.display()))?;
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        s.write_all(req.as_bytes()).await?;
        // Firecracker keeps the connection open: read the headers, then exactly Content-Length.
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            let n = s.read(&mut chunk).await?;
            if n == 0 {
                bail!("firecracker closed the API connection");
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let len: usize = String::from_utf8_lossy(&buf[..head_end])
            .lines()
            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().to_string()))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        while buf.len() < head_end + len {
            let n = s.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let text = String::from_utf8_lossy(&buf);
        let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let status: u16 = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).context("bad HTTP response from firecracker")?;
        let v: Value = if rest.trim().is_empty() { Value::Null } else { serde_json::from_str(rest.trim()).unwrap_or(Value::String(rest.to_string())) };
        if !(200..300).contains(&status) {
            let msg = v["fault_message"].as_str().map(String::from).unwrap_or_else(|| v.to_string());
            bail!("firecracker {method} {path}: {status} {msg}");
        }
        Ok(v)
    };
    tokio::time::timeout(Duration::from_secs(30), fut).await.with_context(|| format!("firecracker {method} {path} timed out"))?
}

pub async fn put(sock: &Path, path: &str, body: Value) -> Result<Value> {
    call(sock, "PUT", path, Some(&body)).await
}

pub async fn patch(sock: &Path, path: &str, body: Value) -> Result<Value> {
    call(sock, "PATCH", path, Some(&body)).await
}

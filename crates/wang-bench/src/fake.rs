//! Fake model server: speaks OpenRouter's streamed chat-completions protocol on 127.0.0.1,
//! so the bench exercises wang's real model client, HTTP stack and SSE parser for $0.
//!
//! It keeps no per-agent state. The step is the number of assistant messages already in
//! the request, and the reply is that step of a fixed [`Script`]. Keep-alive is supported
//! (responses carry Content-Length), as reqwest pools connections.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One canned model reply.
#[derive(Debug, Clone)]
pub enum Reply {
    Tool { name: String, args: Value },
    Text(String),
}

/// What the fake model does at each step of a task.
#[derive(Debug, Clone)]
pub struct Script {
    pub steps: Vec<Reply>,
    pub prompt_tokens: u64,
    pub output_tokens: u64,
}

impl Script {
    /// Default task: count lines with the shell, read the file, answer.
    /// Two real sandboxed tool calls and three model calls.
    pub fn default_task() -> Self {
        Self {
            steps: vec![
                Reply::Tool { name: "shell".into(), args: json!({"command": "wc -l data.txt"}) },
                Reply::Tool { name: "read_file".into(), args: json!({"path": "data.txt"}) },
                Reply::Text("data.txt has 3 lines: alpha, beta, gamma.".into()),
            ],
            prompt_tokens: 2_000,
            output_tokens: 60,
        }
    }

    pub fn model_calls(&self) -> usize {
        self.steps.len()
    }

    /// Build the SSE body for `step` (0-based). Past the end, the last reply repeats.
    pub fn sse(&self, step: usize) -> String {
        let reply = &self.steps[step.min(self.steps.len() - 1)];
        let mut out = String::from(": OPENROUTER PROCESSING\n\n");
        let mut push = |v: Value| out.push_str(&format!("data: {v}\n\n"));
        push(json!({"model": "bench/fake", "choices": [{"delta": {"role": "assistant"}}]}));
        let finish = match reply {
            Reply::Text(t) => {
                // A few chunks, like a real stream.
                let words: Vec<&str> = t.split_inclusive(' ').collect();
                for chunk in words.chunks(4) {
                    push(json!({"choices": [{"delta": {"content": chunk.concat()}}]}));
                }
                "stop"
            }
            Reply::Tool { name, args } => {
                push(json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": format!("call_{step}"),
                    "type": "function", "function": {"name": name, "arguments": args.to_string()}}]}}]}));
                "tool_calls"
            }
        };
        push(json!({"choices": [{"delta": {}, "finish_reason": finish}],
            "usage": {"prompt_tokens": self.prompt_tokens + 300 * step as u64, "completion_tokens": self.output_tokens,
                      "prompt_tokens_details": {"cached_tokens": if step > 0 { self.prompt_tokens } else { 0 }},
                      "cost": 0.0}}));
        out.push_str("data: [DONE]\n\n");
        out
    }
}

/// Count of assistant messages = how many steps this task has already taken.
pub fn step_of(body: &Value) -> usize {
    body["messages"].as_array().map(|m| m.iter().filter(|m| m["role"] == "assistant").count()).unwrap_or(0)
}

/// Runs on its own OS thread and runtime: a real model is remote, so its work must not be
/// billed to the agents' event loop.
pub struct FakeServer {
    pub addr: std::net::SocketAddr,
    pub requests: Arc<AtomicU64>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakeServer {
    /// `latency`: delay before the reply starts (stands in for the model's time to first token).
    pub async fn start(script: Script, latency: Duration) -> Result<Self> {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        std_listener.set_nonblocking(true)?;
        let addr = std_listener.local_addr()?;
        let requests = Arc::new(AtomicU64::new(0));
        let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
        let (script, n) = (Arc::new(script), requests.clone());
        let thread = std::thread::Builder::new().name("fake-model".into()).spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("fake server runtime");
            rt.block_on(async move {
                let listener = TcpListener::from_std(std_listener).expect("fake server listener");
                loop {
                    tokio::select! {
                        _ = &mut rx => break,
                        acc = listener.accept() => {
                            let Ok((sock, _)) = acc else { continue };
                            let (script, n) = (script.clone(), n.clone());
                            tokio::spawn(async move {
                                let _ = serve(sock, &script, latency, &n).await;
                            });
                        }
                    }
                }
            });
        })?;
        Ok(Self { addr, requests, shutdown: Some(tx), thread: Some(thread) })
    }

    pub fn base_url(&self) -> String {
        format!("http://{}/api/v1", self.addr)
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

async fn serve(mut sock: TcpStream, script: &Script, latency: Duration, n: &AtomicU64) -> Result<()> {
    sock.set_nodelay(true)?;
    let mut buf = Vec::with_capacity(16 * 1024);
    loop {
        // Headers.
        let head_end = loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            let mut chunk = [0u8; 8192];
            let k = sock.read(&mut chunk).await?;
            if k == 0 {
                return Ok(()); // client closed the connection
            }
            buf.extend_from_slice(&chunk[..k]);
            if buf.len() > 1 << 20 {
                bail!("headers too large");
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
        let len: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse().ok())
            .context("no content-length")?;
        while buf.len() < head_end + len {
            let mut chunk = vec![0u8; (head_end + len - buf.len()).min(64 * 1024)];
            let k = sock.read(&mut chunk).await?;
            if k == 0 {
                bail!("body cut short");
            }
            buf.extend_from_slice(&chunk[..k]);
        }
        let body: Value = serde_json::from_slice(&buf[head_end..head_end + len]).unwrap_or(Value::Null);
        buf.drain(..head_end + len);
        n.fetch_add(1, Ordering::Relaxed);

        if !latency.is_zero() {
            tokio::time::sleep(latency).await;
        }
        let sse = script.sse(step_of(&body));
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{sse}",
            sse.len()
        );
        sock.write_all(resp.as_bytes()).await?;
    }
}

//! Chrome DevTools Protocol client over `--remote-debugging-pipe`: JSON messages separated by
//! NUL bytes, written to Chrome's fd 3 and read from its fd 4. No port, no WebSocket, so nothing
//! else on the machine (or in the jail) can talk to the browser.
//!
//! Sessions use flat mode (`Target.attachToTarget {flatten: true}`): page commands carry a
//! `sessionId` on the same pipe.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc, oneshot};

/// An event from the browser (`Page.loadEventFired`, ...).
#[derive(Debug, Clone)]
pub struct Event {
    pub method: String,
    pub params: Value,
    pub session: Option<String>,
}

type Pending = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<Result<Value>>>>>>;

pub struct Cdp {
    out: mpsc::UnboundedSender<Vec<u8>>,
    /// `None` once the pipe closed: every later call fails fast.
    pending: Pending,
    events: broadcast::Sender<Event>,
    next_id: AtomicU64,
    /// Why the pipe closed, once it has.
    closed_reason: Arc<Mutex<Option<String>>>,
    pub timeout: Duration,
}

/// Largest single message accepted from the browser (a full-page screenshot is a few MB).
const MAX_MESSAGE: usize = 64 << 20;

impl Cdp {
    /// `w` is Chrome's fd 3 (we write), `r` its fd 4 (we read).
    pub fn new<R, W>(r: R, w: W) -> Arc<Self>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (out, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let (events, _) = broadcast::channel(1024);
        let closed_reason = Arc::new(Mutex::new(None));
        let cdp = Arc::new(Self {
            out,
            pending: pending.clone(),
            events: events.clone(),
            next_id: AtomicU64::new(1),
            closed_reason: closed_reason.clone(),
            timeout: Duration::from_secs(30),
        });

        let mut w = w;
        tokio::spawn(async move {
            while let Some(mut msg) = rx.recv().await {
                msg.push(0);
                if w.write_all(&msg).await.is_err() || w.flush().await.is_err() {
                    break;
                }
            }
        });

        let mut r = r;
        tokio::spawn(async move {
            let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
            let mut chunk = vec![0u8; 64 * 1024];
            let why = loop {
                let n = match r.read(&mut chunk).await {
                    Ok(0) => break "browser closed the DevTools pipe (it exited or was killed)".to_string(),
                    Ok(n) => n,
                    Err(e) => break format!("DevTools pipe: {e}"),
                };
                let mut start = 0;
                buf.extend_from_slice(&chunk[..n]);
                while let Some(i) = buf[start..].iter().position(|&b| b == 0) {
                    dispatch(&buf[start..start + i], &pending, &events);
                    start += i + 1;
                }
                buf.drain(..start);
                if buf.len() > MAX_MESSAGE {
                    break "DevTools message too large".to_string();
                }
            };
            // Fail everything in flight, and everything after.
            *closed_reason.lock().unwrap() = Some(why.clone());
            if let Some(map) = pending.lock().unwrap().take() {
                for (_, tx) in map {
                    let _ = tx.send(Err(anyhow!("{why}")));
                }
            }
            let _ = events.send(Event { method: "wang.closed".into(), params: json!({"reason": why}), session: None });
        });
        cdp
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn closed_reason(&self) -> Option<String> {
        self.closed_reason.lock().unwrap().clone()
    }

    pub fn is_closed(&self) -> bool {
        self.pending.lock().unwrap().is_none()
    }

    /// Send a command and wait for its result.
    pub async fn call(&self, session: Option<&str>, method: &str, params: Value) -> Result<Value> {
        self.call_timeout(session, method, params, self.timeout).await
    }

    pub async fn call_timeout(&self, session: Option<&str>, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut p = self.pending.lock().unwrap();
            let Some(map) = p.as_mut() else { bail!("browser is gone (DevTools pipe closed)") };
            map.insert(id, tx);
        }
        let mut msg = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        if self.out.send(msg.to_string().into_bytes()).is_err() {
            bail!("browser is gone (DevTools pipe closed)");
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => bail!("browser is gone (DevTools pipe closed)"),
            Err(_) => {
                if let Some(map) = self.pending.lock().unwrap().as_mut() {
                    map.remove(&id);
                }
                bail!("{method} timed out after {:.0}s", timeout.as_secs_f64())
            }
        }
    }
}

fn dispatch(raw: &[u8], pending: &Pending, events: &broadcast::Sender<Event>) {
    let Ok(v) = serde_json::from_slice::<Value>(raw) else { return };
    if let Some(id) = v.get("id").and_then(Value::as_u64) {
        let tx = pending.lock().unwrap().as_mut().and_then(|m| m.remove(&id));
        if let Some(tx) = tx {
            let r = match v.get("error") {
                Some(e) => Err(anyhow!("{}", e["message"].as_str().unwrap_or("DevTools error"))),
                None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = tx.send(r);
        }
    } else if let Some(m) = v.get("method").and_then(Value::as_str) {
        let _ = events.send(Event {
            method: m.to_string(),
            params: v.get("params").cloned().unwrap_or(Value::Null),
            session: v.get("sessionId").and_then(Value::as_str).map(String::from),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake browser on the other end of two in-memory pipes.
    #[tokio::test(flavor = "current_thread")]
    async fn calls_events_errors_and_close() {
        let (ours_w, mut theirs_r) = tokio::io::duplex(1 << 16);
        let (mut theirs_w, ours_r) = tokio::io::duplex(1 << 16);
        let cdp = Cdp::new(ours_r, ours_w);
        let mut ev = cdp.subscribe();

        let fake = tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let mut handled = 0;
            while handled < 2 {
                let n = theirs_r.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                while let Some(i) = buf.iter().position(|&b| b == 0) {
                    let msg: Value = serde_json::from_slice(&buf[..i]).unwrap();
                    buf.drain(..=i);
                    let id = msg["id"].as_u64().unwrap();
                    let reply = if msg["method"] == "Bad.method" {
                        json!({"id": id, "error": {"code": -32601, "message": "'Bad.method' wasn't found"}})
                    } else {
                        assert_eq!(msg["sessionId"], "S1");
                        json!({"id": id, "result": {"echo": msg["params"]["x"]}})
                    };
                    // An event split across writes, then the reply, in one go.
                    let ev = json!({"method": "Page.loadEventFired", "params": {"timestamp": 1}, "sessionId": "S1"}).to_string();
                    let (a, b) = ev.as_bytes().split_at(10);
                    theirs_w.write_all(a).await.unwrap();
                    theirs_w.write_all(b).await.unwrap();
                    theirs_w.write_all(b"\0").await.unwrap();
                    theirs_w.write_all(format!("{reply}\0").as_bytes()).await.unwrap();
                    handled += 1;
                }
            }
            // then the browser "dies"
        });

        let r = cdp.call(Some("S1"), "Echo.it", json!({"x": 42})).await.unwrap();
        assert_eq!(r["echo"], 42);
        let e = ev.recv().await.unwrap();
        assert_eq!((e.method.as_str(), e.session.as_deref()), ("Page.loadEventFired", Some("S1")));
        let err = cdp.call(None, "Bad.method", json!({})).await.unwrap_err();
        assert!(err.to_string().contains("wasn't found"));
        fake.await.unwrap();
        // Pipe closed: calls fail fast instead of hanging.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(cdp.is_closed());
        assert!(cdp.call(None, "X.y", json!({})).await.unwrap_err().to_string().contains("gone"));
    }
}

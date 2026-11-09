//! Record model calls during a live eval run and replay them exactly ($0, deterministic).
//!
//! Each call is stored with a stable hash of its request. On replay, a different request
//! hash means the agent behaved differently than when recorded ("diverged"): the run still
//! proceeds, but the result is flagged, because the canned answer may no longer fit.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{CallCtx, ModelClient, ModelResponse};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recorded {
    pub step: u32,
    pub request_hash: String,
    pub response: ModelResponse,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cassette {
    pub model: String,
    pub calls: Vec<Recorded>,
}

impl Cassette {
    pub fn load(path: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Ok(serde_json::from_str(&s)?)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

/// FNV-1a over the request: stable across builds (unlike std's hasher).
pub fn request_hash(system: &str, tools: &[Value], messages: &[Value]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    let mut feed = |b: &[u8]| {
        for x in b {
            h ^= *x as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    };
    feed(system.as_bytes());
    feed(serde_json::to_string(tools).unwrap_or_default().as_bytes());
    feed(serde_json::to_string(messages).unwrap_or_default().as_bytes());
    format!("{h:016x}")
}

pub struct Recording<C> {
    pub inner: C,
    pub tape: Mutex<Vec<Recorded>>,
}

impl<C: ModelClient> Recording<C> {
    pub fn new(inner: C) -> Self {
        Self { inner, tape: Mutex::new(vec![]) }
    }
    pub fn cassette(&self) -> Cassette {
        Cassette { model: self.inner.model().to_string(), calls: self.tape.lock().unwrap().clone() }
    }
}

impl<C: ModelClient> ModelClient for Recording<C> {
    fn provider(&self) -> &'static str {
        self.inner.provider()
    }
    fn model(&self) -> &str {
        self.inner.model()
    }
    async fn create(&self, system: &str, tools: &[Value], messages: &[Value], ctx: &CallCtx<'_>) -> Result<ModelResponse> {
        let r = self.inner.create(system, tools, messages, ctx).await?;
        self.tape.lock().unwrap().push(Recorded { step: ctx.step, request_hash: request_hash(system, tools, messages), response: r.clone() });
        Ok(r)
    }
}

pub struct Replay {
    model: String,
    tape: Mutex<VecDeque<Recorded>>,
    pub diverged: AtomicU32,
}

impl Replay {
    pub fn new(c: Cassette) -> Self {
        Self { model: c.model, tape: Mutex::new(c.calls.into()), diverged: AtomicU32::new(0) }
    }
    pub fn diverged(&self) -> u32 {
        self.diverged.load(Ordering::Relaxed)
    }
    pub fn remaining(&self) -> usize {
        self.tape.lock().unwrap().len()
    }
}

impl ModelClient for Replay {
    fn provider(&self) -> &'static str {
        "replay"
    }
    fn model(&self) -> &str {
        &self.model
    }
    async fn create(&self, system: &str, tools: &[Value], messages: &[Value], ctx: &CallCtx<'_>) -> Result<ModelResponse> {
        let Some(rec) = self.tape.lock().unwrap().pop_front() else {
            bail!("replay: cassette has no more recorded calls (the agent took more steps than when recorded)");
        };
        if rec.request_hash != request_hash(system, tools, messages) {
            self.diverged.fetch_add(1, Ordering::Relaxed);
        }
        for b in &rec.response.content {
            if let Some(t) = b.get("text").and_then(Value::as_str) {
                (ctx.on_text)(t);
            }
        }
        Ok(rec.response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::testing::{Mock, resp};
    use serde_json::json;

    #[tokio::test(flavor = "current_thread")]
    async fn record_then_replay() {
        let rec = Recording::new(Mock::new(vec![resp(json!([{"type": "text", "text": "hi"}]), "end_turn", 0)]));
        let noop = |_: &str| {};
        let ctx = CallCtx { session: "s", on_text: &noop, step: 1 };
        let msgs = vec![json!({"role": "user", "content": [{"type": "text", "text": "x"}]})];
        rec.create("sys", &[], &msgs, &ctx).await.unwrap();
        let cas = rec.cassette();
        let p = std::env::temp_dir().join(format!("wang-cassette-{}.json", std::process::id()));
        cas.save(&p).unwrap();

        let rp = Replay::new(Cassette::load(&p).unwrap());
        let got = std::sync::Mutex::new(String::new());
        let on = |t: &str| got.lock().unwrap().push_str(t);
        let ctx = CallCtx { session: "s", on_text: &on, step: 1 };
        let r = rp.create("sys", &[], &msgs, &ctx).await.unwrap();
        assert_eq!(r.content[0]["text"], "hi");
        assert_eq!(*got.lock().unwrap(), "hi");
        assert_eq!(rp.diverged(), 0);
        assert!(rp.create("sys", &[], &msgs, &ctx).await.is_err(), "exhausted");

        let rp = Replay::new(Cassette::load(&p).unwrap());
        rp.create("sys", &[], &[json!({"different": true})], &ctx).await.unwrap();
        assert_eq!(rp.diverged(), 1);
        std::fs::remove_file(p).ok();
        assert_eq!(request_hash("a", &[], &[]), request_hash("a", &[], &[]));
        assert_ne!(request_hash("a", &[], &[]), request_hash("b", &[], &[]));
    }
}

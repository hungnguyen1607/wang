//! A small local model for routine work (history summaries), at $0.
//!
//! llama.cpp's `llama-server` runs inside wang's jail: no network at all, only the model file
//! and its own binaries visible, RAM capped. It listens on a Unix socket in a private directory,
//! so nothing else on the machine can use it. It starts on first use and stops when idle, so
//! its ~2 GB isn't held while nothing needs it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use wang_jail::run::Running;
use wang_jail::{Bind, Jail, Limits, Policy};

#[derive(Debug, Clone)]
pub struct LocalCfg {
    /// Holds `llama-*/llama-server` and `model.gguf` (`wang local setup`).
    pub dir: PathBuf,
    pub threads: u32,
    /// Context window in tokens (KV cache grows with it).
    pub ctx: u32,
    pub memory_bytes: u64,
    pub idle_timeout: Duration,
}

impl LocalCfg {
    pub fn server(&self) -> Option<PathBuf> {
        let mut found: Vec<PathBuf> = std::fs::read_dir(&self.dir)
            .ok()?
            .flatten()
            .map(|e| e.path().join("llama-server"))
            .filter(|p| p.is_file())
            .collect();
        found.sort();
        found.pop()
    }

    pub fn model(&self) -> PathBuf {
        self.dir.join("model.gguf")
    }

    pub fn ready(&self) -> bool {
        self.server().is_some() && self.model().is_file()
    }
}

/// One completion from the local model.
#[derive(Debug, Clone, Default)]
pub struct LocalReply {
    pub text: String,
    pub prompt_tokens: u64,
    pub output_tokens: u64,
    pub ms: u64,
    pub prompt_tok_s: f64,
    pub gen_tok_s: f64,
}

struct Live {
    running: Running,
    rundir: PathBuf,
}

pub struct LocalModel {
    jail: Arc<Jail>,
    pub cfg: LocalCfg,
    pub name: String,
    live: Mutex<Option<Live>>,
    last_used: std::sync::Mutex<Instant>,
    /// Last cold start, ms.
    pub start_ms: std::sync::Mutex<f64>,
}

impl LocalModel {
    pub fn new(jail: Arc<Jail>, cfg: LocalCfg) -> Arc<Self> {
        Arc::new(Self {
            jail,
            cfg,
            name: "local/qwen2.5-1.5b-instruct".into(),
            live: Mutex::new(None),
            last_used: std::sync::Mutex::new(Instant::now()),
            start_ms: std::sync::Mutex::new(0.0),
        })
    }

    fn sock(rundir: &Path) -> PathBuf {
        rundir.join("llm.sock")
    }

    async fn start(&self) -> Result<Live> {
        let t = Instant::now();
        let server = self.cfg.server().context("llama-server not found (run `wang local setup`)")?;
        let bindir = server.parent().context("server dir")?.to_path_buf();
        let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let rundir = base.join(format!("wang-llm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&rundir);
        std::fs::create_dir_all(&rundir)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&rundir, std::fs::Permissions::from_mode(0o700))?;
        let ro = |s: &Path, d: &str| Bind { src: s.to_path_buf(), dst: PathBuf::from(d), writable: false };
        let policy = Policy {
            binds: vec![
                ro(&bindir, "/llm/bin"),
                ro(&self.cfg.model(), "/llm/model.gguf"),
                Bind { src: rundir.clone(), dst: PathBuf::from("/llm/run"), writable: true },
            ],
            limits: Limits {
                memory_bytes: self.cfg.memory_bytes,
                cpus: self.cfg.threads as f64,
                pids: 512,
                timeout: Duration::from_secs(7 * 24 * 3600),
                tmp_bytes: 64 << 20,
                max_output_bytes: 1 << 20,
            },
            ..Default::default()
        };
        let argv: Vec<String> = [
            "/llm/bin/llama-server",
            "-m",
            "/llm/model.gguf",
            "--host",
            "/llm/run/llm.sock",
            "-c",
            &self.cfg.ctx.to_string(),
            "-t",
            &self.cfg.threads.to_string(),
            "--no-webui",
            "--log-disable",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let mut running = self.jail.spawn_piped(&policy, &argv).await.context("starting the local model in the jail")?;
        let sock = Self::sock(&rundir);
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if let Ok((200, _)) = http(&sock, "GET", "/health", None).await {
                break;
            }
            if running.try_exit_code().is_some() || Instant::now() > deadline {
                let st = running.stop().await;
                let log = String::from_utf8_lossy(&st.stderr);
                let tail: Vec<&str> = log.lines().rev().take(6).collect();
                bail!("local model didn't start: {}", tail.into_iter().rev().collect::<Vec<_>>().join("\n"));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        *self.start_ms.lock().unwrap() = t.elapsed().as_secs_f64() * 1e3;
        tracing::info!(ms = t.elapsed().as_millis() as u64, "local model started");
        Ok(Live { running, rundir })
    }

    /// Ask the local model (starting it if needed). Deterministic-ish (temperature 0.2).
    pub async fn chat(&self, system: &str, user: &str, max_tokens: u32) -> Result<LocalReply> {
        *self.last_used.lock().unwrap() = Instant::now();
        let mut live = self.live.lock().await;
        if let Some(l) = live.as_mut()
            && l.running.try_exit_code().is_some()
        {
            *live = None; // died (e.g. out of memory): start again
        }
        if live.is_none() {
            *live = Some(self.start().await?);
        }
        let sock = Self::sock(&live.as_ref().unwrap().rundir);
        let t = Instant::now();
        let body = json!({
            "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}],
            "max_tokens": max_tokens, "temperature": 0.2, "stream": false,
        });
        let (status, v) = http(&sock, "POST", "/v1/chat/completions", Some(&body)).await?;
        *self.last_used.lock().unwrap() = Instant::now();
        if status != 200 {
            bail!("local model: HTTP {status}: {}", v["error"]["message"].as_str().unwrap_or(&v.to_string()));
        }
        Ok(LocalReply {
            text: v["choices"][0]["message"]["content"].as_str().unwrap_or("").trim().to_string(),
            prompt_tokens: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            output_tokens: v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
            ms: t.elapsed().as_millis() as u64,
            prompt_tok_s: v["timings"]["prompt_per_second"].as_f64().unwrap_or(0.0),
            gen_tok_s: v["timings"]["predicted_per_second"].as_f64().unwrap_or(0.0),
        })
    }

    /// Roughly how many characters of prompt fit, leaving room for `out` tokens (~3.5 chars/token).
    pub fn fits_chars(&self, out: u32) -> usize {
        (self.cfg.ctx.saturating_sub(out + 300) as f64 * 3.5) as usize
    }

    pub async fn memory_bytes(&self) -> Option<u64> {
        self.live.lock().await.as_ref().and_then(|l| l.running.memory_bytes())
    }

    pub async fn running(&self) -> bool {
        self.live.lock().await.is_some()
    }

    /// Stop the server (it restarts on the next request).
    pub async fn stop(&self) {
        if let Some(l) = self.live.lock().await.take() {
            l.running.stop().await;
            let _ = std::fs::remove_dir_all(&l.rundir);
        }
    }

    /// Stop the server after `idle_timeout` without requests.
    pub fn spawn_idle_stop(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let Some(m) = me.upgrade() else { return };
                let idle = m.last_used.lock().unwrap().elapsed();
                if idle > m.cfg.idle_timeout && m.running().await {
                    tracing::info!("local model idle for {}s; stopping it", idle.as_secs());
                    m.stop().await;
                }
            }
        });
    }
}

/// Minimal HTTP/1.1 over a Unix socket (non-streaming JSON).
async fn http(sock: &Path, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value)> {
    let fut = async {
        let mut s = UnixStream::connect(sock).await?;
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(req.as_bytes()).await?;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 16384];
        let head_end = loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            let n = s.read(&mut chunk).await?;
            if n == 0 {
                bail!("local model closed the connection");
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
        let status: u16 = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).context("bad HTTP response")?;
        let len: Option<usize> = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse().ok());
        loop {
            if len.is_some_and(|l| buf.len() >= head_end + l) {
                break;
            }
            let n = s.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let end = len.map(|l| (head_end + l).min(buf.len())).unwrap_or(buf.len());
        let v = serde_json::from_slice(&buf[head_end..end]).unwrap_or(Value::Null);
        Ok((status, v))
    };
    tokio::time::timeout(Duration::from_secs(300), fut).await.context("local model timed out")?
}

/// Small models summarize better into a fixed structure than from an open-ended request.
pub const LOCAL_COMPACT_SYSTEM: &str = "You take notes on a conversation between a user and their laptop assistant. \
Fill in this template using ONLY what the conversation says. Copy names, paths, ports, versions, numbers and commands exactly. \
Put work under Done only if the conversation says it was finished; everything else that was mentioned as remaining goes under Pending.\n\n\
Goal:\nDecisions:\nFacts (paths, ports, versions, names):\nDone:\nPending:\nUser preferences:";

/// A realistic conversation to summarize (for `wang local bench` and tests), and the facts a
/// good summary must keep.
pub fn bench_transcript() -> String {
    let mut t = String::new();
    let turns: &[(&str, &str)] = &[
        ("user", "I want to move my shop-api service from Flask to FastAPI. Repo is ~/code/shop-api, Python 3.11."),
        ("assistant", "Got it. I'll look at the app structure first."),
        ("assistant ran shell", "ls ~/code/shop-api && cat requirements.txt"),
        ("tool result", "app/ tests/ requirements.txt docker-compose.yml  flask==3.0.3 psycopg2-binary sqlalchemy==1.4.52 pytest"),
        ("assistant", "It's Flask 3 with SQLAlchemy 1.4 on Postgres. I suggest upgrading to SQLAlchemy 2.0 while we're at it."),
        ("user", "yes do SQLAlchemy 2.0. postgres runs in docker on port 5433 not 5432, careful."),
        ("assistant ran shell", "grep -rn 'route' app/ | wc -l"),
        ("tool result", "37"),
        ("assistant", "There are 37 routes. I converted the products and cart blueprints to FastAPI routers."),
        ("assistant ran shell", "cd ~/code/shop-api && pytest -x -q"),
        ("tool result", "FAILED tests/test_checkout.py::test_checkout_total - AssertionError: 19.99 != 19.990000000000002 ... 1 failed, 41 passed"),
        ("assistant", "test_checkout_total fails on float rounding. The fix is to use Decimal for prices in the checkout service."),
        ("user", "ok use Decimal. and always run pytest with -x for me please, I like it to stop early"),
        ("assistant ran shell", "pytest -x -q"),
        ("tool result", "42 passed in 3.12s"),
        ("assistant", "All 42 tests pass now. Remaining: the orders and admin blueprints (12 routes), then the Dockerfile."),
        ("user", "great. we need to deploy to staging on Friday, so orders first. I'll be away tomorrow."),
    ];
    for _ in 0..3 {
        for (who, what) in turns {
            t.push_str(&format!("{who}: {what}\n"));
        }
        t.push_str("tool result: (long build log omitted) ... warning: unused import ... ok\n");
    }
    t
}

pub const BENCH_FACTS: &[&str] = &["fastapi", "shop-api", "5433", "sqlalchemy 2", "decimal", "-x", "staging", "friday", "orders"];

/// Fraction of [`BENCH_FACTS`] found in a summary, and the missing ones.
pub fn fact_recall(summary: &str) -> (f64, Vec<&'static str>) {
    let s = summary.to_lowercase().replace("2.0", "2");
    let missing: Vec<&str> = BENCH_FACTS.iter().copied().filter(|f| !s.contains(f)).collect();
    (1.0 - missing.len() as f64 / BENCH_FACTS.len() as f64, missing)
}

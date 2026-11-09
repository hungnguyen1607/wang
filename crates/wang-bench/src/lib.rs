//! wang-bench: how many agents can this laptop run at once, and what gives first?
//!
//! A fake model server ([`fake`]) replays a fixed script for $0. At each level, N real
//! [`Agent`]s run at once against it: the real OpenRouter client, the real tools in their own
//! jail, one shared ledger + trace DB, on the same single-threaded runtime the daemon uses.
//! Levels ramp (1 → 1000 by default) and the ramp stops early on errors or low memory.
//!
//! Per level: tasks/s, model steps/s, task latency, model-call and tool-call latency (from
//! the trace spans), runtime stalls (how late a 10 ms timer fires: blocking work on the
//! event loop shows up here), and memory.

pub mod fake;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use wang_daemon::agent::{Agent, Conversation, Limits};
use wang_daemon::config::ModelCfg;
use wang_daemon::model::{CallCtx, ModelClient, ModelResponse, OpenRouter};
use wang_daemon::tools::{Sandbox, ToolCtx, WebPolicy};
use wang_jail::{Jail, Policy};
use wang_tokens::Ledger;
use wang_trace::Tracer;

pub use fake::{FakeServer, Script};

pub struct BenchOpts {
    pub levels: Vec<usize>,
    pub tasks_per_agent: usize,
    /// Fake model's delay before each reply.
    pub latency: Duration,
    /// Stop when system MemAvailable would drop below this.
    pub min_free_mb: u64,
    /// Stop after a level with more failed tasks than this fraction.
    pub max_error_rate: f64,
    pub sandbox: bool,
    pub out: PathBuf,
}

impl Default for BenchOpts {
    fn default() -> Self {
        Self {
            levels: vec![1, 10, 50, 100, 250, 500, 1000],
            tasks_per_agent: 2,
            latency: Duration::from_millis(100),
            min_free_mb: 1024,
            max_error_rate: 0.05,
            sandbox: true,
            out: PathBuf::from("bench/runs/latest"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Pct {
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
}

pub fn pct(mut v: Vec<f64>) -> Pct {
    if v.is_empty() {
        return Pct::default();
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let at = |q: f64| v[((v.len() as f64 - 1.0) * q).round() as usize];
    Pct { p50: at(0.5), p95: at(0.95), max: v[v.len() - 1] }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Level {
    pub agents: usize,
    pub tasks: usize,
    pub failed: usize,
    pub wall_s: f64,
    pub tasks_per_s: f64,
    pub steps_per_s: f64,
    /// Task latency, ms.
    pub task_ms: Pct,
    pub model_ms: Pct,
    pub tool_ms: Pct,
    /// How late a 10 ms timer fired, ms.
    pub stall_ms: Pct,
    pub rss_peak_mb: f64,
    pub free_before_mb: f64,
    pub free_min_mb: f64,
    pub first_error: Option<String>,
    /// Tool calls that failed (the fake model's answer doesn't depend on them, so a task can
    /// "succeed" over a broken tool: count them separately).
    #[serde(default)]
    pub tool_errors: usize,
}

impl Level {
    /// System memory used per agent at this level (daemon + jails + kernel), MB.
    pub fn mb_per_agent(&self) -> f64 {
        ((self.free_before_mb - self.free_min_mb) / self.agents.max(1) as f64).max(0.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchReport {
    pub started_at: String,
    pub sandbox: bool,
    pub latency_ms: u64,
    pub model_calls_per_task: usize,
    pub tasks_per_agent: usize,
    pub levels: Vec<Level>,
    pub stopped: Option<String>,
}

/// One OpenRouter client shared by every agent, like the daemon does.
#[derive(Clone)]
struct Shared(Arc<OpenRouter>);

impl ModelClient for Shared {
    fn provider(&self) -> &'static str {
        self.0.provider()
    }
    fn model(&self) -> &str {
        self.0.model()
    }
    async fn create(&self, system: &str, tools: &[Value], messages: &[Value], ctx: &CallCtx<'_>) -> Result<ModelResponse> {
        self.0.create(system, tools, messages, ctx).await
    }
}

struct Env {
    client: Shared,
    ledger: Arc<Ledger>,
    tracer: Arc<Tracer>,
    jail: Option<Arc<Jail>>,
    db: PathBuf,
    fixtures: PathBuf,
    tasks_per_agent: usize,
    low_memory: AtomicBool,
}

pub fn mem_available_mb() -> f64 {
    read_kb("/proc/meminfo", "MemAvailable:") / 1024.0
}

pub fn rss_mb() -> f64 {
    read_kb("/proc/self/status", "VmRSS:") / 1024.0
}

fn read_kb(file: &str, key: &str) -> f64 {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix(key).map(|v| v.trim().trim_end_matches("kB").trim().to_string())))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0)
}

const DATA: &str = "alpha\nbeta\ngamma\n";

/// One agent: its own fixture folder and jail policy, `tasks_per_agent` tasks in a row.
/// Returns (task latency ms or error, task id) per task.
async fn agent(env: &Env, level: usize, idx: usize) -> Vec<(Result<f64, String>, Option<i64>)> {
    let dir = env.fixtures.join(format!("{level}-{idx}"));
    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join("data.txt"), DATA)) {
        return vec![(Err(format!("fixture: {e}")), None)];
    }
    let sandbox = match &env.jail {
        Some(j) => Sandbox::Jailed {
            jail: j.clone(),
            policy: Policy { read: vec![dir.clone()], cwd: Some(dir.clone()), ..Default::default() },
        },
        None => Sandbox::Off,
    };
    let ctx = ToolCtx::with(dir, sandbox, WebPolicy { domains: vec![], allow_private: false });
    let a = Agent::new(env.client.clone(), env.ledger.clone(), env.tracer.clone(), ctx, Limits { max_steps: 6, ..Default::default() });
    let mut out = vec![];
    for k in 0..env.tasks_per_agent {
        if env.low_memory.load(Ordering::Relaxed) {
            break;
        }
        let mut conv = Conversation::new(format!("bench-{level}-{idx}-{k}"));
        let t = Instant::now();
        let r = match a.run_turn(&mut conv, "bench", "How many lines are in data.txt?", &|_| {}).await {
            Ok(r) if r.outcome == "done" && r.reply.contains("3 lines") => (Ok(t.elapsed().as_secs_f64() * 1e3), Some(r.task_id)),
            Ok(r) => (Err(format!("outcome {}: {}", r.outcome, r.reply.chars().take(200).collect::<String>())), Some(r.task_id)),
            Err(e) => (Err(format!("{e:#}")), None),
        };
        out.push(r);
    }
    out
}

/// Model-call and tool-call durations (ms) for tasks in `ids`, from the trace spans.
fn tool_errors(db: &Path, lo: i64, hi: i64) -> Result<usize> {
    let c = rusqlite::Connection::open(db)?;
    Ok(c.query_row("SELECT COUNT(*) FROM tool_calls WHERE task_id BETWEEN ?1 AND ?2 AND is_error = 1", [lo, hi], |r| r.get::<_, i64>(0))? as usize)
}

fn span_durations(db: &Path, lo: i64, hi: i64) -> Result<(Vec<f64>, Vec<f64>)> {
    let c = rusqlite::Connection::open(db)?;
    let mut st = c.prepare("SELECT kind, end_us - start_us FROM spans WHERE task_id BETWEEN ?1 AND ?2 AND end_us IS NOT NULL AND kind IN ('model_call', 'tool_call')")?;
    let (mut model, mut tool) = (vec![], vec![]);
    for row in st.query_map([lo, hi], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (k, us) = row?;
        if k == "model_call" { &mut model } else { &mut tool }.push(us as f64 / 1e3);
    }
    Ok((model, tool))
}

async fn run_level(env: &Arc<Env>, n: usize, calls_per_task: usize, min_free_mb: f64) -> Result<Level> {
    let free_before = mem_available_mb();
    // Monitor: timer lateness, peak RSS, lowest free memory. Trips `low_memory`.
    let stop = Arc::new(AtomicBool::new(false));
    let monitor = {
        let (stop, env) = (stop.clone(), env.clone());
        tokio::spawn(async move {
            let (mut stalls, mut rss, mut free) = (vec![], 0f64, f64::MAX);
            let mut i = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let t = Instant::now();
                tokio::time::sleep(Duration::from_millis(10)).await;
                stalls.push((t.elapsed().as_secs_f64() * 1e3 - 10.0).max(0.0));
                if i % 5 == 0 {
                    rss = rss.max(rss_mb());
                    let f = mem_available_mb();
                    free = free.min(f);
                    if f < min_free_mb {
                        env.low_memory.store(true, Ordering::Relaxed);
                    }
                }
                i += 1;
            }
            (stalls, rss, free.min(mem_available_mb()))
        })
    };
    let started = Instant::now();
    let results: Vec<_> = futures::future::join_all((0..n).map(|i| {
        let env = env.clone();
        async move { agent(&env, n, i).await }
    }))
    .await
    .into_iter()
    .flatten()
    .collect();
    let wall_s = started.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    let (stalls, rss_peak, free_min) = monitor.await?;

    let ids: Vec<i64> = results.iter().filter_map(|r| r.1).collect();
    let (model, tool, tool_errs) = match (ids.iter().min(), ids.iter().max()) {
        (Some(lo), Some(hi)) => {
            let (m, t) = span_durations(&env.db, *lo, *hi)?;
            (m, t, tool_errors(&env.db, *lo, *hi)?)
        }
        _ => (vec![], vec![], 0),
    };
    let ok: Vec<f64> = results.iter().filter_map(|r| r.0.as_ref().ok().copied()).collect();
    let first_error = results.iter().find_map(|r| r.0.as_ref().err().cloned());
    Ok(Level {
        agents: n,
        tasks: results.len(),
        failed: results.len() - ok.len(),
        tool_errors: tool_errs,
        wall_s,
        tasks_per_s: ok.len() as f64 / wall_s,
        steps_per_s: (ok.len() * calls_per_task) as f64 / wall_s,
        task_ms: pct(ok),
        model_ms: pct(model),
        tool_ms: pct(tool),
        stall_ms: pct(stalls),
        rss_peak_mb: rss_peak,
        free_before_mb: free_before,
        free_min_mb: free_min,
        first_error,
    })
}

pub async fn run(opts: &BenchOpts, mut on_level: impl FnMut(&Level)) -> Result<BenchReport> {
    let script = Script::default_task();
    let calls = script.model_calls();
    let server = FakeServer::start(script, opts.latency).await?;
    let cfg = ModelCfg { base_url: Some(server.base_url()), model: Some("bench/fake".into()), ..Default::default() };
    let client = Shared(Arc::new(OpenRouter::with_key(&cfg, "bench".into())?));

    if opts.out.exists() {
        std::fs::remove_dir_all(&opts.out).with_context(|| format!("clearing {}", opts.out.display()))?;
    }
    std::fs::create_dir_all(&opts.out)?;
    let db = opts.out.join("bench.db");
    let jail = if opts.sandbox {
        let j = Arc::new(Jail::new()?);
        j.check().await?;
        Some(j)
    } else {
        None
    };
    let env = Arc::new(Env {
        client,
        ledger: Arc::new(Ledger::open(&db)?),
        tracer: Arc::new(Tracer::open(&db)?),
        jail,
        db,
        fixtures: opts.out.join("fixtures"),
        tasks_per_agent: opts.tasks_per_agent.max(1),
        low_memory: AtomicBool::new(false),
    });

    let mut report = BenchReport {
        started_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        sandbox: opts.sandbox,
        latency_ms: opts.latency.as_millis() as u64,
        model_calls_per_task: calls,
        tasks_per_agent: env.tasks_per_agent,
        levels: vec![],
        stopped: None,
    };
    let min_free = opts.min_free_mb as f64;
    for &n in &opts.levels {
        // Predict this level's memory from the last one; don't start a level that would swap.
        if let Some(prev) = report.levels.last() {
            let need = prev.mb_per_agent() * n as f64;
            let free = mem_available_mb();
            if free - need < min_free {
                report.stopped = Some(format!(
                    "skipped {n} agents: needs ~{need:.0} MB ({:.1} MB/agent at {}), {free:.0} MB available, keeping {min_free:.0} MB free",
                    prev.mb_per_agent(),
                    prev.agents
                ));
                break;
            }
        }
        let level = run_level(&env, n, calls, min_free).await?;
        on_level(&level);
        let failed = level.failed as f64 / level.tasks.max(1) as f64;
        let low = env.low_memory.load(Ordering::Relaxed);
        report.levels.push(level);
        if low {
            report.stopped = Some(format!("stopped at {n} agents: free memory fell below {min_free:.0} MB"));
            break;
        }
        if failed > opts.max_error_rate {
            report.stopped = Some(format!("stopped at {n} agents: {:.0}% of tasks failed", failed * 100.0));
            break;
        }
        // Fixture folders of this level are no longer needed.
        let _ = std::fs::remove_dir_all(&env.fixtures);
    }
    std::fs::write(opts.out.join("report.json"), serde_json::to_string_pretty(&report)?)?;
    Ok(report)
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "  {:>5} {:>6} {:>5} {:>6} {:>7.1} {:>7.1} {:>8.0} {:>8.0} {:>7.1} {:>7.1} {:>7.1} {:>7.1} {:>6.0} {:>6.1}",
            self.agents,
            self.tasks,
            self.failed,
            self.tool_errors,
            self.tasks_per_s,
            self.steps_per_s,
            self.task_ms.p50,
            self.task_ms.p95,
            self.model_ms.p50,
            self.tool_ms.p50,
            self.tool_ms.p95,
            self.stall_ms.p95,
            self.rss_peak_mb,
            self.mb_per_agent()
        )
    }
}

pub const HEADER: &str = "  agents  tasks  fail toolerr  task/s  step/s  task p50  task p95  model50  tool50  tool95 stall95  rssMB MB/agt";

impl fmt::Display for BenchReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "wang bench — fake model ({} ms/reply), {} model calls + {} tool calls per task, {} task(s) per agent, sandbox {}",
            self.latency_ms,
            self.model_calls_per_task,
            self.model_calls_per_task - 1,
            self.tasks_per_agent,
            if self.sandbox { "on" } else { "off" }
        )?;
        writeln!(f, "  (latencies in ms; MB/agt = system memory used per agent)\n")?;
        writeln!(f, "{HEADER}")?;
        for l in &self.levels {
            writeln!(f, "{l}")?;
        }
        if let Some(best) = self.levels.iter().filter(|l| l.failed == 0 && l.tool_errors == 0).max_by(|a, b| a.steps_per_s.total_cmp(&b.steps_per_s)) {
            writeln!(f, "\n  peak: {:.0} steps/s ({:.0} tasks/s) at {} agents", best.steps_per_s, best.tasks_per_s, best.agents)?;
        }
        if let Some(max_ok) = self.levels.iter().filter(|l| l.failed == 0 && l.tool_errors == 0).map(|l| l.agents).max() {
            writeln!(f, "  most agents at once with no failures: {max_ok}")?;
        }
        for l in self.levels.iter().filter(|l| l.first_error.is_some()) {
            writeln!(f, "  first error at {}: {}", l.agents, l.first_error.as_deref().unwrap_or(""))?;
        }
        if let Some(s) = &self.stopped {
            writeln!(f, "  {s}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;

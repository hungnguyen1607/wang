//! wang-evals: a fixed set of tasks, run against the real agent loop, scored automatically.
//!
//! - `--live` runs the real model and records every model call to a cassette.
//! - replay (default) serves the recorded calls back: $0, deterministic, fast. Use it after
//!   every harness change; re-record with `--live` after prompt or model changes.
//!
//! A task is a TOML file: a prompt, fixture files, and checks (reply contains X, and/or a
//! shell command that must exit 0 in the fixture folder afterwards).

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use wang_daemon::agent::{Agent, Conversation, Limits};
use wang_daemon::config::Config;
use wang_daemon::model::cassette::{Cassette, Recording, Replay};
use wang_daemon::model::{AnyModel, ModelClient};
use wang_daemon::tools::{Sandbox, ToolCtx, WebPolicy};
use wang_jail::{Io, Jail, Policy};
use wang_tokens::Ledger;
use wang_trace::Tracer;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalTask {
    pub id: String,
    pub prompt: String,
    /// Fixture files: relative path → contents.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    #[serde(default)]
    pub check: Check,
    #[serde(default)]
    pub max_steps: Option<u32>,
    /// Network the task's shell commands may use.
    #[serde(default)]
    pub domains: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    /// Every string must appear in the reply (case-insensitive).
    #[serde(default)]
    pub reply_contains: Vec<String>,
    /// None of these may appear in the reply (case-insensitive).
    #[serde(default)]
    pub reply_excludes: Vec<String>,
    /// Shell command run (jailed, read-only) in the fixture folder afterwards; exit 0 = pass.
    #[serde(default)]
    pub command: Option<String>,
}

pub fn load_suite(dir: &Path) -> Result<Vec<EvalTask>> {
    let mut tasks = vec![];
    for e in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let p = e?.path();
        if p.extension().is_some_and(|x| x == "toml") {
            let s = std::fs::read_to_string(&p)?;
            let t: EvalTask = toml::from_str(&s).with_context(|| format!("parsing {}", p.display()))?;
            tasks.push(t);
        }
    }
    tasks.sort_by(|a, b| a.id.cmp(&b.id));
    let mut ids: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    ids.dedup();
    if ids.len() != tasks.len() {
        bail!("duplicate task ids in {}", dir.display());
    }
    Ok(tasks)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    Live,
    Replay,
}

pub struct RunOpts {
    pub mode: Mode,
    pub suite: PathBuf,
    pub cassettes: PathBuf,
    pub out: PathBuf,
    /// Cheap model for routing (None = off).
    pub routing: Option<String>,
    pub only: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub id: String,
    pub passed: bool,
    pub outcome: String,
    pub steps: u32,
    pub cost_usd: f64,
    pub prompt_tokens: u64,
    pub output_tokens: u64,
    pub wall_ms: f64,
    /// Replay only: model calls whose request differed from the recording.
    pub diverged: u32,
    pub failures: Vec<String>,
    pub task_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub started_at: String,
    pub mode: Mode,
    pub model: String,
    pub routing: Option<String>,
    pub results: Vec<TaskResult>,
}

impl Report {
    pub fn passed(&self) -> usize {
        self.results.iter().filter(|r| r.passed).count()
    }
    pub fn cost(&self) -> f64 {
        self.results.iter().map(|r| r.cost_usd).sum()
    }
    pub fn success_rate(&self) -> f64 {
        self.passed() as f64 / self.results.len().max(1) as f64
    }
    pub fn cost_per_success(&self) -> Option<f64> {
        (self.passed() > 0).then(|| self.cost() / self.passed() as f64)
    }
    pub fn avg_steps(&self) -> f64 {
        self.results.iter().map(|r| r.steps as f64).sum::<f64>() / self.results.len().max(1) as f64
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
    pub fn load(path: &Path) -> Result<Self> {
        Ok(serde_json::from_str(&std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?)?)
    }
}

fn cassette_path(dir: &Path, id: &str, routing: &Option<String>) -> PathBuf {
    match routing {
        Some(_) => dir.join(format!("{id}.routed.json")),
        None => dir.join(format!("{id}.json")),
    }
}

fn write_fixture(dir: &Path, files: &BTreeMap<String, String>) -> Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    std::fs::create_dir_all(dir)?;
    for (rel, body) in files {
        if rel.contains("..") || rel.starts_with('/') {
            bail!("fixture path must be relative: {rel}");
        }
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, body)?;
    }
    Ok(())
}

struct Env {
    cfg: Config,
    jail: Arc<Jail>,
    ledger: Arc<Ledger>,
    tracer: Arc<Tracer>,
}

async fn run_one<C: ModelClient>(env: &Env, t: &EvalTask, fixture: &Path, client: C) -> Result<(TaskResult, C)> {
    write_fixture(fixture, &t.files)?;
    let policy = Policy {
        read: vec![fixture.to_path_buf()],
        write: vec![fixture.to_path_buf()],
        domains: t.domains.clone(),
        limits: env.cfg.sandbox.policy()?.limits,
        ..Default::default()
    };
    let web = WebPolicy { domains: env.cfg.web_fetch.domains.clone(), allow_private: false };
    let ctx = ToolCtx::with(fixture.to_path_buf(), Sandbox::Jailed { jail: env.jail.clone(), policy }, web);
    let limits = Limits {
        max_steps: t.max_steps.unwrap_or(env.cfg.agent.max_steps),
        tool_output_cap: env.cfg.agent.tool_output_cap,
        loop_fail_limit: env.cfg.agent.loop_fail_limit.max(1),
        loop_repeat_limit: env.cfg.agent.loop_repeat_limit.max(2),
        budget_usd: 0.0,
        ..Default::default()
    };
    // No approvals in evals: privileged commands run with the fixture's write access.
    let agent = Agent::new(client, env.ledger.clone(), env.tracer.clone(), ctx, limits);
    let mut conv = Conversation::new(format!("eval-{}", t.id));
    let started = Instant::now();
    let turn = agent.run_turn(&mut conv, "eval", &t.prompt, &|_| {}).await;
    let wall_ms = started.elapsed().as_secs_f64() * 1e3;

    let mut failures = vec![];
    let (reply, outcome, steps, cost, pt, ot, task_id) = match &turn {
        Ok(r) => (r.reply.clone(), r.outcome.clone(), r.calls, r.cost_usd, r.usage.prompt_tokens(), r.usage.output_tokens, Some(r.task_id)),
        Err(e) => {
            failures.push(format!("error: {e:#}"));
            (String::new(), "error".to_string(), 0, 0.0, 0, 0, None)
        }
    };
    if outcome != "done" && turn.is_ok() {
        failures.push(format!("outcome {outcome}"));
    }
    let lower = reply.to_lowercase();
    for want in &t.check.reply_contains {
        if !lower.contains(&want.to_lowercase()) {
            failures.push(format!("reply lacks {want:?}"));
        }
    }
    for bad in &t.check.reply_excludes {
        if lower.contains(&bad.to_lowercase()) {
            failures.push(format!("reply contains {bad:?}"));
        }
    }
    if let Some(cmd) = &t.check.command {
        let pol = Policy { read: vec![fixture.to_path_buf()], cwd: Some(fixture.to_path_buf()), ..Default::default() };
        let r = env.jail.run(&pol, &["bash".into(), "-c".into(), cmd.clone()], Io::Capture).await?;
        if r.exit_code != 0 {
            let err = String::from_utf8_lossy(&r.stderr);
            failures.push(format!("check command failed (exit {}){}", r.exit_code, if err.trim().is_empty() { String::new() } else { format!(": {}", err.trim()) }));
        }
    }
    let result = TaskResult {
        id: t.id.clone(),
        passed: failures.is_empty(),
        outcome,
        steps,
        cost_usd: cost,
        prompt_tokens: pt,
        output_tokens: ot,
        wall_ms,
        diverged: 0,
        failures,
        task_id,
    };
    Ok((result, agent.client))
}

pub async fn run(cfg: Config, opts: &RunOpts) -> Result<Report> {
    let mut tasks = load_suite(&opts.suite)?;
    if let Some(only) = &opts.only {
        tasks.retain(|t| &t.id == only);
        if tasks.is_empty() {
            bail!("no task with id {only}");
        }
    }
    let jail = Arc::new(Jail::new()?);
    jail.check().await?;
    std::fs::create_dir_all(&opts.out)?;
    let db = opts.out.join("run.db");
    let env = Env { ledger: Arc::new(Ledger::open(&db)?), tracer: Arc::new(Tracer::open(&db)?), jail, cfg };
    let model = match &opts.routing {
        Some(cheap) => format!("{} (routing → {cheap})", env.cfg.model.model_id()),
        None => env.cfg.model.model_id(),
    };
    let mut results = vec![];
    for t in &tasks {
        let fixture = opts.out.join("fixtures").join(&t.id);
        let cas_path = cassette_path(&opts.cassettes, &t.id, &opts.routing);
        let r = match opts.mode {
            Mode::Live => {
                let client = Recording::new(AnyModel::from_config_routed(&env.cfg.model, opts.routing.as_deref())?);
                let (r, client) = run_one(&env, t, &fixture, client).await?;
                client.cassette().save(&cas_path)?;
                r
            }
            Mode::Replay => match Cassette::load(&cas_path) {
                Ok(c) => {
                    let (mut r, client) = run_one(&env, t, &fixture, Replay::new(c)).await?;
                    r.diverged = client.diverged();
                    r
                }
                Err(_) => TaskResult {
                    id: t.id.clone(),
                    passed: false,
                    outcome: "not_recorded".into(),
                    steps: 0,
                    cost_usd: 0.0,
                    prompt_tokens: 0,
                    output_tokens: 0,
                    wall_ms: 0.0,
                    diverged: 0,
                    failures: vec![format!("no cassette at {}: record it with `wang eval --live`", cas_path.display())],
                    task_id: None,
                },
            },
        };
        results.push(r);
    }
    let report = Report {
        started_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        mode: opts.mode,
        model,
        routing: opts.routing.clone(),
        results,
    };
    report.save(&opts.out.join("report.json"))?;
    Ok(report)
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode = match self.mode {
            Mode::Live => "live (recorded)",
            Mode::Replay => "replay",
        };
        writeln!(f, "wang eval — {mode} · {} · {} tasks", self.model, self.results.len())?;
        writeln!(f)?;
        writeln!(f, "  {:<24} {:<6} {:>5} {:>9} {:>8}  notes", "task", "result", "steps", "cost", "time")?;
        for r in &self.results {
            let mut notes = r.failures.join("; ");
            if r.diverged > 0 {
                if !notes.is_empty() {
                    notes += "; ";
                }
                notes += &format!("{} call(s) diverged from the recording", r.diverged);
            }
            writeln!(
                f,
                "  {:<24} {:<6} {:>5} {:>9} {:>7.1}s  {}",
                r.id,
                if r.passed { "PASS" } else { "FAIL" },
                r.steps,
                format!("${:.4}", r.cost_usd),
                r.wall_ms / 1e3,
                notes
            )?;
        }
        writeln!(f)?;
        writeln!(
            f,
            "  success {}/{} ({:.0}%) · cost ${:.4} · per success {} · avg steps {:.1} · time {:.1}s",
            self.passed(),
            self.results.len(),
            self.success_rate() * 100.0,
            self.cost(),
            self.cost_per_success().map(|c| format!("${c:.4}")).unwrap_or_else(|| "-".into()),
            self.avg_steps(),
            self.results.iter().map(|r| r.wall_ms).sum::<f64>() / 1e3
        )
    }
}

pub struct Comparison {
    pub regressions: Vec<String>,
    pub fixes: Vec<String>,
    pub text: String,
}

/// Compare two runs task by task. Regressions = passed before, failing now.
pub fn compare(prev: &Report, cur: &Report) -> Comparison {
    let before: BTreeMap<&str, &TaskResult> = prev.results.iter().map(|r| (r.id.as_str(), r)).collect();
    let mut regressions = vec![];
    let mut fixes = vec![];
    for r in &cur.results {
        if let Some(b) = before.get(r.id.as_str()) {
            if b.passed && !r.passed {
                regressions.push(r.id.clone());
            }
            if !b.passed && r.passed {
                fixes.push(r.id.clone());
            }
        }
    }
    let pct = |a: f64, b: f64| if a > 0.0 { format!(" ({:+.0}%)", (b - a) / a * 100.0) } else { String::new() };
    let mut text = format!(
        "vs {} ({}): success {}/{} → {}/{} · cost ${:.4} → ${:.4}{} · avg steps {:.1} → {:.1}\n",
        prev.started_at,
        prev.model,
        prev.passed(),
        prev.results.len(),
        cur.passed(),
        cur.results.len(),
        prev.cost(),
        cur.cost(),
        pct(prev.cost(), cur.cost()),
        prev.avg_steps(),
        cur.avg_steps()
    );
    text += &format!(
        "  regressions: {}\n  fixed: {}\n",
        if regressions.is_empty() { "none".into() } else { regressions.join(", ") },
        if fixes.is_empty() { "none".into() } else { fixes.join(", ") }
    );
    Comparison { regressions, fixes, text }
}

/// Latest saved run under `runs` (directories are named by start time), excluding `skip`.
pub fn latest_run(runs: &Path, skip: &Path) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(runs)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("report.json").exists() && p != skip)
        .collect();
    dirs.sort();
    dirs.pop()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suite_parses_and_rejects_typos() {
        let dir = std::env::temp_dir().join(format!("wang-suite-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.toml"),
            "id = \"count\"\nprompt = \"how many lines?\"\n[files]\n\"n.txt\" = \"a\\nb\\n\"\n[check]\nreply_contains = [\"2\"]\n",
        )
        .unwrap();
        let s = load_suite(&dir).unwrap();
        assert_eq!(s[0].files["n.txt"], "a\nb\n");
        assert_eq!(s[0].check.reply_contains, vec!["2"]);
        std::fs::write(dir.join("b.toml"), "id = \"x\"\nprompt = \"p\"\n[check]\nreply_contain = [\"2\"]\n").unwrap();
        assert!(load_suite(&dir).is_err());
        std::fs::remove_dir_all(dir).ok();
        assert!(write_fixture(&std::env::temp_dir().join("wang-fx-bad"), &BTreeMap::from([("../x".to_string(), String::new())])).is_err());
    }

    fn result(id: &str, passed: bool, cost: f64) -> TaskResult {
        TaskResult { id: id.into(), passed, outcome: "done".into(), steps: 2, cost_usd: cost, prompt_tokens: 0, output_tokens: 0, wall_ms: 0.0, diverged: 0, failures: vec![], task_id: None }
    }

    #[test]
    fn compare_finds_regressions() {
        let prev = Report { started_at: "t0".into(), mode: Mode::Replay, model: "m".into(), routing: None, results: vec![result("a", true, 0.02), result("b", false, 0.02)] };
        let cur = Report { started_at: "t1".into(), mode: Mode::Replay, model: "m".into(), routing: None, results: vec![result("a", false, 0.01), result("b", true, 0.01)] };
        let c = compare(&prev, &cur);
        assert_eq!(c.regressions, vec!["a"]);
        assert_eq!(c.fixes, vec!["b"]);
        assert!(c.text.contains("(-50%)"), "{}", c.text);
    }
}

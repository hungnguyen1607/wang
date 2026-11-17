//! wang-evals: a fixed set of tasks, run against the real agent loop, scored automatically.
//!
//! - `--live` runs the real model and records every model call to a cassette.
//! - replay (default) serves the recorded calls back: $0, deterministic, fast. Use it after
//!   every harness change; re-record with `--live` after prompt or model changes.
//!
//! - Arms compare the harness against itself: `wang` (as shipped), `plain` (same model, tools and
//!   jail, but no trimming, dedupe, loop detection, workflow guidance or cache hints) and `plan`
//!   (forced multi-agent orchestration). `--trials N` repeats each run, since live models vary.
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
use wang_daemon::config::{Config, ModelCfg};
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
    /// Shell command run (jailed, no network) in the fixture folder before the agent starts,
    /// for fixtures too big to write inline (e.g. `seq 1 200000 > big.log`).
    #[serde(default)]
    pub setup: Option<String>,
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

/// Which harness configuration a run uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Arm {
    /// wang as shipped: the workflow classifier picks the loop or orchestration.
    #[default]
    Wang,
    /// Baseline: a plain agent loop. Same model, tools and sandbox, none of the harness optimizations.
    Plain,
    /// Every task goes through the planner → workers → checker → writer pipeline.
    Plan,
}

impl Arm {
    pub fn name(self) -> &'static str {
        match self {
            Arm::Wang => "wang",
            Arm::Plain => "plain",
            Arm::Plan => "plan",
        }
    }

    /// The message the agent gets for this arm.
    pub fn prompt(self, prompt: &str) -> String {
        match self {
            Arm::Wang => prompt.to_string(),
            // /general: no workflow guidance and no tool gating.
            Arm::Plain => format!("/general {prompt}"),
            Arm::Plan => format!("/plan {prompt}"),
        }
    }

    /// Agent limits for this arm, from the configured ones.
    pub fn limits(self, base: Limits) -> Limits {
        match self {
            Arm::Plain => Limits {
                tool_output_cap: usize::MAX,
                loop_fail_limit: u32::MAX,
                loop_repeat_limit: u32::MAX,
                compact_at_tokens: 0,
                dedupe: false,
                ..base
            },
            _ => base,
        }
    }

    /// Model settings for this arm.
    pub fn model_cfg(self, base: &ModelCfg) -> ModelCfg {
        ModelCfg { cache_hints: self != Arm::Plain && base.cache_hints, ..base.clone() }
    }
}

impl std::str::FromStr for Arm {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.trim() {
            "wang" => Arm::Wang,
            "plain" => Arm::Plain,
            "plan" => Arm::Plan,
            other => bail!("unknown arm {other:?} (use wang, plain, plan)"),
        })
    }
}

impl fmt::Display for Arm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
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
    /// Harness configurations to run (default: just `wang`).
    pub arms: Vec<Arm>,
    /// Runs per task and arm (live models are nondeterministic).
    pub trials: u32,
    /// Live mode: stop starting runs once this much has been spent (None = no cap).
    pub max_usd: Option<f64>,
}

impl Default for RunOpts {
    fn default() -> Self {
        Self {
            mode: Mode::Replay,
            suite: PathBuf::from("evals/tasks"),
            cassettes: PathBuf::from("evals/cassettes"),
            out: PathBuf::from("evals/runs/out"),
            routing: None,
            only: None,
            arms: vec![Arm::Wang],
            trials: 1,
            max_usd: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub id: String,
    #[serde(default)]
    pub arm: Arm,
    #[serde(default = "one")]
    pub trial: u32,
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

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub started_at: String,
    pub mode: Mode,
    pub model: String,
    pub routing: Option<String>,
    pub results: Vec<TaskResult>,
}

/// Aggregates over a set of results (a whole report, or one arm of it).
pub struct Stats<'a>(pub Vec<&'a TaskResult>);

impl Stats<'_> {
    pub fn runs(&self) -> usize {
        self.0.len()
    }
    pub fn passed(&self) -> usize {
        self.0.iter().filter(|r| r.passed).count()
    }
    pub fn cost(&self) -> f64 {
        self.0.iter().map(|r| r.cost_usd).sum()
    }
    pub fn success_rate(&self) -> f64 {
        self.passed() as f64 / self.runs().max(1) as f64
    }
    pub fn cost_per_success(&self) -> Option<f64> {
        (self.passed() > 0).then(|| self.cost() / self.passed() as f64)
    }
    fn avg(&self, f: impl Fn(&TaskResult) -> f64) -> f64 {
        self.0.iter().map(|r| f(r)).sum::<f64>() / self.runs().max(1) as f64
    }
    pub fn avg_steps(&self) -> f64 {
        self.avg(|r| r.steps as f64)
    }
    pub fn avg_prompt_tokens(&self) -> f64 {
        self.avg(|r| r.prompt_tokens as f64)
    }
    pub fn avg_secs(&self) -> f64 {
        self.avg(|r| r.wall_ms / 1e3)
    }
    /// Lowest and highest success rate across trials.
    pub fn trial_spread(&self) -> (f64, f64) {
        let mut by: BTreeMap<u32, (usize, usize)> = BTreeMap::new();
        for r in &self.0 {
            let e = by.entry(r.trial).or_default();
            e.0 += r.passed as usize;
            e.1 += 1;
        }
        let rates: Vec<f64> = by.values().map(|(p, n)| *p as f64 / (*n).max(1) as f64).collect();
        let lo = rates.iter().cloned().fold(f64::INFINITY, f64::min);
        let hi = rates.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if rates.is_empty() { (0.0, 0.0) } else { (lo, hi) }
    }
}

impl Report {
    pub fn all(&self) -> Stats<'_> {
        Stats(self.results.iter().collect())
    }
    pub fn arm(&self, arm: Arm) -> Stats<'_> {
        Stats(self.results.iter().filter(|r| r.arm == arm).collect())
    }
    /// Arms present, in order.
    pub fn arms(&self) -> Vec<Arm> {
        let mut a: Vec<Arm> = self.results.iter().map(|r| r.arm).collect();
        a.sort();
        a.dedup();
        a
    }
    pub fn passed(&self) -> usize {
        self.all().passed()
    }
    pub fn cost(&self) -> f64 {
        self.all().cost()
    }
    pub fn success_rate(&self) -> f64 {
        self.all().success_rate()
    }
    pub fn cost_per_success(&self) -> Option<f64> {
        self.all().cost_per_success()
    }
    pub fn avg_steps(&self) -> f64 {
        self.all().avg_steps()
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

fn cassette_path(dir: &Path, id: &str, arm: Arm, routing: &Option<String>, trial: u32) -> PathBuf {
    let routed = if routing.is_some() { ".routed" } else { "" };
    dir.join(format!("{id}.{arm}{routed}.t{trial}.json"))
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

async fn run_one<C: ModelClient>(env: &Env, t: &EvalTask, arm: Arm, trial: u32, fixture: &Path, client: C) -> Result<(TaskResult, C)> {
    write_fixture(fixture, &t.files)?;
    if let Some(cmd) = &t.setup {
        let pol = Policy { read: vec![fixture.to_path_buf()], write: vec![fixture.to_path_buf()], cwd: Some(fixture.to_path_buf()), ..Default::default() };
        let r = env.jail.run(&pol, &["bash".into(), "-c".into(), cmd.clone()], Io::Capture).await?;
        if r.exit_code != 0 {
            bail!("{}: setup failed (exit {}): {}", t.id, r.exit_code, String::from_utf8_lossy(&r.stderr).trim());
        }
    }
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
    let limits = arm.limits(limits);
    // No approvals in evals: privileged commands run with the fixture's write access.
    let agent = Agent::new(client, env.ledger.clone(), env.tracer.clone(), ctx, limits);
    let mut conv = Conversation::new(format!("eval-{}-{arm}-t{trial}", t.id));
    let started = Instant::now();
    let turn = agent.run_turn(&mut conv, "eval", &arm.prompt(&t.prompt), &|_| {}).await;
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
        arm,
        trial,
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
    let mut spent = 0.0;
    for trial in 1..=opts.trials.max(1) {
        for &arm in &opts.arms {
            for t in &tasks {
                let fixture = opts.out.join("fixtures").join(format!("{}.{arm}.t{trial}", t.id));
                let cas_path = cassette_path(&opts.cassettes, &t.id, arm, &opts.routing, trial);
                let skip = |outcome: &str, why: String| TaskResult {
                    id: t.id.clone(),
                    arm,
                    trial,
                    passed: false,
                    outcome: outcome.into(),
                    steps: 0,
                    cost_usd: 0.0,
                    prompt_tokens: 0,
                    output_tokens: 0,
                    wall_ms: 0.0,
                    diverged: 0,
                    failures: vec![why],
                    task_id: None,
                };
                let r = match opts.mode {
                    Mode::Live if opts.max_usd.is_some_and(|cap| spent >= cap) => {
                        skip("over_budget", format!("not run: ${spent:.4} spent, cap ${:.4}", opts.max_usd.unwrap_or_default()))
                    }
                    Mode::Live => {
                        let client = Recording::new(AnyModel::from_config_routed(&arm.model_cfg(&env.cfg.model), opts.routing.as_deref())?);
                        let (r, client) = run_one(&env, t, arm, trial, &fixture, client).await?;
                        client.cassette().save(&cas_path)?;
                        r
                    }
                    Mode::Replay => match Cassette::load(&cas_path) {
                        Ok(c) => {
                            let (mut r, client) = run_one(&env, t, arm, trial, &fixture, Replay::new(c)).await?;
                            r.diverged = client.diverged();
                            r
                        }
                        Err(_) => skip("not_recorded", format!("no cassette at {}: record it with `wang eval --live`", cas_path.display())),
                    },
                };
                spent += r.cost_usd;
                results.push(r);
            }
        }
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
        let arms = self.arms();
        let multi = arms.len() > 1 || self.results.iter().any(|r| r.trial > 1);
        writeln!(f, "wang eval — {mode} · {} · {} runs", self.model, self.results.len())?;
        writeln!(f)?;
        writeln!(f, "  {:<24} {:<10} {:<6} {:>5} {:>9} {:>8}  notes", "task", "arm", "result", "steps", "cost", "time")?;
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
                "  {:<24} {:<10} {:<6} {:>5} {:>9} {:>7.1}s  {}",
                r.id,
                format!("{}/t{}", r.arm, r.trial),
                if r.passed { "PASS" } else { "FAIL" },
                r.steps,
                format!("${:.4}", r.cost_usd),
                r.wall_ms / 1e3,
                notes
            )?;
        }
        writeln!(f)?;
        if multi {
            write!(f, "{}", arm_table(self))?;
        } else {
            let a = self.all();
            writeln!(
                f,
                "  success {}/{} ({:.0}%) · cost ${:.4} · per success {} · avg steps {:.1} · time {:.1}s",
                a.passed(),
                a.runs(),
                a.success_rate() * 100.0,
                a.cost(),
                a.cost_per_success().map(|c| format!("${c:.4}")).unwrap_or_else(|| "-".into()),
                a.avg_steps(),
                a.avg_secs() * a.runs() as f64
            )?;
        }
        Ok(())
    }
}

/// Per-arm summary, plus per-task wins and losses against the `plain` baseline.
pub fn arm_table(report: &Report) -> String {
    let mut out = format!(
        "  {:<6} {:>9} {:>11} {:>11} {:>14} {:>7} {:>11} {:>7}\n",
        "arm", "success", "trials", "cost", "per success", "steps", "prompt tok", "time"
    );
    for arm in report.arms() {
        let s = report.arm(arm);
        let (lo, hi) = s.trial_spread();
        out += &format!(
            "  {:<6} {:>4}/{:<4} {:>11} {:>11} {:>14} {:>7.1} {:>11.0} {:>6.1}s\n",
            arm.name(),
            s.passed(),
            s.runs(),
            format!("{:.0}–{:.0}%", lo * 100.0, hi * 100.0),
            format!("${:.4}", s.cost()),
            s.cost_per_success().map(|c| format!("${c:.4}")).unwrap_or_else(|| "-".into()),
            s.avg_steps(),
            s.avg_prompt_tokens(),
            s.avg_secs()
        );
    }
    let base = report.arm(Arm::Plain);
    if base.runs() > 0 {
        let rate = |st: &Stats, id: &str| {
            let v: Vec<&&TaskResult> = st.0.iter().filter(|r| r.id == id).collect();
            v.iter().filter(|r| r.passed).count() as f64 / v.len().max(1) as f64
        };
        let ids: std::collections::BTreeSet<&str> = base.0.iter().map(|r| r.id.as_str()).collect();
        for arm in report.arms().into_iter().filter(|a| *a != Arm::Plain) {
            let s = report.arm(arm);
            let (mut wins, mut losses) = (vec![], vec![]);
            for id in &ids {
                let (a, b) = (rate(&s, id), rate(&base, id));
                if a > b {
                    wins.push(*id);
                } else if a < b {
                    losses.push(*id);
                }
            }
            let cost = match (s.cost_per_success(), base.cost_per_success()) {
                (Some(a), Some(b)) if b > 0.0 => format!(" · cost per success {:+.0}%", (a - b) / b * 100.0),
                _ => String::new(),
            };
            out += &format!(
                "\n  {arm} vs plain: success {:+.0} pts{cost}\n    better on: {}\n    worse on:  {}\n",
                (s.success_rate() - base.success_rate()) * 100.0,
                if wins.is_empty() { "none".into() } else { wins.join(", ") },
                if losses.is_empty() { "none".into() } else { losses.join(", ") }
            );
        }
    }
    out
}

pub struct Comparison {
    pub regressions: Vec<String>,
    pub fixes: Vec<String>,
    pub text: String,
}

/// Compare two runs per (task, arm, trial). Regressions = passed before, failing now.
pub fn compare(prev: &Report, cur: &Report) -> Comparison {
    let key = |r: &TaskResult| {
        let base = if r.arm == Arm::Wang { r.id.clone() } else { format!("{}[{}]", r.id, r.arm) };
        if r.trial > 1 { format!("{base}#t{}", r.trial) } else { base }
    };
    let before: BTreeMap<String, &TaskResult> = prev.results.iter().map(|r| (key(r), r)).collect();
    let mut regressions = vec![];
    let mut fixes = vec![];
    for r in &cur.results {
        if let Some(b) = before.get(&key(r)) {
            if b.passed && !r.passed {
                regressions.push(key(r));
            }
            if !b.passed && r.passed {
                fixes.push(key(r));
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

    #[test]
    fn arms_parse_and_configure() {
        assert_eq!("plain".parse::<Arm>().unwrap(), Arm::Plain);
        assert!("plainn".parse::<Arm>().is_err());
        assert_eq!(Arm::Plain.prompt("hi"), "/general hi");
        assert_eq!(Arm::Plan.prompt("hi"), "/plan hi");
        assert_eq!(Arm::Wang.prompt("hi"), "hi");
        let l = Arm::Plain.limits(Limits::default());
        assert!(!l.dedupe && l.tool_output_cap == usize::MAX && l.loop_repeat_limit == u32::MAX);
        assert!(Arm::Wang.limits(Limits::default()).dedupe);
        assert!(!Arm::Plain.model_cfg(&ModelCfg::default()).cache_hints);
        assert!(Arm::Plan.model_cfg(&ModelCfg::default()).cache_hints);
        let d = Path::new("c");
        assert_eq!(cassette_path(d, "x", Arm::Plain, &None, 2), d.join("x.plain.t2.json"));
        assert_eq!(cassette_path(d, "x", Arm::Wang, &Some("m".into()), 1), d.join("x.wang.routed.t1.json"));
    }

    #[test]
    fn old_reports_still_load() {
        let r: TaskResult = serde_json::from_str(r#"{"id":"a","passed":true,"outcome":"done","steps":1,"cost_usd":0.0,"prompt_tokens":0,"output_tokens":0,"wall_ms":0.0,"diverged":0,"failures":[],"task_id":null}"#).unwrap();
        assert_eq!((r.arm, r.trial), (Arm::Wang, 1));
    }

    #[test]
    fn arm_table_compares_with_plain() {
        let mk = |id: &str, arm, passed, cost| TaskResult { arm, ..result(id, passed, cost) };
        let rep = Report {
            started_at: "t".into(),
            mode: Mode::Live,
            model: "m".into(),
            routing: None,
            results: vec![mk("a", Arm::Wang, true, 0.01), mk("b", Arm::Wang, true, 0.01), mk("a", Arm::Plain, true, 0.04), mk("b", Arm::Plain, false, 0.04)],
        };
        let t = arm_table(&rep);
        assert!(t.contains("wang vs plain: success +50 pts · cost per success -88%"), "{t}");
        assert!(t.contains("better on: b"), "{t}");
    }

    fn result(id: &str, passed: bool, cost: f64) -> TaskResult {
        TaskResult { id: id.into(), arm: Arm::Wang, trial: 1, passed, outcome: "done".into(), steps: 2, cost_usd: cost, prompt_tokens: 0, output_tokens: 0, wall_ms: 0.0, diverged: 0, failures: vec![], task_id: None }
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

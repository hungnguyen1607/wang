//! `wang stats`: agent health from the ledger.
//!
//! Success = the task finished (`done`) and you didn't rate it 👎.
//! Cost per successful task = *all* spend in the period / successful tasks, because failed
//! attempts are part of what success costs.

use std::fmt;

use anyhow::Result;
use rusqlite::params;

use crate::ledger::{Ledger, fmt_tokens};

#[derive(Debug, Clone, Default)]
pub struct ToolHealth {
    pub name: String,
    pub calls: u64,
    pub errors: u64,
    pub avg_ms: f64,
    pub repeats: u64,
    pub trimmed_bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub struct FailingCall {
    pub tool: String,
    pub detail: String,
    pub failures: u64,
    pub last_error: String,
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub label: String,
    pub tasks: u64,
    pub successes: u64,
    /// (outcome, count), most common first.
    pub outcomes: Vec<(String, u64)>,
    pub rated_good: u64,
    pub rated_bad: u64,
    pub cost_usd: f64,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub model_calls: u64,
    pub avg_latency_ms: f64,
    pub avg_ttft_ms: Option<f64>,
    pub avg_steps: f64,
    pub tool_calls: u64,
    pub tool_errors: u64,
    pub tools: Vec<ToolHealth>,
    pub failing_calls: Vec<FailingCall>,
    pub sandbox: Option<SandboxStats>,
    pub approvals: Option<ApprovalStats>,
}

#[derive(Debug, Clone, Default)]
pub struct ApprovalStats {
    pub requests: u64,
    pub approved: u64,
    pub denied: u64,
    pub timed_out: u64,
    pub avg_wait_ms: f64,
}

/// From wang-trace spans in the same database (absent if there are none).
#[derive(Debug, Clone, Default)]
pub struct SandboxStats {
    pub jailed_calls: u64,
    pub setup_ms_avg: f64,
    pub wall_ms_avg: f64,
    pub peak_memory_max: u64,
    pub oom_kills: u64,
    pub timeouts: u64,
    pub fork_limit_hits: u64,
    pub blocked_requests: u64,
}

impl Stats {
    pub fn success_rate(&self) -> Option<f64> {
        (self.tasks > 0).then(|| self.successes as f64 / self.tasks as f64)
    }
    pub fn cost_per_success(&self) -> Option<f64> {
        (self.successes > 0).then(|| self.cost_usd / self.successes as f64)
    }
}

impl Ledger {
    /// Health over the last `days` local calendar days; `None` = all time.
    pub fn stats(&self, days: Option<u32>, label: &str) -> Result<Stats> {
        let since = format!("-{} days", days.map(|d| d.max(1) - 1).unwrap_or(100_000));
        // Agent health is about wang's own tasks: proxied requests and orchestrator workers (parts of a
        // planned task) aren't tasks of their own.
        let f = "date(t.started_at, 'localtime') >= date('now', 'localtime', ?1) AND t.source NOT LIKE 'proxy:%' AND t.source NOT LIKE 'worker:%'";
        let c = self.conn.lock().unwrap();
        let mut s = Stats { label: label.into(), ..Default::default() };

        let mut stmt = c.prepare(&format!(
            "SELECT t.status, COUNT(*) FROM tasks t WHERE {f} GROUP BY t.status ORDER BY COUNT(*) DESC, t.status"
        ))?;
        s.outcomes = stmt
            .query_map(params![since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?
            .collect::<rusqlite::Result<_>>()?;
        s.tasks = s.outcomes.iter().map(|(_, n)| n).sum();

        (s.successes, s.rated_good, s.rated_bad, s.avg_steps) = c.query_row(
            &format!(
                "SELECT COALESCE(SUM(t.status = 'done' AND COALESCE(t.feedback, '') != 'bad'), 0),
                        COALESCE(SUM(t.feedback = 'good'), 0), COALESCE(SUM(t.feedback = 'bad'), 0),
                        COALESCE(AVG(t.steps), 0)
                 FROM tasks t WHERE {f}"
            ),
            params![since],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)? as u64, r.get(3)?)),
        )?;

        (s.model_calls, s.cost_usd, s.prompt_tokens, s.cached_tokens, s.avg_latency_ms, s.avg_ttft_ms) = c.query_row(
            &format!(
                "SELECT COUNT(*), COALESCE(SUM(k.cost_usd), 0),
                        COALESCE(SUM(k.input_tokens + k.cache_write_tokens + k.cache_read_tokens), 0),
                        COALESCE(SUM(k.cache_read_tokens), 0), COALESCE(AVG(k.latency_ms), 0), AVG(k.ttft_ms)
                 FROM calls k JOIN tasks t ON t.id = k.task_id WHERE {f}"
            ),
            params![since],
            |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get(1)?,
                    r.get::<_, i64>(2)? as u64,
                    r.get::<_, i64>(3)? as u64,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )?;

        let mut stmt = c.prepare(&format!(
            "SELECT x.name, COUNT(*), SUM(x.is_error), AVG(x.ms), SUM(x.repeated), SUM(x.raw_bytes - x.sent_bytes)
             FROM tool_calls x JOIN tasks t ON t.id = x.task_id WHERE {f}
             GROUP BY x.name ORDER BY SUM(x.is_error) DESC, COUNT(*) DESC"
        ))?;
        s.tools = stmt
            .query_map(params![since], |r| {
                Ok(ToolHealth {
                    name: r.get(0)?,
                    calls: r.get::<_, i64>(1)? as u64,
                    errors: r.get::<_, i64>(2)? as u64,
                    avg_ms: r.get(3)?,
                    repeats: r.get::<_, i64>(4)? as u64,
                    trimmed_bytes: r.get::<_, i64>(5)? as u64,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        s.tool_calls = s.tools.iter().map(|t| t.calls).sum();
        s.tool_errors = s.tools.iter().map(|t| t.errors).sum();

        let mut stmt = c.prepare(&format!(
            "SELECT x.name, x.detail, COUNT(*),
                    (SELECT y.error FROM tool_calls y WHERE y.name = x.name AND y.detail = x.detail AND y.is_error
                     ORDER BY y.id DESC LIMIT 1)
             FROM tool_calls x JOIN tasks t ON t.id = x.task_id WHERE {f} AND x.is_error
             GROUP BY x.name, x.detail ORDER BY COUNT(*) DESC, MAX(x.id) DESC LIMIT 5"
        ))?;
        s.failing_calls = stmt
            .query_map(params![since], |r| {
                Ok(FailingCall {
                    tool: r.get(0)?,
                    detail: r.get(1)?,
                    failures: r.get::<_, i64>(2)? as u64,
                    last_error: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                })
            })?
            .collect::<rusqlite::Result<_>>()?;

        let has_spans: bool = c
            .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'spans'")?
            .exists([])?;
        if has_spans {
            let j = |k: &str| format!("json_extract(sp.attrs, '$.\"wang.sandbox.{k}\"')");
            let row = c.query_row(
                &format!(
                    "SELECT COUNT(*), COALESCE(AVG({setup}), 0), COALESCE(AVG({wall}), 0), COALESCE(MAX({peak}), 0),
                            COALESCE(SUM({oom} = 1), 0), COALESCE(SUM({to} = 1), 0), COALESCE(SUM({pids} > 0), 0),
                            COALESCE(SUM(json_array_length({blocked})), 0)
                     FROM spans sp JOIN tasks t ON t.id = sp.task_id
                     WHERE sp.kind = 'tool_call' AND {setup} IS NOT NULL AND {f}",
                    setup = j("setup_ms"), wall = j("wall_ms"), peak = j("peak_memory_bytes"), oom = j("oom_killed"),
                    to = j("timed_out"), pids = j("pids_limit_hits"), blocked = j("blocked"),
                ),
                params![since],
                |r| {
                    Ok(SandboxStats {
                        jailed_calls: r.get::<_, i64>(0)? as u64,
                        setup_ms_avg: r.get(1)?,
                        wall_ms_avg: r.get(2)?,
                        peak_memory_max: r.get::<_, i64>(3)? as u64,
                        oom_kills: r.get::<_, i64>(4)? as u64,
                        timeouts: r.get::<_, i64>(5)? as u64,
                        fork_limit_hits: r.get::<_, i64>(6)? as u64,
                        blocked_requests: r.get::<_, i64>(7)? as u64,
                    })
                },
            )?;
            if row.jailed_calls > 0 {
                s.sandbox = Some(row);
            }
            let d = "json_extract(sp.attrs, '$.\"wang.approval.decision\"')";
            let a = c.query_row(
                &format!(
                    "SELECT COUNT(*), COALESCE(SUM({d} = 'approved'), 0), COALESCE(SUM({d} = 'denied'), 0),
                            COALESCE(SUM({d} = 'timed_out'), 0),
                            COALESCE(AVG(json_extract(sp.attrs, '$.\"wang.approval.wait_ms\"')), 0)
                     FROM spans sp JOIN tasks t ON t.id = sp.task_id WHERE sp.kind = 'approval' AND {f}"
                ),
                params![since],
                |r| {
                    Ok(ApprovalStats {
                        requests: r.get::<_, i64>(0)? as u64,
                        approved: r.get::<_, i64>(1)? as u64,
                        denied: r.get::<_, i64>(2)? as u64,
                        timed_out: r.get::<_, i64>(3)? as u64,
                        avg_wait_ms: r.get(4)?,
                    })
                },
            )?;
            if a.requests > 0 {
                s.approvals = Some(a);
            }
        }
        Ok(s)
    }
}

fn pct(x: f64) -> String {
    format!("{:.0}%", x * 100.0)
}

fn one_line(s: &str, n: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > n { format!("{}…", s.chars().take(n).collect::<String>()) } else { s }
}

impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "wang stats — {}", self.label)?;
        writeln!(f)?;
        if self.tasks == 0 {
            return writeln!(f, "  no tasks yet");
        }
        writeln!(
            f,
            "  success rate        {}  ({} of {} tasks)",
            self.success_rate().map(pct).unwrap_or_else(|| "-".into()),
            self.successes,
            self.tasks
        )?;
        let outcomes: Vec<String> = self.outcomes.iter().map(|(o, n)| format!("{o} {n}")).collect();
        writeln!(f, "  outcomes            {}", outcomes.join(" · "))?;
        writeln!(
            f,
            "  your ratings        👍 {}  👎 {}  unrated {}",
            self.rated_good,
            self.rated_bad,
            self.tasks - self.rated_good - self.rated_bad
        )?;
        writeln!(f)?;
        writeln!(
            f,
            "  cost per success    {}  (total ${:.4} / {} successes)",
            self.cost_per_success().map(|c| format!("${c:.4}")).unwrap_or_else(|| "-".into()),
            self.cost_usd,
            self.successes
        )?;
        writeln!(
            f,
            "  cache hit           {}  ({} of {} prompt tokens)",
            if self.prompt_tokens == 0 { "-".into() } else { pct(self.cached_tokens as f64 / self.prompt_tokens as f64) },
            fmt_tokens(self.cached_tokens),
            fmt_tokens(self.prompt_tokens)
        )?;
        writeln!(f, "  steps per task      {:.1}", self.avg_steps)?;
        writeln!(
            f,
            "  model calls         {}  (avg {:.2}s{})",
            self.model_calls,
            self.avg_latency_ms / 1e3,
            self.avg_ttft_ms.map(|t| format!(", first token {:.2}s", t / 1e3)).unwrap_or_default()
        )?;
        writeln!(
            f,
            "  tool calls          {}  ({} failed{})",
            self.tool_calls,
            self.tool_errors,
            if self.tool_calls > 0 { format!(", {}", pct(self.tool_errors as f64 / self.tool_calls as f64)) } else { String::new() }
        )?;

        if !self.tools.is_empty() {
            writeln!(f)?;
            writeln!(f, "  {:<12} {:>6} {:>7} {:>6} {:>8} {:>8}", "tool", "calls", "failed", "fail%", "avg", "repeats")?;
            for t in &self.tools {
                writeln!(
                    f,
                    "  {:<12} {:>6} {:>7} {:>6} {:>7.2}s {:>8}",
                    t.name,
                    t.calls,
                    t.errors,
                    pct(t.errors as f64 / t.calls.max(1) as f64),
                    t.avg_ms / 1e3,
                    t.repeats
                )?;
            }
        }
        if let Some(sb) = &self.sandbox {
            writeln!(f)?;
            writeln!(
                f,
                "  sandbox             {} jailed calls · setup avg {:.0} ms · call avg {:.0} ms · peak RAM {:.1} MiB",
                sb.jailed_calls,
                sb.setup_ms_avg,
                sb.wall_ms_avg,
                sb.peak_memory_max as f64 / (1u64 << 20) as f64
            )?;
            writeln!(
                f,
                "  sandbox blocked     {} network requests · {} memory kills · {} timeouts · {} fork-limit hits",
                sb.blocked_requests, sb.oom_kills, sb.timeouts, sb.fork_limit_hits
            )?;
        }
        if let Some(a) = &self.approvals {
            writeln!(
                f,
                "  approvals           {} asked · {} approved · {} denied · {} unanswered · avg answer {:.0}s",
                a.requests,
                a.approved,
                a.denied,
                a.timed_out,
                a.avg_wait_ms / 1e3
            )?;
        }
        if !self.failing_calls.is_empty() {
            writeln!(f)?;
            writeln!(f, "  most failing calls")?;
            for c in &self.failing_calls {
                writeln!(f, "  {:>3}×  {} {}", c.failures, c.tool, one_line(&c.detail, 60))?;
                if !c.last_error.is_empty() {
                    writeln!(f, "        └ {}", one_line(&c.last_error, 90))?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::ledger::{CallRecord, ToolRecord};
    use crate::{Ledger, Usage};

    #[test]
    fn health_numbers() {
        let l = Ledger::in_memory().unwrap();
        let call = |t, cost| {
            l.record_call(t, &CallRecord { model: "m", usage: Usage { input_tokens: 100, cache_read_tokens: 300, ..Default::default() },
                                          provider_cost: Some(cost), latency_ms: 1000, ttft_ms: Some(200), stop_reason: "end_turn" })
                .unwrap();
        };
        let tool = |t, name, detail, err: bool| {
            l.record_tool(t, &ToolRecord { name, detail, is_error: err, error: err.then_some("exit code: 1"), ms: 10, ..Default::default() })
                .unwrap();
        };
        // done, rated good
        let a = l.start_task("t", "a").unwrap();
        call(a, 0.01);
        tool(a, "shell", "ls", false);
        l.finish_task(a, 2, "done").unwrap();
        l.set_feedback(a, true).unwrap();
        // done but rated bad -> not a success
        let b = l.start_task("t", "b").unwrap();
        call(b, 0.02);
        l.finish_task(b, 1, "done").unwrap();
        l.set_feedback(b, false).unwrap();
        // loop aborted
        let c = l.start_task("t", "c").unwrap();
        call(c, 0.03);
        for _ in 0..3 {
            tool(c, "shell", "cat /nope", true);
        }
        tool(c, "web_fetch", "http://x", true);
        l.finish_task(c, 3, "loop_aborted").unwrap();
        // done, unrated
        let d = l.start_task("t", "d").unwrap();
        call(d, 0.04);
        l.finish_task(d, 1, "done").unwrap();

        let s = l.stats(Some(7), "week").unwrap();
        assert_eq!(s.tasks, 4);
        assert_eq!(s.successes, 2);
        assert_eq!(s.success_rate(), Some(0.5));
        assert!((s.cost_per_success().unwrap() - 0.05).abs() < 1e-12); // 0.10 total / 2
        assert_eq!((s.rated_good, s.rated_bad), (1, 1));
        assert_eq!(s.outcomes[0], ("done".to_string(), 3));
        assert_eq!(s.tool_calls, 5);
        assert_eq!(s.tool_errors, 4);
        assert_eq!(s.tools[0].name, "shell");
        assert_eq!(s.tools[0].errors, 3);
        assert_eq!(s.failing_calls[0].detail, "cat /nope");
        assert_eq!(s.failing_calls[0].failures, 3);
        assert_eq!(s.failing_calls[0].last_error, "exit code: 1");
        assert_eq!(s.avg_ttft_ms, Some(200.0));
        let text = s.to_string();
        assert!(text.contains("success rate        50%  (2 of 4 tasks)"), "{text}");
        assert!(text.contains("cost per success    $0.0500"), "{text}");
        assert!(text.contains("3×  shell cat /nope"), "{text}");

        assert_eq!(l.stats(None, "all").unwrap().tasks, 4);
        assert!(Ledger::in_memory().unwrap().stats(None, "all").unwrap().to_string().contains("no tasks yet"));
    }
}

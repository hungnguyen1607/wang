//! wang-trace: a span for every task, step, model call and tool call (and, later, sandbox
//! and approval events), stored in local SQLite.
//!
//! Attribute names follow the OpenTelemetry GenAI conventions where one exists
//! (`gen_ai.request.model`, `gen_ai.usage.input_tokens`, `gen_ai.tool.name`, ...) and use a
//! `wang.` prefix otherwise, so spans can be exported to an OTel backend later.

use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Task,
    Step,
    ModelCall,
    ToolCall,
    Sandbox,
    Approval,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Task => "task",
            Kind::Step => "step",
            Kind::ModelCall => "model_call",
            Kind::ToolCall => "tool_call",
            Kind::Sandbox => "sandbox",
            Kind::Approval => "approval",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Error,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Error => "error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Span {
    pub id: i64,
    pub task_id: i64,
    pub parent_id: Option<i64>,
    pub kind: String,
    pub name: String,
    pub start_us: i64,
    pub end_us: Option<i64>,
    pub status: String,
    pub attrs: Value,
}

impl Span {
    pub fn duration_us(&self) -> Option<i64> {
        self.end_us.map(|e| e - self.start_us)
    }
    pub fn attr(&self, key: &str) -> Option<&Value> {
        self.attrs.get(key)
    }
    fn u(&self, key: &str) -> u64 {
        self.attrs.get(key).and_then(Value::as_u64).unwrap_or(0)
    }
    fn s(&self, key: &str) -> &str {
        self.attrs.get(key).and_then(Value::as_str).unwrap_or("")
    }
}

pub struct Tracer {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS spans (
    id        INTEGER PRIMARY KEY,
    task_id   INTEGER NOT NULL,
    parent_id INTEGER,
    kind      TEXT NOT NULL,
    name      TEXT NOT NULL,
    start_us  INTEGER NOT NULL,
    end_us    INTEGER,
    status    TEXT NOT NULL DEFAULT 'running',
    attrs     TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS spans_task ON spans(task_id);
"#;

pub fn now_us() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as i64).unwrap_or(0)
}

impl Tracer {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // See wang-tokens Ledger::init: no fsync per commit.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn start(&self, task_id: i64, parent: Option<i64>, kind: Kind, name: &str, attrs: Value) -> Result<i64> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO spans (task_id, parent_id, kind, name, start_us, attrs) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![task_id, parent, kind.as_str(), name, now_us(), attrs.to_string()],
        )?;
        Ok(c.last_insert_rowid())
    }

    /// Close a span and merge `attrs` into its attributes.
    pub fn end(&self, span: i64, status: Status, attrs: Value) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE spans SET end_us = ?2, status = ?3, attrs = json_patch(attrs, ?4) WHERE id = ?1",
            params![span, now_us(), status.as_str(), attrs.to_string()],
        )?;
        Ok(())
    }

    /// Merge attributes into a span without closing it (e.g. user feedback on a finished task).
    pub fn annotate(&self, span: i64, attrs: Value) -> Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute("UPDATE spans SET attrs = json_patch(attrs, ?2) WHERE id = ?1", params![span, attrs.to_string()])?;
        Ok(())
    }

    pub fn task_root(&self, task_id: i64) -> Result<Option<i64>> {
        Ok(self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT id FROM spans WHERE task_id = ?1 AND kind = 'task' ORDER BY id LIMIT 1",
                params![task_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn latest_task(&self) -> Result<Option<i64>> {
        Ok(self
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT MAX(task_id) FROM spans WHERE kind = 'task'", [], |r| r.get(0))
            .optional()?
            .flatten())
    }

    pub fn spans(&self, task_id: i64) -> Result<Vec<Span>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT id, task_id, parent_id, kind, name, start_us, end_us, status, attrs
             FROM spans WHERE task_id = ?1 ORDER BY start_us, id",
        )?;
        let rows = stmt
            .query_map(params![task_id], |r| {
                let attrs: String = r.get(8)?;
                Ok(Span {
                    id: r.get(0)?,
                    task_id: r.get(1)?,
                    parent_id: r.get(2)?,
                    kind: r.get(3)?,
                    name: r.get(4)?,
                    start_us: r.get(5)?,
                    end_us: r.get(6)?,
                    status: r.get(7)?,
                    attrs: serde_json::from_str(&attrs).unwrap_or(Value::Null),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

fn secs(us: i64) -> String {
    format!("{:.2}s", us as f64 / 1e6)
}

fn tok(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.2}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

fn bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1} KB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

/// Human-readable step-by-step timeline for one task.
pub fn render(spans: &[Span]) -> String {
    let Some(root) = spans.iter().find(|s| s.kind == "task") else {
        return "no spans recorded for this task\n".into();
    };
    let t0 = root.start_us;
    let mut out = String::new();
    let feedback = match root.s("wang.feedback") {
        "good" => " · 👍",
        "bad" => " · 👎",
        _ => "",
    };
    out += &format!(
        "task #{} · {} · {} steps · {} · ${:.4}{}\n",
        root.task_id,
        if root.end_us.is_none() { "running" } else { root.s("wang.outcome") },
        root.u("wang.steps"),
        root.duration_us().map(secs).unwrap_or_else(|| "-".into()),
        root.attr("wang.cost_usd").and_then(Value::as_f64).unwrap_or(0.0),
        feedback
    );
    out += &format!("goal: {}\n", root.s("wang.goal").replace('\n', " "));
    if let Some(err) = root.attr("error.message").and_then(Value::as_str) {
        out += &format!("error: {err}\n");
    }
    out += "\n";

    for s in spans.iter().filter(|s| s.kind != "task") {
        let at = format!("+{:>7}", secs(s.start_us - t0));
        let dur = s.duration_us().map(secs).unwrap_or_else(|| "running".into());
        let mark = if s.status == "error" { "✗" } else { " " };
        match s.kind.as_str() {
            "step" => out += &format!("{at}  {}\n", s.name),
            "model_call" if s.name == "compact history" => {
                out += &format!(
                    "{at} {mark}  history summarized: {} messages, {} prompt tokens before · ${:.4}\n",
                    s.u("wang.compact.messages"),
                    tok(s.u("wang.compact.tokens_before")),
                    s.attr("wang.cost_usd").and_then(Value::as_f64).unwrap_or(0.0)
                );
            }
            "model_call" => {
                let ttft = s
                    .attr("wang.ttft_ms")
                    .and_then(Value::as_u64)
                    .map(|ms| format!(" (ttft {:.2}s)", ms as f64 / 1e3))
                    .unwrap_or_default();
                let reasoning = match s.u("gen_ai.usage.reasoning_tokens") {
                    0 => String::new(),
                    r => format!(" (reasoning {})", tok(r)),
                };
                out += &format!(
                    "{at} {mark}  model {}  {dur}{ttft}  prompt {} (cached {}) · out {}{reasoning} · ${:.4} · {}\n",
                    s.s("gen_ai.response.model").is_empty().then(|| s.s("gen_ai.request.model")).unwrap_or(s.s("gen_ai.response.model")),
                    tok(s.u("gen_ai.usage.input_tokens")),
                    tok(s.u("gen_ai.usage.cache_read.input_tokens")),
                    tok(s.u("gen_ai.usage.output_tokens")),
                    s.attr("wang.cost_usd").and_then(Value::as_f64).unwrap_or(0.0),
                    s.attr("gen_ai.response.finish_reasons")
                        .and_then(|v| v.get(0))
                        .and_then(Value::as_str)
                        .unwrap_or("-"),
                );
            }
            "tool_call" => {
                let raw = s.u("wang.tool.raw_bytes");
                let sent = s.u("wang.tool.sent_bytes");
                let size = if raw > sent { format!("{} → {}", bytes(raw), bytes(sent)) } else { bytes(raw) };
                let mut flags = String::new();
                if s.attr("wang.tool.repeated").and_then(Value::as_bool) == Some(true) {
                    flags += " [repeat]";
                }
                if s.attr("wang.tool.deduped").and_then(Value::as_bool) == Some(true) {
                    flags += " [deduped]";
                }
                let detail: String = s.s("wang.tool.detail").chars().take(80).collect();
                out += &format!(
                    "{at} {mark}    tool {}  {dur}  {size}{flags}  {}\n",
                    s.s("gen_ai.tool.name"),
                    detail.replace('\n', " ")
                );
                if let Some(setup) = s.attr("wang.sandbox.setup_ms").and_then(Value::as_f64) {
                    let mut jail = format!(
                        "jail: setup {:.0} ms · peak {} · cpu {:.0} ms",
                        setup,
                        bytes(s.u("wang.sandbox.peak_memory_bytes")),
                        s.attr("wang.sandbox.cpu_ms").and_then(Value::as_f64).unwrap_or(0.0)
                    );
                    if s.attr("wang.sandbox.oom_killed").and_then(Value::as_bool) == Some(true) {
                        jail += " · KILLED: memory limit";
                    }
                    if s.attr("wang.sandbox.timed_out").and_then(Value::as_bool) == Some(true) {
                        jail += " · KILLED: time limit";
                    }
                    if s.u("wang.sandbox.pids_limit_hits") > 0 {
                        jail += &format!(" · {} forks refused", s.u("wang.sandbox.pids_limit_hits"));
                    }
                    if let Some(b) = s.attr("wang.sandbox.blocked").and_then(Value::as_array).filter(|b| !b.is_empty()) {
                        let hosts: Vec<&str> = b.iter().filter_map(Value::as_str).collect();
                        jail += &format!(" · blocked {}", hosts.join(", "));
                    }
                    out += &format!("              {jail}\n");
                }
            }
            "approval" => {
                let decision = s.s("wang.approval.decision");
                let wait = s.attr("wang.approval.wait_ms").and_then(Value::as_u64).map(|m| format!(" after {:.1}s", m as f64 / 1e3)).unwrap_or_default();
                out += &format!("{at} {mark}      approval {}{wait}\n", if decision.is_empty() { "pending" } else { decision });
            }
            _ => out += &format!("{at} {mark}  {} {}  {dur}\n", s.kind, s.name),
        }
        if s.kind != "task" {
            if let Some(e) = s.attr("error.message").and_then(Value::as_str) {
                let e: String = e.chars().take(160).collect();
                out += &format!("            error: {}\n", e.replace('\n', " "));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spans_roundtrip_and_render() {
        let t = Tracer::in_memory().unwrap();
        let root = t.start(7, None, Kind::Task, "task", json!({"wang.goal": "find big files"})).unwrap();
        let step = t.start(7, Some(root), Kind::Step, "step 1", json!({})).unwrap();
        let mc = t.start(7, Some(step), Kind::ModelCall, "chat m", json!({"gen_ai.request.model": "m"})).unwrap();
        t.end(
            mc,
            Status::Ok,
            json!({"gen_ai.usage.input_tokens": 2100, "gen_ai.usage.cache_read.input_tokens": 1800,
                   "gen_ai.usage.output_tokens": 120, "wang.cost_usd": 0.0004, "wang.ttft_ms": 400,
                   "gen_ai.response.finish_reasons": ["tool_use"]}),
        )
        .unwrap();
        let tool = t.start(7, Some(step), Kind::ToolCall, "execute_tool shell", json!({"gen_ai.tool.name": "shell", "wang.tool.detail": "du -sh *"})).unwrap();
        t.end(tool, Status::Error, json!({"wang.tool.raw_bytes": 50000, "wang.tool.sent_bytes": 12000, "wang.tool.repeated": true, "error.message": "exit 1",
                                          "wang.sandbox.setup_ms": 16.2, "wang.sandbox.peak_memory_bytes": 7340032, "wang.sandbox.cpu_ms": 90.0,
                                          "wang.sandbox.blocked": ["evil.com:443"]})).unwrap();
        t.end(step, Status::Ok, json!({})).unwrap();
        t.end(root, Status::Ok, json!({"wang.outcome": "done", "wang.steps": 1, "wang.cost_usd": 0.0004})).unwrap();
        t.annotate(root, json!({"wang.feedback": "good"})).unwrap();

        assert_eq!(t.latest_task().unwrap(), Some(7));
        assert_eq!(t.task_root(7).unwrap(), Some(root));
        let spans = t.spans(7).unwrap();
        assert_eq!(spans.len(), 4);
        assert_eq!(spans[0].attrs["wang.goal"], "find big files");
        let text = render(&spans);
        assert!(text.contains("task #7 · done · 1 steps"), "{text}");
        assert!(text.contains("👍"));
        assert!(text.contains("prompt 2.1k (cached 1.8k)"), "{text}");
        assert!(text.contains("ttft 0.40s"));
        assert!(text.contains("48.8 KB → 11.7 KB [repeat]"), "{text}");
        assert!(text.contains("error: exit 1"));
        assert!(text.contains("jail: setup 16 ms · peak 7.0 MB · cpu 90 ms · blocked evil.com:443"), "{text}");
    }

    #[test]
    fn empty() {
        let t = Tracer::in_memory().unwrap();
        assert_eq!(t.latest_task().unwrap(), None);
        assert!(render(&[]).contains("no spans"));
    }
}

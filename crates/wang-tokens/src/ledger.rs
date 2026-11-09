use std::fmt;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::pricing::{Usage, cost_usd, price_for};

/// SQLite log of every task, model call and tool call — the cost side of wang.
/// (The step-by-step timeline lives in `wang-trace`, in the same database file.)
pub struct Ledger {
    pub(crate) conn: Mutex<Connection>,
}

/// One tool call, as recorded.
#[derive(Debug, Clone, Default)]
pub struct ToolRecord<'a> {
    pub name: &'a str,
    /// One-line summary of the call (command, path, url).
    pub detail: &'a str,
    pub raw_bytes: usize,
    pub sent_bytes: usize,
    pub ms: u64,
    pub is_error: bool,
    /// First part of the error output, when `is_error`.
    pub error: Option<&'a str>,
    pub repeated: bool,
}

/// One model call, as recorded.
#[derive(Debug, Clone)]
pub struct CallRecord<'a> {
    pub model: &'a str,
    pub usage: Usage,
    /// Cost reported by the provider (OpenRouter `usage.cost`); preferred over the price table.
    pub provider_cost: Option<f64>,
    pub latency_ms: u64,
    pub ttft_ms: Option<u64>,
    pub stop_reason: &'a str,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
    id          INTEGER PRIMARY KEY,
    started_at  TEXT NOT NULL DEFAULT (datetime('now')),
    source      TEXT NOT NULL,
    prompt      TEXT NOT NULL,
    steps       INTEGER NOT NULL DEFAULT 0,
    status      TEXT NOT NULL DEFAULT 'running',
    feedback    TEXT
);
CREATE TABLE IF NOT EXISTS calls (
    id                 INTEGER PRIMARY KEY,
    task_id            INTEGER NOT NULL REFERENCES tasks(id),
    ts                 TEXT NOT NULL DEFAULT (datetime('now')),
    model              TEXT NOT NULL,
    input_tokens       INTEGER NOT NULL,
    cache_write_tokens INTEGER NOT NULL,
    cache_read_tokens  INTEGER NOT NULL,
    output_tokens      INTEGER NOT NULL,
    reasoning_tokens   INTEGER NOT NULL DEFAULT 0,
    cost_usd           REAL NOT NULL,
    cost_source        TEXT NOT NULL,
    latency_ms         INTEGER NOT NULL,
    ttft_ms            INTEGER,
    stop_reason        TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS tool_calls (
    id         INTEGER PRIMARY KEY,
    task_id    INTEGER NOT NULL REFERENCES tasks(id),
    ts         TEXT NOT NULL DEFAULT (datetime('now')),
    name       TEXT NOT NULL,
    raw_bytes  INTEGER NOT NULL,
    sent_bytes INTEGER NOT NULL,
    ms         INTEGER NOT NULL,
    is_error   INTEGER NOT NULL,
    repeated   INTEGER NOT NULL DEFAULT 0,
    detail     TEXT NOT NULL DEFAULT '',
    error      TEXT
);
CREATE INDEX IF NOT EXISTS calls_task ON calls(task_id);
CREATE INDEX IF NOT EXISTS tool_calls_task ON tool_calls(task_id);
CREATE TABLE IF NOT EXISTS proxy_requests (
    id          INTEGER PRIMARY KEY,
    task_id     INTEGER REFERENCES tasks(id),
    ts          TEXT NOT NULL DEFAULT (datetime('now')),
    client      TEXT NOT NULL,
    api         TEXT NOT NULL,
    model       TEXT NOT NULL,
    status      INTEGER NOT NULL,
    bytes_in    INTEGER NOT NULL,
    bytes_out   INTEGER NOT NULL,
    trimmed     INTEGER NOT NULL,
    deduped     INTEGER NOT NULL,
    cache_hint  INTEGER NOT NULL,
    latency_ms  INTEGER NOT NULL
);
"#;

/// Add columns introduced after a database was created (CREATE IF NOT EXISTS won't).
fn migrate(conn: &Connection) -> Result<()> {
    let added: &[(&str, &str, &str)] = &[
        ("tool_calls", "detail", "TEXT NOT NULL DEFAULT ''"),
        ("tool_calls", "error", "TEXT"),
    ];
    for (table, col, decl) in added {
        let exists: bool = conn
            .prepare(&format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"))?
            .exists([col])?;
        if !exists {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {col} {decl}"))?;
        }
    }
    Ok(())
}

impl Ledger {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // WAL + synchronous=NORMAL: no fsync per commit (synced at checkpoints). Corruption-safe;
        // a power cut can lose the last few writes. FULL capped the agent at ~17 tasks/s (bench/m5-scale.md).
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn start_task(&self, source: &str, prompt: &str) -> Result<i64> {
        let preview: String = prompt.chars().take(200).collect();
        let c = self.conn.lock().unwrap();
        c.execute("INSERT INTO tasks (source, prompt) VALUES (?1, ?2)", params![source, preview])?;
        Ok(c.last_insert_rowid())
    }

    pub fn finish_task(&self, task_id: i64, steps: u32, status: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE tasks SET steps = ?2, status = ?3 WHERE id = ?1",
            params![task_id, steps, status],
        )?;
        Ok(())
    }

    /// 👍 / 👎 on a task. Returns false if the task doesn't exist.
    pub fn set_feedback(&self, task_id: i64, good: bool) -> Result<bool> {
        let n = self.conn.lock().unwrap().execute(
            "UPDATE tasks SET feedback = ?2 WHERE id = ?1",
            params![task_id, if good { "good" } else { "bad" }],
        )?;
        Ok(n > 0)
    }

    pub fn latest_task(&self) -> Result<Option<i64>> {
        Ok(self.conn.lock().unwrap().query_row("SELECT MAX(id) FROM tasks WHERE source NOT LIKE 'proxy:%' AND source NOT LIKE 'worker:%'", [], |r| r.get(0)).optional()?.flatten())
    }

    /// Record a model call; returns the cost in USD that was recorded.
    pub fn record_call(&self, task_id: i64, rec: &CallRecord) -> Result<f64> {
        let (cost, source) = match rec.provider_cost {
            Some(c) => (c, "provider"),
            None if price_for(rec.model).is_some() => (cost_usd(rec.model, &rec.usage), "price_table"),
            None => (0.0, "unknown"),
        };
        let u = &rec.usage;
        self.conn.lock().unwrap().execute(
            "INSERT INTO calls (task_id, model, input_tokens, cache_write_tokens, cache_read_tokens, output_tokens,
                                reasoning_tokens, cost_usd, cost_source, latency_ms, ttft_ms, stop_reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                task_id,
                rec.model,
                u.input_tokens,
                u.cache_write_tokens,
                u.cache_read_tokens,
                u.output_tokens,
                u.reasoning_tokens,
                cost,
                source,
                rec.latency_ms,
                rec.ttft_ms,
                rec.stop_reason
            ],
        )?;
        Ok(cost)
    }

    pub fn record_tool(&self, task_id: i64, t: &ToolRecord) -> Result<()> {
        let error: Option<String> = t.error.map(|e| e.chars().take(500).collect());
        self.conn.lock().unwrap().execute(
            "INSERT INTO tool_calls (task_id, name, detail, raw_bytes, sent_bytes, ms, is_error, error, repeated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![task_id, t.name, t.detail, t.raw_bytes as i64, t.sent_bytes as i64, t.ms, t.is_error, error, t.repeated],
        )?;
        Ok(())
    }

    /// Summary of the last `days` local calendar days (1 = today).
    pub fn summary(&self, days: u32, label: &str) -> Result<Summary> {
        let days = days.max(1);
        let since = format!("-{} days", days - 1);
        let c = self.conn.lock().unwrap();
        let task_filter = "date(t.started_at, 'localtime') >= date('now', 'localtime', ?1)";

        let mut usage = Usage::default();
        let (calls, cost, unpriced): (i64, f64, i64) = c.query_row(
            &format!(
                "SELECT COUNT(*), COALESCE(SUM(k.input_tokens),0), COALESCE(SUM(k.cache_write_tokens),0),
                        COALESCE(SUM(k.cache_read_tokens),0), COALESCE(SUM(k.output_tokens),0),
                        COALESCE(SUM(k.reasoning_tokens),0), COALESCE(SUM(k.cost_usd),0),
                        COALESCE(SUM(k.cost_source = 'unknown'),0)
                 FROM calls k JOIN tasks t ON t.id = k.task_id WHERE {task_filter}"
            ),
            params![since],
            |r| {
                usage.input_tokens = r.get::<_, i64>(1)? as u64;
                usage.cache_write_tokens = r.get::<_, i64>(2)? as u64;
                usage.cache_read_tokens = r.get::<_, i64>(3)? as u64;
                usage.output_tokens = r.get::<_, i64>(4)? as u64;
                usage.reasoning_tokens = r.get::<_, i64>(5)? as u64;
                Ok((r.get(0)?, r.get(6)?, r.get(7)?))
            },
        )?;

        let (tool_calls, tool_raw, tool_sent, tool_repeats): (i64, i64, i64, i64) = c.query_row(
            &format!(
                "SELECT COUNT(*), COALESCE(SUM(x.raw_bytes),0), COALESCE(SUM(x.sent_bytes),0), COALESCE(SUM(x.repeated),0)
                 FROM tool_calls x JOIN tasks t ON t.id = x.task_id WHERE {task_filter}"
            ),
            params![since],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;

        let mut stmt = c.prepare(&format!(
            "SELECT t.id, time(t.started_at, 'localtime'), t.prompt, t.status, t.feedback, COUNT(k.id),
                    COALESCE(SUM(k.input_tokens + k.cache_write_tokens + k.cache_read_tokens),0),
                    COALESCE(SUM(k.cache_read_tokens),0), COALESCE(SUM(k.output_tokens),0),
                    COALESCE(SUM(k.cost_usd),0)
             FROM tasks t LEFT JOIN calls k ON k.task_id = t.id
             WHERE {task_filter}
             GROUP BY t.id ORDER BY t.id"
        ))?;
        let tasks = stmt
            .query_map(params![since], |r| {
                Ok(TaskRow {
                    id: r.get(0)?,
                    time: r.get(1)?,
                    prompt: r.get(2)?,
                    status: r.get(3)?,
                    feedback: r.get(4)?,
                    calls: r.get(5)?,
                    prompt_tokens: r.get::<_, i64>(6)? as u64,
                    cache_read_tokens: r.get::<_, i64>(7)? as u64,
                    output_tokens: r.get::<_, i64>(8)? as u64,
                    cost_usd: r.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(Summary {
            label: label.to_string(),
            calls: calls as u64,
            usage,
            cost_usd: cost,
            unpriced_calls: unpriced as u64,
            tool_calls: tool_calls as u64,
            tool_raw_bytes: tool_raw as u64,
            tool_sent_bytes: tool_sent as u64,
            tool_repeats: tool_repeats as u64,
            tasks,
        })
    }
}

#[derive(Debug, Clone)]
pub struct TaskRow {
    pub id: i64,
    pub time: String,
    pub prompt: String,
    pub status: String,
    pub feedback: Option<String>,
    pub calls: i64,
    pub prompt_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub label: String,
    pub calls: u64,
    pub usage: Usage,
    pub cost_usd: f64,
    pub unpriced_calls: u64,
    pub tool_calls: u64,
    pub tool_raw_bytes: u64,
    pub tool_sent_bytes: u64,
    pub tool_repeats: u64,
    pub tasks: Vec<TaskRow>,
}

pub fn fmt_tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.2}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

impl Summary {
    /// Short form, for a narrow screen.
    pub fn short(&self) -> String {
        let u = &self.usage;
        format!(
            "{}: {} tasks · {} calls · ${:.4} · prompt {} ({:.0}% cached) · output {}",
            self.label,
            self.tasks.len(),
            self.calls,
            self.cost_usd,
            fmt_tokens(u.prompt_tokens()),
            u.cache_hit_ratio() * 100.0,
            fmt_tokens(u.output_tokens),
        )
    }
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let u = &self.usage;
        writeln!(f, "wang cost — {}", self.label)?;
        writeln!(f)?;
        writeln!(f, "  tasks          {}", self.tasks.len())?;
        writeln!(f, "  model calls    {}", self.calls)?;
        writeln!(f, "  cost           ${:.4}", self.cost_usd)?;
        writeln!(f, "  prompt tokens  {}", fmt_tokens(u.prompt_tokens()))?;
        writeln!(f, "    uncached     {}", fmt_tokens(u.input_tokens))?;
        writeln!(f, "    cache write  {}", fmt_tokens(u.cache_write_tokens))?;
        writeln!(f, "    cache read   {}", fmt_tokens(u.cache_read_tokens))?;
        writeln!(f, "  cache hit      {:.1}%", u.cache_hit_ratio() * 100.0)?;
        writeln!(f, "  output tokens  {}  (reasoning {})", fmt_tokens(u.output_tokens), fmt_tokens(u.reasoning_tokens))?;
        writeln!(
            f,
            "  tool calls     {}  ({} → {} bytes after caps, {} repeats)",
            self.tool_calls, self.tool_raw_bytes, self.tool_sent_bytes, self.tool_repeats
        )?;
        if self.unpriced_calls > 0 {
            writeln!(f, "  ! {} calls had no reported cost and no known price (counted as $0)", self.unpriced_calls)?;
        }
        if !self.tasks.is_empty() {
            writeln!(f)?;
            writeln!(
                f,
                "  {:>5}  {:8}  {:>5}  {:>8}  {:>6}  {:>7}  {:>9}     goal",
                "task", "time", "calls", "prompt", "cached", "output", "cost"
            )?;
            for t in &self.tasks {
                let cached = if t.prompt_tokens == 0 {
                    0.0
                } else {
                    t.cache_read_tokens as f64 * 100.0 / t.prompt_tokens as f64
                };
                let fb = match t.feedback.as_deref() {
                    Some("good") => "👍",
                    Some("bad") => "👎",
                    _ => "  ",
                };
                let mut preview: String = t.prompt.replace('\n', " ").chars().take(40).collect();
                if t.status != "done" {
                    preview = format!("[{}] {preview}", t.status);
                }
                writeln!(
                    f,
                    "  {:>5}  {:8}  {:>5}  {:>8}  {:>5.0}%  {:>7}  ${:>8.4}  {fb} {}",
                    t.id,
                    t.time,
                    t.calls,
                    fmt_tokens(t.prompt_tokens),
                    cached,
                    fmt_tokens(t.output_tokens),
                    t.cost_usd,
                    preview
                )?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_summarizes() {
        let l = Ledger::in_memory().unwrap();
        let t = l.start_task("test", "what's in my notes?").unwrap();
        let u = Usage { input_tokens: 200, output_tokens: 100, cache_write_tokens: 1000, ..Default::default() };
        let c1 = l
            .record_call(t, &CallRecord { model: "claude-opus-5-5", usage: u, provider_cost: None, latency_ms: 900, ttft_ms: None, stop_reason: "tool_use" })
            .unwrap();
        assert!(c1 > 0.0);
        let u2 = Usage { input_tokens: 50, output_tokens: 40, cache_write_tokens: 300, cache_read_tokens: 1000, reasoning_tokens: 10 };
        let c2 = l
            .record_call(t, &CallRecord { model: "some/unknown", usage: u2, provider_cost: Some(0.5), latency_ms: 700, ttft_ms: Some(200), stop_reason: "end_turn" })
            .unwrap();
        assert_eq!(c2, 0.5);
        l.record_tool(t, &ToolRecord { name: "shell", detail: "ls", raw_bytes: 50_000, sent_bytes: 12_000, ms: 30, repeated: true, ..Default::default() }).unwrap();
        l.finish_task(t, 2, "done").unwrap();
        assert!(l.set_feedback(t, true).unwrap());
        assert!(!l.set_feedback(999, true).unwrap());
        assert_eq!(l.latest_task().unwrap(), Some(t));

        let s = l.summary(1, "today").unwrap();
        assert_eq!(s.calls, 2);
        assert_eq!(s.tasks.len(), 1);
        assert_eq!(s.usage.cache_read_tokens, 1000);
        assert_eq!(s.usage.reasoning_tokens, 10);
        assert_eq!(s.tool_repeats, 1);
        assert!((s.cost_usd - (c1 + 0.5)).abs() < 1e-12);
        assert_eq!(s.unpriced_calls, 0);
        let text = s.to_string();
        assert!(text.contains("what's in my notes?"), "{text}");
        assert!(text.contains("👍"));
    }
}

#[cfg(test)]
mod migrate_tests {
    use super::*;

    #[test]
    fn adds_new_columns_to_m1_database() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE tasks (id INTEGER PRIMARY KEY, started_at TEXT NOT NULL DEFAULT (datetime('now')), source TEXT NOT NULL,
                                 prompt TEXT NOT NULL, steps INTEGER NOT NULL DEFAULT 0, status TEXT NOT NULL DEFAULT 'running', feedback TEXT);
             CREATE TABLE tool_calls (id INTEGER PRIMARY KEY, task_id INTEGER NOT NULL, ts TEXT NOT NULL DEFAULT (datetime('now')),
                                      name TEXT NOT NULL, raw_bytes INTEGER NOT NULL, sent_bytes INTEGER NOT NULL, ms INTEGER NOT NULL,
                                      is_error INTEGER NOT NULL, repeated INTEGER NOT NULL DEFAULT 0);",
        )
        .unwrap();
        let l = Ledger::init(conn).unwrap();
        let t = l.start_task("t", "x").unwrap();
        l.record_tool(t, &ToolRecord { name: "shell", detail: "false", is_error: true, error: Some("exit 1"), ..Default::default() }).unwrap();
        // idempotent
        migrate(&l.conn.lock().unwrap()).unwrap();
    }
}

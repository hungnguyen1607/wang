//! What `wang proxy` did to each request it passed through, and the savings summary.
//! Token counts and $ for those requests live in the normal `calls` table (tasks with
//! source `proxy:<client>`), so `wang cost` includes them.

use std::fmt;

use anyhow::Result;
use rusqlite::params;

use crate::ledger::{Ledger, fmt_tokens};
use crate::pricing::{Usage, price_for};

/// One request passed through the proxy.
#[derive(Debug, Clone, Default)]
pub struct ProxyRecord<'a> {
    pub task_id: Option<i64>,
    /// `claude-code`, `opencode`, ...
    pub client: &'a str,
    /// `anthropic` or `openai`
    pub api: &'a str,
    pub model: &'a str,
    /// Upstream HTTP status (0 = upstream unreachable, or blocked by the proxy).
    pub status: u16,
    /// Request body as the client sent it / as forwarded.
    pub bytes_in: usize,
    pub bytes_out: usize,
    pub trimmed: u32,
    pub deduped: u32,
    pub cache_hint: bool,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, Default)]
pub struct ProxyClientRow {
    pub client: String,
    pub requests: u64,
    pub errors: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub trimmed: u64,
    pub deduped: u64,
    pub cache_hints: u64,
    pub usage: Usage,
    pub cost_usd: f64,
    /// Lower / upper estimate of $ the removed bytes would have cost (cache-read vs uncached price).
    pub saved_usd: (f64, f64),
}

#[derive(Debug, Clone, Default)]
pub struct ProxySummary {
    pub label: String,
    pub clients: Vec<ProxyClientRow>,
}

impl ProxySummary {
    pub fn cost_usd(&self) -> f64 {
        self.clients.iter().map(|c| c.cost_usd).sum()
    }
}

/// ~4 bytes per token for English text and code: an estimate, labeled as such.
pub const BYTES_PER_TOKEN: f64 = 4.0;

impl Ledger {
    pub fn record_proxy(&self, r: &ProxyRecord) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO proxy_requests (task_id, client, api, model, status, bytes_in, bytes_out, trimmed, deduped,
                                         cache_hint, latency_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                r.task_id,
                r.client,
                r.api,
                r.model,
                r.status,
                r.bytes_in as i64,
                r.bytes_out as i64,
                r.trimmed,
                r.deduped,
                r.cache_hint,
                r.latency_ms as i64
            ],
        )?;
        Ok(())
    }

    /// $ spent through the proxy today (local time), for the daily budget.
    pub fn proxy_cost_today(&self) -> Result<f64> {
        Ok(self.conn.lock().unwrap().query_row(
            "SELECT COALESCE(SUM(k.cost_usd), 0) FROM calls k JOIN tasks t ON t.id = k.task_id
             WHERE t.source LIKE 'proxy:%' AND date(k.ts, 'localtime') = date('now', 'localtime')",
            [],
            |r| r.get(0),
        )?)
    }

    /// Per-client summary of the last `days` local calendar days.
    pub fn proxy_summary(&self, days: u32, label: &str) -> Result<ProxySummary> {
        let since = format!("-{} days", days.max(1) - 1);
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT client, COUNT(*), COALESCE(SUM(status != 200), 0), COALESCE(SUM(bytes_in), 0),
                    COALESCE(SUM(bytes_out), 0), COALESCE(SUM(trimmed), 0), COALESCE(SUM(deduped), 0),
                    COALESCE(SUM(cache_hint), 0)
             FROM proxy_requests WHERE date(ts, 'localtime') >= date('now', 'localtime', ?1)
             GROUP BY client ORDER BY COUNT(*) DESC",
        )?;
        let mut clients = stmt
            .query_map(params![since], |r| {
                Ok(ProxyClientRow {
                    client: r.get(0)?,
                    requests: r.get::<_, i64>(1)? as u64,
                    errors: r.get::<_, i64>(2)? as u64,
                    bytes_in: r.get::<_, i64>(3)? as u64,
                    bytes_out: r.get::<_, i64>(4)? as u64,
                    trimmed: r.get::<_, i64>(5)? as u64,
                    deduped: r.get::<_, i64>(6)? as u64,
                    cache_hints: r.get::<_, i64>(7)? as u64,
                    ..Default::default()
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        for row in &mut clients {
            // Token usage and cost, from the calls the proxy recorded for this client.
            c.query_row(
                "SELECT COALESCE(SUM(k.input_tokens),0), COALESCE(SUM(k.cache_write_tokens),0),
                        COALESCE(SUM(k.cache_read_tokens),0), COALESCE(SUM(k.output_tokens),0),
                        COALESCE(SUM(k.cost_usd),0)
                 FROM calls k JOIN tasks t ON t.id = k.task_id
                 WHERE t.source = ?2 AND date(k.ts, 'localtime') >= date('now', 'localtime', ?1)",
                params![since, format!("proxy:{}", row.client)],
                |r| {
                    row.usage.input_tokens = r.get::<_, i64>(0)? as u64;
                    row.usage.cache_write_tokens = r.get::<_, i64>(1)? as u64;
                    row.usage.cache_read_tokens = r.get::<_, i64>(2)? as u64;
                    row.usage.output_tokens = r.get::<_, i64>(3)? as u64;
                    row.cost_usd = r.get(4)?;
                    Ok(())
                },
            )?;
            // What the removed bytes would have cost, per model: at cache-read price (most of a
            // long conversation is cached) up to the uncached input price.
            let mut by_model = c.prepare(
                "SELECT model, COALESCE(SUM(bytes_in - bytes_out), 0) FROM proxy_requests
                 WHERE client = ?2 AND status = 200 AND date(ts, 'localtime') >= date('now', 'localtime', ?1)
                 GROUP BY model",
            )?;
            let rows = by_model
                .query_map(params![since, row.client], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (model, saved_bytes) in rows {
                if let Some(p) = price_for(&model) {
                    let mtok = saved_bytes.max(0) as f64 / BYTES_PER_TOKEN / 1e6;
                    row.saved_usd.0 += mtok * p.cache_read;
                    row.saved_usd.1 += mtok * p.input;
                }
            }
        }
        Ok(ProxySummary { label: label.into(), clients })
    }
}

impl fmt::Display for ProxySummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "wang proxy — {}", self.label)?;
        if self.clients.is_empty() {
            return writeln!(f, "\n  no requests yet");
        }
        for c in &self.clients {
            let saved = c.bytes_in.saturating_sub(c.bytes_out);
            let pct = if c.bytes_in == 0 { 0.0 } else { saved as f64 * 100.0 / c.bytes_in as f64 };
            writeln!(f)?;
            writeln!(f, "  {}", c.client)?;
            writeln!(f, "    requests       {}  ({} not 200)", c.requests, c.errors)?;
            writeln!(f, "    cost           ${:.4}", c.cost_usd)?;
            writeln!(
                f,
                "    prompt tokens  {}  (cache hit {:.1}%), output {}",
                fmt_tokens(c.usage.prompt_tokens()),
                c.usage.cache_hit_ratio() * 100.0,
                fmt_tokens(c.usage.output_tokens)
            )?;
            writeln!(
                f,
                "    request bytes  {} → {}  (-{pct:.1}%, ≈{} tokens not sent)",
                fmt_tokens(c.bytes_in),
                fmt_tokens(c.bytes_out),
                fmt_tokens((saved as f64 / BYTES_PER_TOKEN) as u64)
            )?;
            writeln!(f, "    edits          {} tool outputs trimmed, {} repeats replaced", c.trimmed, c.deduped)?;
            writeln!(f, "    cache hints    added to {} requests that had none", c.cache_hints)?;
            if saved > 0 {
                writeln!(f, "    est. saved     ${:.4}–${:.4}  (removed tokens at cache-read … uncached price)", c.saved_usd.0, c.saved_usd.1)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CallRecord;

    #[test]
    fn summary_joins_requests_and_calls() {
        let l = Ledger::in_memory().unwrap();
        let t = l.start_task("proxy:claude-code", "fix the bug").unwrap();
        l.record_call(
            t,
            &CallRecord {
                model: "claude-opus-5-5",
                usage: Usage { input_tokens: 100, cache_read_tokens: 900, output_tokens: 50, ..Default::default() },
                provider_cost: None,
                latency_ms: 10,
                ttft_ms: None,
                stop_reason: "end_turn",
            },
        )
        .unwrap();
        let rec = ProxyRecord {
            task_id: Some(t),
            client: "claude-code",
            api: "anthropic",
            model: "claude-opus-5-5",
            status: 200,
            bytes_in: 4_000_000,
            bytes_out: 3_000_000,
            trimmed: 2,
            deduped: 1,
            cache_hint: false,
            latency_ms: 10,
        };
        l.record_proxy(&rec).unwrap();
        l.record_proxy(&ProxyRecord { status: 529, bytes_in: 10, bytes_out: 10, trimmed: 0, deduped: 0, ..rec }).unwrap();

        let s = l.proxy_summary(1, "today").unwrap();
        assert_eq!(s.clients.len(), 1);
        let c = &s.clients[0];
        assert_eq!((c.requests, c.errors, c.trimmed, c.deduped), (2, 1, 2, 1));
        assert_eq!(c.usage.cache_read_tokens, 900);
        // 1 MB removed ≈ 250k tokens: $0.05 at cache-read, $1.00 uncached (opus 5.5).
        assert!((c.saved_usd.0 - 0.05).abs() < 1e-9 && (c.saved_usd.1 - 1.0).abs() < 1e-9, "{:?}", c.saved_usd);
        assert!(l.proxy_cost_today().unwrap() > 0.0);
        let out = s.to_string();
        assert!(out.contains("claude-code") && out.contains("2 tool outputs trimmed"), "{out}");

        // Proxy traffic stays out of agent health and the "latest task" default.
        assert_eq!(l.latest_task().unwrap(), None);
        assert_eq!(l.stats(Some(1), "today").unwrap().tasks, 0);
    }
}

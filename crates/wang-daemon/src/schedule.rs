//! Scheduled tasks (M12): "every weekday at 8:00, summarize the top Hacker News posts".
//!
//! - **When** is plain English, parsed here: `every day at 8:00`, `every weekday at 9am`,
//!   `every monday at 18:30`, `every morning`, `every 2 hours`, `in 20 minutes`,
//!   `tomorrow at 9am`, `at 17:00`, `at 2026-12-01 09:00`.
//! - **Store**: a `schedules` table in wang's database (SQLite), so the daemon, the CLI and the
//!   agent's `schedule` tool can all change it safely.
//! - **Runner**: the daemon checks every 15 s; a due task runs through the full agent (workflows,
//!   plans, sandbox, budgets) in its own conversation, and the reply goes back where the schedule
//!   was made: that Telegram chat, else wang's alerts (ntfy / Telegram). The last result is kept
//!   for `wang schedule list`.
//! - Missed runs (daemon off) within 12 h run once on start; older ones are skipped.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Duration as CDuration, Local, NaiveDate, NaiveTime, TimeZone};
use rusqlite::{Connection, OptionalExtension, params};

use crate::agent::Conversation;
use crate::model::ModelClient;
use crate::notify::{Notice, Notify};
use crate::server::Hub;
use crate::telegram::Bot;

/// Shortest interval for a repeating task.
pub const MIN_EVERY: CDuration = CDuration::minutes(10);
/// Most active schedules.
pub const MAX_ACTIVE: usize = 20;
/// Runs missed for longer than this (daemon was off) are skipped, not caught up.
const CATCH_UP: CDuration = CDuration::hours(12);

#[derive(Debug, Clone, PartialEq)]
pub enum Rule {
    Once(DateTime<Local>),
    Every(CDuration),
    /// At `time` on the given weekdays (Monday = 0).
    Weekly { time: NaiveTime, days: [bool; 7] },
}

const DAY_NAMES: [&str; 7] = ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"];

fn parse_time(s: &str) -> Option<NaiveTime> {
    let s = s.trim().replace(' ', "");
    match s.as_str() {
        "noon" => return NaiveTime::from_hms_opt(12, 0, 0),
        "midnight" => return NaiveTime::from_hms_opt(0, 0, 0),
        _ => {}
    }
    let (body, pm) = if let Some(b) = s.strip_suffix("pm") {
        (b, Some(true))
    } else if let Some(b) = s.strip_suffix("am") {
        (b, Some(false))
    } else {
        (s.as_str(), None)
    };
    let (h, m) = match body.split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        None => (body.parse::<u32>().ok()?, 0),
    };
    let h = match pm {
        Some(true) if h < 12 => h + 12,
        Some(false) if h == 12 => 0,
        Some(_) if h > 12 => return None,
        _ => h,
    };
    NaiveTime::from_hms_opt(h, m, 0)
}

fn at(date: NaiveDate, t: NaiveTime) -> Option<DateTime<Local>> {
    Local.from_local_datetime(&date.and_time(t)).earliest()
}

fn unit(n: i64, u: &str) -> Option<CDuration> {
    match u.trim_end_matches('s') {
        "m" | "min" | "minute" => Some(CDuration::minutes(n)),
        "h" | "hr" | "hour" => Some(CDuration::hours(n)),
        "d" | "day" => Some(CDuration::days(n)),
        _ => None,
    }
}

/// Parse a plain-English "when", relative to `now`.
pub fn parse(spec: &str, now: DateTime<Local>) -> Result<Rule> {
    let s = spec.trim().to_lowercase().replace(',', " ");
    let words: Vec<&str> = s.split_whitespace().collect();
    // An optional "[at] TIME" from position `i` of `ws` to the end: None = no time given.
    fn time_from(ws: &[&str], i: usize) -> Option<Option<NaiveTime>> {
        match ws.get(i) {
            None => Some(None),
            Some(&"at") => parse_time(&ws[i + 1..].join(" ")).map(Some),
            Some(_) => parse_time(&ws[i..].join(" ")).map(Some),
        }
    }
    let rest_time = |i: usize| time_from(&words, i);
    let bad = || anyhow::anyhow!("can't read \"{spec}\". Try: every day at 8:00 · every weekday at 9am · every monday at 18:30 · every 2 hours · in 20 minutes · tomorrow at 9am · at 2026-12-01 09:00");

    // in 20 minutes / in 2 hours
    if words.first() == Some(&"in") && words.len() >= 2 {
        let (n, u) = match words[1].find(|c: char| c.is_alphabetic()) {
            Some(i) if i > 0 => (words[1][..i].parse::<i64>().ok(), words[1][i..].to_string()),
            _ => (words[1].parse::<i64>().ok(), words.get(2).copied().unwrap_or("").to_string()),
        };
        let d = n.and_then(|n| unit(n, &u)).ok_or_else(bad)?;
        return Ok(Rule::Once(now + d));
    }
    // tomorrow / today [at] TIME
    if matches!(words.first(), Some(&"tomorrow") | Some(&"today")) {
        let t = rest_time(1).ok_or_else(bad)?.unwrap_or(NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        let date = now.date_naive() + CDuration::days(if words[0] == "tomorrow" { 1 } else { 0 });
        let when = at(date, t).ok_or_else(bad)?;
        if when <= now {
            bail!("that time today has already passed");
        }
        return Ok(Rule::Once(when));
    }
    // at [DATE] TIME / on DATE [at] TIME
    if matches!(words.first(), Some(&"at") | Some(&"on")) && words.len() >= 2 {
        if let Ok(date) = NaiveDate::parse_from_str(words[1], "%Y-%m-%d") {
            let t = rest_time(2).ok_or_else(bad)?.unwrap_or(NaiveTime::from_hms_opt(9, 0, 0).unwrap());
            let when = at(date, t).ok_or_else(bad)?;
            if when <= now {
                bail!("{} is in the past", when.format("%Y-%m-%d %H:%M"));
            }
            return Ok(Rule::Once(when));
        }
        let t = parse_time(&words[1..].join(" ")).ok_or_else(bad)?;
        let today = at(now.date_naive(), t).ok_or_else(bad)?;
        return Ok(Rule::Once(if today > now { today } else { at(now.date_naive() + CDuration::days(1), t).ok_or_else(bad)? }));
    }
    // every ...
    let (first, rest) = match words.split_first() {
        Some((f, r)) => (*f, r),
        None => return Err(bad()),
    };
    if first != "every" && first != "daily" && first != "weekdays" {
        return Err(bad());
    }
    let rest: Vec<&str> = if first == "every" { rest.to_vec() } else { words.clone() };
    // every 2 hours / every hour / every 30 minutes / every 30m
    let (n, u, used) = match rest.first() {
        Some(w) if w.chars().next().is_some_and(|c| c.is_ascii_digit()) => match w.find(|c: char| c.is_alphabetic()) {
            Some(i) => (w[..i].parse::<i64>().ok(), w[i..].to_string(), 1),
            None => (w.parse::<i64>().ok(), rest.get(1).copied().unwrap_or("").to_string(), 2),
        },
        Some(&w) if matches!(w, "minute" | "hour") => (Some(1), w.to_string(), 1),
        _ => (None, String::new(), 0),
    };
    if let (Some(n), true) = (n, used > 0) {
        let d = unit(n, &u).ok_or_else(bad)?;
        if used != rest.len() {
            return Err(bad());
        }
        if d < MIN_EVERY {
            bail!("the shortest repeat is every {} minutes", MIN_EVERY.num_minutes());
        }
        return Ok(Rule::Every(d));
    }
    // every day / daily / every weekday / weekdays / every weekend / every monday [and friday] / every morning / every evening
    let mut days = [false; 7];
    let mut default_time = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
    let mut i = 0;
    while i < rest.len() {
        let w = rest[i].trim_end_matches('s');
        match w {
            "day" | "daily" => days = [true; 7],
            "weekday" => days = [true, true, true, true, true, false, false],
            "weekend" => days = [false, false, false, false, false, true, true],
            "morning" => {
                days = [true; 7];
                default_time = NaiveTime::from_hms_opt(8, 0, 0).unwrap();
            }
            "evening" | "night" => {
                days = [true; 7];
                default_time = NaiveTime::from_hms_opt(18, 0, 0).unwrap();
            }
            "and" => {}
            _ => match DAY_NAMES.iter().position(|d| *d == w || d[..3] == *w) {
                Some(d) => days[d] = true,
                None => break,
            },
        }
        i += 1;
    }
    if !days.iter().any(|d| *d) {
        return Err(bad());
    }
    let time = time_from(&rest, i).ok_or_else(bad)?.unwrap_or(default_time);
    Ok(Rule::Weekly { time, days })
}

impl Rule {
    /// The next run strictly after `after` (`None` for a one-off already past).
    pub fn next_after(&self, after: DateTime<Local>) -> Option<DateTime<Local>> {
        match self {
            Rule::Once(t) => (*t > after).then_some(*t),
            Rule::Every(d) => Some(after + *d),
            Rule::Weekly { time, days } => (0..=7).find_map(|k| {
                let date = after.date_naive() + CDuration::days(k);
                let t = at(date, *time)?;
                (days[date.weekday().num_days_from_monday() as usize] && t > after).then_some(t)
            }),
        }
    }

    /// Stored form (re-parsed by [`Rule::decode`]).
    pub fn encode(&self) -> String {
        match self {
            Rule::Once(t) => format!("once {}", t.to_rfc3339()),
            Rule::Every(d) => format!("every {}", d.num_seconds()),
            Rule::Weekly { time, days } => {
                let d: Vec<&str> = (0..7).filter(|i| days[*i]).map(|i| &DAY_NAMES[i][..3]).collect();
                format!("weekly {} {}", time.format("%H:%M"), d.join(","))
            }
        }
    }

    pub fn decode(s: &str) -> Option<Self> {
        let (kind, rest) = s.split_once(' ')?;
        match kind {
            "once" => DateTime::parse_from_rfc3339(rest).ok().map(|t| Rule::Once(t.with_timezone(&Local))),
            "every" => rest.parse::<i64>().ok().map(|n| Rule::Every(CDuration::seconds(n))),
            "weekly" => {
                let (t, d) = rest.split_once(' ')?;
                let time = NaiveTime::parse_from_str(t, "%H:%M").ok()?;
                let mut days = [false; 7];
                for x in d.split(',') {
                    days[DAY_NAMES.iter().position(|n| &n[..3] == x)?] = true;
                }
                Some(Rule::Weekly { time, days })
            }
            _ => None,
        }
    }

    /// Human form for listings.
    pub fn describe(&self) -> String {
        match self {
            Rule::Once(t) => format!("once, {}", t.format("%a %b %-d %H:%M")),
            Rule::Every(d) if d.num_minutes() % 60 == 0 => format!("every {} h", d.num_hours()),
            Rule::Every(d) => format!("every {} min", d.num_minutes()),
            Rule::Weekly { time, days } => {
                let which = match days {
                    [true, true, true, true, true, true, true] => "every day".to_string(),
                    [true, true, true, true, true, false, false] => "weekdays".to_string(),
                    [false, false, false, false, false, true, true] => "weekends".to_string(),
                    _ => (0..7).filter(|i| days[*i]).map(|i| {
                        let d = DAY_NAMES[i];
                        format!("{}{}", d[..1].to_uppercase(), &d[1..3])
                    }).collect::<Vec<_>>().join(", "),
                };
                format!("{which} at {}", time.format("%H:%M"))
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Schedule {
    pub id: i64,
    pub spec: String,
    pub rule: Rule,
    pub task: String,
    /// Conversation it was made in: decides where results go (`tg-<chat>` → that chat).
    pub session: String,
    pub next_run: Option<DateTime<Local>>,
    pub last_run: Option<DateTime<Local>>,
    pub last_status: Option<String>,
    pub last_result: Option<String>,
    pub runs: i64,
}

pub struct Store {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS schedules (
    id INTEGER PRIMARY KEY, spec TEXT NOT NULL, rule TEXT NOT NULL, task TEXT NOT NULL, session TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')), next_run TEXT, last_run TEXT, last_status TEXT,
    last_result TEXT, runs INTEGER NOT NULL DEFAULT 0, enabled INTEGER NOT NULL DEFAULT 1)";

fn ts(t: &Option<String>) -> Option<DateTime<Local>> {
    t.as_deref().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|t| t.with_timezone(&Local))
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let c = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(c)
    }

    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(c: Connection) -> Result<Self> {
        c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000;")?;
        c.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(c) })
    }

    pub fn add(&self, spec: &str, task: &str, session: &str, now: DateTime<Local>) -> Result<Schedule> {
        let task = task.trim();
        if task.is_empty() {
            bail!("what should wang do? The task is empty");
        }
        let rule = parse(spec, now)?;
        if self.list(None)?.len() >= MAX_ACTIVE {
            bail!("already {MAX_ACTIVE} schedules; remove one first");
        }
        let next = rule.next_after(now).context("that time has already passed")?;
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO schedules (spec, rule, task, session, next_run) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![spec.trim(), rule.encode(), task, session, next.to_rfc3339()],
        )?;
        let id = c.last_insert_rowid();
        drop(c);
        self.get(id)?.context("schedule vanished")
    }

    fn row(r: &rusqlite::Row) -> rusqlite::Result<Option<Schedule>> {
        let rule: String = r.get(2)?;
        let Some(rule) = Rule::decode(&rule) else { return Ok(None) };
        Ok(Some(Schedule {
            id: r.get(0)?,
            spec: r.get(1)?,
            rule,
            task: r.get(3)?,
            session: r.get(4)?,
            next_run: ts(&r.get(5)?),
            last_run: ts(&r.get(6)?),
            last_status: r.get(7)?,
            last_result: r.get(8)?,
            runs: r.get(9)?,
        }))
    }

    const COLS: &'static str = "id, spec, rule, task, session, next_run, last_run, last_status, last_result, runs";

    pub fn get(&self, id: i64) -> Result<Option<Schedule>> {
        let c = self.conn.lock().unwrap();
        Ok(c.query_row(&format!("SELECT {} FROM schedules WHERE id = ?1", Self::COLS), [id], Self::row).optional()?.flatten())
    }

    /// Active schedules, optionally only one conversation's.
    pub fn list(&self, session: Option<&str>) -> Result<Vec<Schedule>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare(&format!("SELECT {} FROM schedules WHERE enabled = 1 AND (?1 IS NULL OR session = ?1) ORDER BY next_run", Self::COLS))?;
        let rows = st.query_map(params![session], Self::row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.into_iter().flatten().collect())
    }

    /// Stop a schedule. With `session`, only that conversation's.
    pub fn remove(&self, id: i64, session: Option<&str>) -> Result<bool> {
        let c = self.conn.lock().unwrap();
        Ok(c.execute("UPDATE schedules SET enabled = 0 WHERE id = ?1 AND enabled = 1 AND (?2 IS NULL OR session = ?2)", params![id, session])? > 0)
    }

    /// Schedules whose next run is due.
    pub fn due(&self, now: DateTime<Local>) -> Result<Vec<Schedule>> {
        Ok(self.list(None)?.into_iter().filter(|s| s.next_run.is_some_and(|t| t <= now)).collect())
    }

    /// Move a schedule to its next run before running it (so it can't fire twice).
    pub fn advance(&self, s: &Schedule, now: DateTime<Local>) -> Result<()> {
        let next = s.rule.next_after(now);
        let c = self.conn.lock().unwrap();
        c.execute(
            "UPDATE schedules SET next_run = ?2, enabled = ?3 WHERE id = ?1",
            params![s.id, next.map(|t| t.to_rfc3339()), next.is_some() as i64],
        )?;
        Ok(())
    }

    pub fn record(&self, id: i64, at: DateTime<Local>, status: &str, result: &str) -> Result<()> {
        let snippet: String = result.chars().take(2000).collect();
        self.conn.lock().unwrap().execute(
            "UPDATE schedules SET last_run = ?2, last_status = ?3, last_result = ?4, runs = runs + 1 WHERE id = ?1",
            params![id, at.to_rfc3339(), status, snippet],
        )?;
        Ok(())
    }
}

/// Where results go.
pub struct Delivery {
    pub bot: Option<Arc<Bot>>,
    pub notifier: Option<Arc<dyn Notify>>,
}

impl Delivery {
    async fn send(&self, s: &Schedule, text: &str) {
        let short: String = s.task.chars().take(80).collect();
        if let (Some(bot), Some(chat)) = (&self.bot, s.session.strip_prefix("tg-").and_then(|c| c.parse::<i64>().ok())) {
            if let Err(e) = bot.send_long(chat, &format!("⏰ {short}\n\n{text}")).await {
                tracing::warn!(id = s.id, "telegram delivery failed: {e:#}");
            }
            return;
        }
        if let Some(n) = &self.notifier {
            let msg: String = text.chars().take(1500).collect();
            let notice = Notice { title: format!("wang ⏰ {short}"), message: msg, priority: 3, tags: vec!["alarm_clock".into()], approval_id: None };
            if let Err(e) = n.send(&notice).await {
                tracing::warn!(id = s.id, "alert delivery failed: {e:#}");
            }
        }
    }
}

/// Run one due schedule through the agent and deliver the result.
pub async fn run_one<C: ModelClient>(hub: &Hub<C>, store: &Store, delivery: &Delivery, s: &Schedule) -> Result<String> {
    let now = Local::now();
    store.advance(s, now)?;
    if s.next_run.is_some_and(|t| now - t > CATCH_UP) {
        store.record(s.id, now, "missed", "skipped: the daemon was off at the scheduled time")?;
        return Ok("missed".into());
    }
    let mut conv = Conversation::new(format!("sched-{}-{}", s.id, now.timestamp()));
    let run = tokio::time::timeout(Duration::from_secs(30 * 60), hub.agent.run_turn(&mut conv, &format!("schedule:{}", s.id), &s.task, &|_| {})).await;
    let (status, text) = match run {
        Ok(Ok(r)) => (r.outcome.clone(), r.reply),
        Ok(Err(e)) => ("error".to_string(), format!("The scheduled task failed: {e:#}")),
        Err(_) => ("timeout".to_string(), "The scheduled task took longer than 30 minutes and was stopped.".to_string()),
    };
    store.record(s.id, now, &status, &text)?;
    delivery.send(s, &text).await;
    tracing::info!(id = s.id, status, "scheduled task ran");
    Ok(status)
}

/// The daemon's scheduler loop.
pub async fn run<C: ModelClient>(hub: Arc<Hub<C>>, store: Arc<Store>, delivery: Arc<Delivery>) -> Result<()> {
    loop {
        match store.due(Local::now()) {
            Ok(due) => {
                for s in due {
                    let (hub, store, delivery) = (hub.clone(), store.clone(), delivery.clone());
                    // Each run in its own task so a slow one doesn't hold the others.
                    tokio::spawn(async move {
                        if let Err(e) = run_one(&hub, &store, &delivery, &s).await {
                            tracing::warn!(id = s.id, "scheduled task: {e:#}");
                        }
                    });
                }
            }
            Err(e) => tracing::warn!("scheduler: {e:#}"),
        }
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}

/// `wang schedule list` / tool output.
pub fn render(list: &[Schedule]) -> String {
    if list.is_empty() {
        return "No scheduled tasks.".into();
    }
    let mut out = String::new();
    for s in list {
        let next = s.next_run.map(|t| t.format("%a %b %-d %H:%M").to_string()).unwrap_or_else(|| "-".into());
        out += &format!("#{} · {} · next {next}\n   {}\n", s.id, s.rule.describe(), s.task);
        if let (Some(at), Some(st)) = (s.last_run, &s.last_status) {
            let snippet: String = s.last_result.as_deref().unwrap_or("").replace('\n', " ").chars().take(120).collect();
            out += &format!("   last: {} ({st}) {snippet}\n", at.format("%a %H:%M"));
        }
    }
    out
}

#[cfg(test)]
mod tests;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use chrono::{Local, TimeZone, Timelike};
use serde_json::json;

use super::*;
use crate::agent::{Agent, Limits};
use crate::model::testing::{Mock, resp};
use crate::notify::{Notice, Notify};
use crate::tools::ToolCtx;

/// Monday 2026-11-09, 10:00 local.
fn now() -> DateTime<Local> {
    Local.with_ymd_and_hms(2026, 11, 9, 10, 0, 0).unwrap()
}

fn next(spec: &str) -> DateTime<Local> {
    parse(spec, now()).unwrap_or_else(|e| panic!("{spec}: {e}")).next_after(now()).unwrap()
}

fn ymdhm(t: DateTime<Local>) -> String {
    t.format("%Y-%m-%d %H:%M").to_string()
}

#[test]
fn parses_plain_english() {
    let cases = [
        ("every day at 8:00", "2026-11-10 08:00"),
        ("daily at 18:30", "2026-11-09 18:30"),
        ("every weekday at 9am", "2026-11-10 09:00"),
        ("every monday at 6:30pm", "2026-11-09 18:30"),
        ("every friday and saturday at 7pm", "2026-11-13 19:00"),
        ("every weekend at noon", "2026-11-14 12:00"),
        ("every morning", "2026-11-10 08:00"),
        ("every evening", "2026-11-09 18:00"),
        ("every 2 hours", "2026-11-09 12:00"),
        ("every 30m", "2026-11-09 10:30"),
        ("every hour", "2026-11-09 11:00"),
        ("in 20 minutes", "2026-11-09 10:20"),
        ("in 2h", "2026-11-09 12:00"),
        ("tomorrow at 9am", "2026-11-10 09:00"),
        ("tomorrow", "2026-11-10 09:00"),
        ("at 17:00", "2026-11-09 17:00"),
        ("at 9:00", "2026-11-10 09:00"),
        ("at 2026-12-01 09:15", "2026-12-01 09:15"),
        ("Every Weekday at 12am", "2026-11-10 00:00"),
    ];
    for (spec, want) in cases {
        assert_eq!(ymdhm(next(spec)), want, "{spec}");
    }
}

#[test]
fn rejects_bad_specs_helpfully() {
    for bad in ["whenever", "every 5 minutes", "at 2020-01-01 09:00", "today at 9am", "every blursday", "at 25:00", "in soon"] {
        let e = parse(bad, now()).unwrap_err().to_string();
        assert!(!e.is_empty(), "{bad}");
    }
    assert!(parse("whenever", now()).unwrap_err().to_string().contains("every day at 8:00"));
    assert!(parse("every 5 minutes", now()).unwrap_err().to_string().contains("10 minutes"));
}

#[test]
fn rules_round_trip_and_describe() {
    for spec in ["every weekday at 9am", "every 2 hours", "in 20 minutes", "every friday and saturday at 7pm"] {
        let r = parse(spec, now()).unwrap();
        assert_eq!(Rule::decode(&r.encode()), Some(r.clone()), "{spec}");
    }
    assert_eq!(parse("every weekday at 9am", now()).unwrap().describe(), "weekdays at 09:00");
    assert_eq!(parse("every 2 hours", now()).unwrap().describe(), "every 2 h");
    assert_eq!(parse("every fri and sat at 7pm", now()).unwrap().describe(), "Fri, Sat at 19:00");
    // A weekly rule keeps firing week after week.
    let r = parse("every monday at 9:00", now()).unwrap();
    let a = r.next_after(now()).unwrap();
    let b = r.next_after(a).unwrap();
    assert_eq!(((b - a).num_days(), a.hour()), (7, 9));
}

#[test]
fn store_add_list_remove_due_advance() {
    let s = Store::in_memory().unwrap();
    let a = s.add("every day at 8:00", "summarize hacker news", "tg-42", now()).unwrap();
    let b = s.add("in 20 minutes", "remind me to stretch", "cli", now()).unwrap();
    assert!(s.add("every day", "  ", "cli", now()).is_err(), "empty task");
    assert_eq!(s.list(None).unwrap().len(), 2);
    assert_eq!(s.list(Some("tg-42")).unwrap().len(), 1);
    // Due at 10:30: only the one-off.
    let later = now() + CDuration::minutes(30);
    let due = s.due(later).unwrap();
    assert_eq!(due.iter().map(|d| d.id).collect::<Vec<_>>(), vec![b.id]);
    s.advance(&due[0], later).unwrap();
    assert!(s.due(later).unwrap().is_empty());
    assert_eq!(s.list(None).unwrap().len(), 1, "one-offs end after running");
    // Remove is scoped to the conversation when asked.
    assert!(!s.remove(a.id, Some("tg-99")).unwrap());
    assert!(s.remove(a.id, Some("tg-42")).unwrap());
    assert!(s.list(None).unwrap().is_empty());
    for i in 0..MAX_ACTIVE {
        s.add("every day at 8:00", &format!("t{i}"), "cli", now()).unwrap();
    }
    assert!(s.add("every day at 8:00", "one too many", "cli", now()).unwrap_err().to_string().contains("already"));
}

#[derive(Default)]
struct Inbox(Mutex<Vec<Notice>>);

impl Notify for Inbox {
    fn send<'a>(&'a self, n: &'a Notice) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        self.0.lock().unwrap().push(n.clone());
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn due_task_runs_through_the_agent_and_is_delivered() {
    let agent = Agent::new(
        Mock::new(vec![resp(json!([{"type": "text", "text": "Top story: Rust 2.0 released"}]), "end_turn", 0)]),
        Arc::new(wang_tokens::Ledger::in_memory().unwrap()),
        Arc::new(wang_trace::Tracer::in_memory().unwrap()),
        ToolCtx::new(std::env::temp_dir()),
        Limits::default(),
    );
    let hub = Hub::new(agent);
    let store = Store::in_memory().unwrap();
    let inbox = Arc::new(Inbox::default());
    let delivery = Delivery { bot: None, notifier: Some(inbox.clone()) };
    // Due now (made "in 10 minutes" from 10 minutes ago).
    let s = store.add("in 10 minutes", "summarize hacker news", "cli", Local::now() - CDuration::minutes(10)).unwrap();
    let due = store.due(Local::now()).unwrap();
    assert_eq!(due.len(), 1);
    let status = run_one(&hub, &store, &delivery, &due[0]).await.unwrap();
    assert_eq!(status, "done");
    let got = inbox.0.lock().unwrap();
    assert_eq!(got.len(), 1);
    assert!(got[0].title.contains("summarize hacker news") && got[0].message.contains("Rust 2.0"), "{:?}", got[0]);
    let after = store.get(s.id).unwrap().unwrap();
    assert_eq!((after.runs, after.last_status.as_deref()), (1, Some("done")));
    assert!(after.last_result.unwrap().contains("Rust 2.0"));
    assert!(store.list(None).unwrap().is_empty(), "the one-off is finished");
    // The run is a normal agent task in the ledger.
    assert_eq!(hub.agent.ledger.summary(1, "t").unwrap().tasks.len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn long_missed_runs_are_skipped_not_run() {
    let agent = Agent::new(
        Mock::new(vec![]),
        Arc::new(wang_tokens::Ledger::in_memory().unwrap()),
        Arc::new(wang_trace::Tracer::in_memory().unwrap()),
        ToolCtx::new(std::env::temp_dir()),
        Limits::default(),
    );
    let hub = Hub::new(agent);
    let store = Store::in_memory().unwrap();
    let delivery = Delivery { bot: None, notifier: None };
    store.add("every day at 8:00", "daily brief", "cli", Local::now() - CDuration::days(3)).unwrap();
    let due = store.due(Local::now()).unwrap();
    assert_eq!(run_one(&hub, &store, &delivery, &due[0]).await.unwrap(), "missed");
    let s = &store.list(None).unwrap()[0];
    assert!(s.next_run.unwrap() > Local::now(), "moved to the next 8:00");
}

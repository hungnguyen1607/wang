use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use wang_tokens::{Ledger, Usage};
use wang_trace::Tracer;

use super::*;
use crate::agent::{Agent, Conversation, Limits};
use crate::model::{CallCtx, ModelClient, ModelResponse};
use crate::tools::ToolCtx;

/// A fake model that answers by role (planner, checker, writer, worker), so parallel workers
/// don't depend on call order. Records what each role was sent.
struct Roles {
    plan: Value,
    replan: Value,
    /// Worker replies: first instruction keyword found in the message → reply.
    workers: Vec<(&'static str, &'static str)>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    log: Mutex<Vec<(String, String)>>,
}

impl Roles {
    fn new(plan: Value, workers: Vec<(&'static str, &'static str)>) -> Self {
        Self { plan, replan: json!({"steps": []}), workers, in_flight: AtomicUsize::new(0), max_in_flight: AtomicUsize::new(0), log: Mutex::new(vec![]) }
    }
    fn sent(&self, role: &str) -> Vec<String> {
        self.log.lock().unwrap().iter().filter(|(r, _)| r == role).map(|(_, t)| t.clone()).collect()
    }
}

fn reply(content: Value, stop: &str) -> ModelResponse {
    ModelResponse {
        content: content.as_array().unwrap().clone(),
        stop_reason: stop.into(),
        usage: Usage { input_tokens: 100, output_tokens: 20, ..Default::default() },
        model: "roles".into(),
        cost_usd: Some(0.001),
        ..Default::default()
    }
}

impl ModelClient for Roles {
    fn provider(&self) -> &'static str {
        "test"
    }
    fn model(&self) -> &str {
        "roles"
    }
    async fn create(&self, system: &str, _tools: &[Value], messages: &[Value], ctx: &CallCtx<'_>) -> anyhow::Result<ModelResponse> {
        let user = messages.last().map(|m| text_of(m["content"].as_array().map(|a| a.as_slice()).unwrap_or(&[]))).unwrap_or_default();
        let role = if system.starts_with("You plan tasks") {
            "plan"
        } else if system.starts_with("You check one step") {
            "check"
        } else if system.starts_with("You write the final answer") {
            "write"
        } else {
            "worker"
        };
        self.log.lock().unwrap().push((role.into(), user.clone()));
        Ok(match role {
            "plan" => {
                let input = if user.contains("Plan the REMAINING") { self.replan.clone() } else { self.plan.clone() };
                reply(json!([{"type": "tool_use", "id": "p", "name": "submit_plan", "input": input}]), "tool_use")
            }
            "check" => {
                let bad = user.contains("BAD");
                reply(json!([{"type": "tool_use", "id": "c", "name": "submit_verdict", "input": {"ok": !bad, "reason": if bad { "result is BAD" } else { "fine" }}}]), "tool_use")
            }
            "write" => {
                let text = format!("ANSWER from {} results", user.matches("\n### ").count());
                (ctx.on_text)(&text);
                reply(json!([{"type": "text", "text": text}]), "end_turn")
            }
            _ if user.contains("TASK-VM") && !messages.iter().any(|m| m.to_string().contains("tool_result")) => {
                // A worker that uses its VM: write a marker, then list what's in /work.
                let tag = if user.contains("TASK-VM1") { "one" } else { "two" };
                let cmd = format!("echo {tag} > /work/{tag}; sleep 1; ls /work | tr '\\n' ' '");
                reply(json!([{"type": "tool_use", "id": format!("v{tag}"), "name": "vm", "input": {"action": "run", "command": cmd}}]), "tool_use")
            }
            _ if user.contains("TASK-VM") || messages.iter().any(|m| m.to_string().contains("tool_result")) => {
                // Report what the VM printed.
                let out = messages.iter().rev().find_map(|m| m["content"].as_array()?.iter().find(|b| b["type"] == "tool_result").map(|b| b["content"].to_string())).unwrap_or_default();
                reply(json!([{"type": "text", "text": format!("vm said: {out}")}]), "end_turn")
            }
            _ => {
                let n = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_in_flight.fetch_max(n, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                let r = self.workers.iter().find(|(k, _)| user.contains(k)).map(|(_, r)| *r).unwrap_or("generic result");
                reply(json!([{"type": "text", "text": r}]), "end_turn")
            }
        })
    }
}

fn agent(m: Roles) -> Agent<Roles> {
    Agent::new(
        m,
        Arc::new(Ledger::in_memory().unwrap()),
        Arc::new(Tracer::in_memory().unwrap()),
        ToolCtx::new(std::env::temp_dir()),
        Limits { max_steps: 5, tool_output_cap: 2000, ..Default::default() },
    )
}

fn three_step_plan() -> Value {
    json!({"steps": [
        {"id": "s1", "title": "find libraries", "workflow": "research", "instruction": "TASK-A find http clients", "done_when": "lists clients"},
        {"id": "s2", "title": "check machine", "workflow": "quick", "instruction": "TASK-B check rust version", "done_when": "has a version"},
        {"id": "s3", "title": "benchmark", "workflow": "code", "instruction": "TASK-C benchmark them", "depends_on": ["s1", "s2"], "done_when": "has timings"}
    ]})
}

#[tokio::test(flavor = "current_thread")]
async fn plans_runs_parallel_workers_passes_results_and_writes() {
    let a = agent(Roles::new(three_step_plan(), vec![("TASK-A", "reqwest, ureq"), ("TASK-B", "rustc 1.95"), ("TASK-C", "reqwest 12ms, ureq 9ms")]));
    let events = Mutex::new(vec![]);
    let mut conv = Conversation::new("c1");
    let r = a.run_turn(&mut conv, "test", "/plan compare rust http clients and benchmark them", &|e| events.lock().unwrap().push(e)).await.unwrap();

    assert_eq!(r.workflow, Workflow::Plan);
    assert_eq!(r.outcome, "done");
    assert!(r.reply.starts_with("Plan:\n1. [research] find libraries\n2. [quick] check machine\n3. [code] benchmark (after s1, s2)"), "{}", r.reply);
    assert!(r.reply.ends_with("ANSWER from 3 results"), "{}", r.reply);
    // s1 and s2 ran at the same time; s3 waited for both and got their results.
    assert_eq!(a.client.max_in_flight.load(Ordering::SeqCst), 2);
    let workers = a.client.sent("worker");
    let s3 = workers.iter().find(|w| w.contains("TASK-C")).unwrap();
    assert!(s3.contains("reqwest, ureq") && s3.contains("rustc 1.95") && s3.contains("[Done when] has timings"), "{s3}");
    // Each worker got its workflow's guidance; workers never plan.
    assert!(workers.iter().find(|w| w.contains("TASK-A")).unwrap().contains("[wang workflow: research]"));
    assert_eq!(a.client.sent("plan").len(), 1);
    assert_eq!(a.client.sent("check").len(), 3);
    // Events: plan text, step start/end, the streamed answer.
    let ev = events.lock().unwrap();
    let steps: Vec<String> = ev.iter().filter_map(|e| match e { ServerMsg::ToolStart { name, detail } if name == "step" => Some(detail.clone()), _ => None }).collect();
    assert_eq!(steps.len(), 3, "{steps:?}");
    assert!(ev.iter().any(|e| matches!(e, ServerMsg::Text { delta } if delta.contains("ANSWER"))));
    // Metering: planner + 3 workers + 3 checks + writer = 8 calls; workers are their own tasks.
    assert_eq!(r.calls, 8);
    assert!((r.cost_usd - 0.008).abs() < 1e-9, "{}", r.cost_usd);
    let s = a.ledger.summary(1, "t").unwrap();
    assert_eq!(s.tasks.len(), 4);
    assert_eq!(a.ledger.stats(Some(1), "t").unwrap().tasks, 1, "workers aren't counted as tasks of their own in stats");
    // The conversation keeps only the request and the answer.
    assert_eq!(conv.messages.len(), 2);
    let spans = a.tracer.spans(r.task_id).unwrap();
    assert_eq!(spans.iter().filter(|s| s.name.starts_with("step ")).count(), 3);
}

#[tokio::test(flavor = "current_thread")]
async fn failed_step_retries_then_replans_and_skips_dependents() {
    let plan = json!({"steps": [
        {"id": "s1", "title": "fetch data", "workflow": "research", "instruction": "TASK-BAD fetch", "done_when": "has data"},
        {"id": "s2", "title": "analyze", "workflow": "code", "instruction": "TASK-X analyze", "depends_on": ["s1"], "done_when": "has numbers"}
    ]});
    let mut roles = Roles::new(plan, vec![("TASK-BAD", "BAD output"), ("TASK-ALT", "good data from mirror"), ("TASK-X", "numbers")]);
    roles.replan = json!({"steps": [
        {"id": "s3", "title": "fetch from mirror", "workflow": "research", "instruction": "TASK-ALT fetch from mirror", "done_when": "has data"},
        {"id": "s4", "title": "analyze", "workflow": "code", "instruction": "TASK-X analyze", "depends_on": ["s3"], "done_when": "has numbers"}
    ]});
    let a = agent(roles);
    let mut conv = Conversation::new("c2");
    let r = a.run_turn(&mut conv, "test", "/plan fetch the data and analyze it", &|_| {}).await.unwrap();
    // s1: tried twice (one retry with the reviewer's reason), then the rest was re-planned.
    let workers = a.client.sent("worker");
    assert_eq!(workers.iter().filter(|w| w.contains("TASK-BAD")).count(), 1);
    assert!(workers.iter().any(|w| w.contains("A reviewer checked your result and found a problem: result is BAD")));
    let plans = a.client.sent("plan");
    assert_eq!(plans.len(), 2);
    assert!(plans[1].contains("s1 (FAILED)") && plans[1].contains("Plan the REMAINING"), "{}", plans[1]);
    // The re-planned steps ran; the original s2 (behind the failure) was dropped.
    assert!(workers.iter().any(|w| w.contains("TASK-ALT")));
    assert!(r.reply.contains("Re-planned") || r.reply.contains("Plan:"));
    assert_eq!(r.outcome, "partial", "s1 itself still failed");
    let write = a.client.sent("write");
    assert!(write[0].contains("s1 [research] fetch data (failed)") && write[0].contains("s4 [code] analyze (done)"), "{}", write[0]);
}

#[tokio::test(flavor = "current_thread")]
async fn one_step_or_broken_plans_fall_back_to_the_single_loop() {
    let one = json!({"steps": [{"id": "s1", "title": "just do it", "workflow": "quick", "instruction": "TASK-Q"}]});
    let a = agent(Roles::new(one, vec![]));
    let r = a.run_turn(&mut Conversation::new("c3"), "test", "/plan list my files", &|_| {}).await.unwrap();
    assert_eq!(r.workflow, Workflow::Quick, "one step: run it directly");
    assert!(a.client.sent("write").is_empty());

    let a = agent(Roles::new(json!({"nonsense": true}), vec![]));
    let r = a.run_turn(&mut Conversation::new("c4"), "test", "/plan do two things then three more", &|_| {}).await.unwrap();
    assert_eq!(r.workflow, Workflow::General, "no usable plan: single loop");
    assert!(a.client.sent("write").is_empty());
}

#[test]
fn plan_parsing_guards() {
    let v = json!({"steps": [
        {"id": "a", "title": "t", "workflow": "plan", "instruction": "i", "depends_on": ["a", "zzz", "b"]},
        {"id": "a", "title": "dup id", "workflow": "research", "instruction": "i", "depends_on": ["a"]},
        {"id": "c", "title": "t", "workflow": "code", "instruction": "i"},
    ]});
    let p = parse_plan(&v, 2, &HashSet::new()).unwrap();
    assert_eq!(p.len(), 2, "capped at max steps");
    assert!(p[0].depends_on.is_empty(), "self, unknown and later dependencies dropped: {:?}", p[0].depends_on);
    assert_eq!(p[0].workflow(), Workflow::General, "workers never plan");
    assert_ne!(p[1].id, "a", "duplicate id renamed");
    assert_eq!(p[1].depends_on, vec!["a".to_string()]);
    assert!(parse_plan(&json!({"steps": []}), 5, &HashSet::new()).is_err());
}

/// Real micro-VMs: two parallel code steps each get their own VM (skipped without `wang vm setup`).
#[tokio::test(flavor = "current_thread")]
async fn parallel_workers_get_their_own_vms() {
    let helper = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("jail");
    let cfg = wang_vm::VmCfg::default();
    let (Ok(assets), true) = (wang_vm::Assets::find(None), helper.exists()) else {
        eprintln!("skipping: needs `wang vm setup` and {}", helper.display());
        return;
    };
    if assets.base(&cfg).is_none() {
        return;
    }
    let jail = Arc::new(wang_jail::Jail::with_helper(helper).unwrap());
    let vms = wang_vm::Sessions::new(jail, assets, cfg, 6);
    let plan = json!({"steps": [
        {"id": "a", "title": "first", "workflow": "code", "instruction": "TASK-VM1", "done_when": ""},
        {"id": "b", "title": "second", "workflow": "code", "instruction": "TASK-VM2", "done_when": ""}
    ]});
    let mut a = agent(Roles::new(plan, vec![]));
    a.ctx = ToolCtx::new(std::env::temp_dir()).with_vm(Some(vms.clone()));
    let started = std::time::Instant::now();
    let r = a.run_turn(&mut Conversation::new("vmc"), "test", "/plan two experiments", &|_| {}).await.unwrap();
    assert_eq!(r.outcome, "done", "{}", r.reply);
    let report = &a.client.sent("write")[0];
    // Each VM saw only its own file: separate machines.
    assert!(report.contains("vm said") && report.contains("one ") && report.contains("two "), "{report}");
    for line in report.lines().filter(|l| l.contains("vm said")) {
        assert!(!(line.contains("one ") && line.contains("two ")), "a worker saw the other's VM: {line}");
    }
    // Both ran at once (each sleeps 1 s inside its VM).
    assert!(started.elapsed() < std::time::Duration::from_millis(3_500), "{:?}", started.elapsed());
    vms.shutdown().await;
}

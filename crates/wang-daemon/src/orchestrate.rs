//! Multi-agent orchestration (M11): think before doing, then check while doing.
//!
//! ```text
//! task ─► planner (strong model) ─► plan: steps, each with a workflow, dependencies, "done when"
//!          │                         (optionally shown to the user for approval)
//!          ▼
//!       scheduler: steps whose dependencies are done run in parallel (max_parallel)
//!          ├─► worker agent per step: its own conversation and session (so its own browser /
//!          │   VM), its workflow's tools only, the results of the steps it depends on
//!          ├─► checker: did the result meet "done when"? no → the worker retries with the
//!          │   reason; still no → re-plan the remaining work once
//!          ▼
//!       writer (strong model): the final answer from the step results, streamed to the user
//! ```
//!
//! Every model call is metered in the ledger and traced; workers are their own ledger tasks
//! (source `worker:<task>`), so their tool calls and costs show up in `wang trace` / `wang cost`.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use wang_tokens::{CallRecord, Usage, cap_middle};
use wang_trace::{Kind, Status};

use crate::agent::{Agent, Conversation, TurnResult, text_of};
use crate::approvals::Decision;
use crate::model::{CallCtx, ModelClient, ModelResponse};
use crate::protocol::ServerMsg;
use crate::workflow::Workflow;

pub type Events<'a> = &'a (dyn Fn(ServerMsg) + Send + Sync);

#[derive(Debug, Clone)]
pub struct OrchestrationCfg {
    /// Auto-plan multi-part tasks (`/plan` always plans).
    pub enabled: bool,
    /// Worker agents running at once.
    pub max_parallel: usize,
    /// Most steps in a plan.
    pub max_steps: usize,
    /// Retries per step after a failed check.
    pub retries: u32,
    /// Re-plans of the remaining work after a step fails for good.
    pub replans: u32,
    /// Ask the user to approve the plan before any step runs.
    pub approve_plan: bool,
}

impl Default for OrchestrationCfg {
    fn default() -> Self {
        Self { enabled: true, max_parallel: 2, max_steps: 6, retries: 1, replans: 1, approve_plan: false }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Step {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub workflow: String,
    pub instruction: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub done_when: String,
}

impl Step {
    pub fn workflow(&self) -> Workflow {
        match Workflow::from_name(&self.workflow.to_ascii_lowercase()) {
            Some(Workflow::Plan) | None => Workflow::General, // workers never plan again
            Some(w) => w,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct StepOutcome {
    pub ok: bool,
    pub result: String,
    pub reason: String,
    pub attempts: u32,
    pub worker_task: Option<i64>,
    pub skipped: bool,
}

const PLANNER_SYSTEM: &str = "You plan tasks for wang, a laptop assistant whose work is done by separate worker agents. \
Break the user's task into 2 to {MAX} steps and submit them with submit_plan. Rules:\n\
- Each step is done by a worker that sees ONLY its instruction plus the results of the steps it depends on, so write self-contained instructions (include names, URLs, numbers from the task).\n\
- Pick each step's workflow: research (web pages and browser; cite sources), code (writes and runs code in a throwaway VM; can try alternatives in parallel), quick (small local lookups with shell and files), general (anything else).\n\
- Steps that don't depend on each other run in parallel: only add depends_on when a step really needs another's result.\n\
- done_when says how a checker can tell the step succeeded (e.g. 'lists 3 libraries with links', 'prints timings for both').\n\
- Don't add a final 'write the answer' step: wang writes the answer from the step results.\n\
- If the task really is one step, submit one step.";

const CHECKER_SYSTEM: &str = "You check one step of a plan. Given the step's instruction, its success criterion and the worker's result, \
decide whether the result meets the criterion. Be strict about missing parts and invented facts, lenient about style. \
Call submit_verdict with ok and a one-sentence reason (what is missing or wrong, if anything).";

const WRITER_SYSTEM: &str = "You write the final answer to the user's task from the results of the steps wang's worker agents completed. \
Answer the task directly; keep useful specifics (numbers, links, code, commands). If a step failed or was skipped, say what is missing. \
Don't describe the plan or the workers unless it matters.";

fn plan_tool(max: usize) -> Value {
    json!({
        "name": "submit_plan",
        "description": "Submit the plan.",
        "input_schema": {
            "type": "object",
            "properties": {"steps": {"type": "array", "maxItems": max, "items": {
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "short id, e.g. s1"},
                    "title": {"type": "string"},
                    "workflow": {"type": "string", "enum": ["research", "code", "quick", "general"]},
                    "instruction": {"type": "string"},
                    "depends_on": {"type": "array", "items": {"type": "string"}},
                    "done_when": {"type": "string"}
                },
                "required": ["id", "title", "workflow", "instruction"]
            }}},
            "required": ["steps"]
        }
    })
}

fn verdict_tool() -> Value {
    json!({
        "name": "submit_verdict",
        "description": "Submit the check result.",
        "input_schema": {"type": "object", "properties": {"ok": {"type": "boolean"}, "reason": {"type": "string"}}, "required": ["ok", "reason"]}
    })
}

/// The input of a structured-output tool call, or a JSON object in the reply text.
fn structured(resp: &ModelResponse, tool: &str) -> Option<Value> {
    if let Some(b) = resp.content.iter().find(|b| b["type"] == "tool_use" && b["name"] == tool) {
        return Some(b["input"].clone());
    }
    let t = text_of(&resp.content);
    let (a, b) = (t.find('{')?, t.rfind('}')?);
    serde_json::from_str(&t[a..=b]).ok()
}

/// Parse and check a plan: unique ids, dependencies on earlier steps only (no cycles), size cap.
pub fn parse_plan(v: &Value, max: usize, existing: &HashSet<String>) -> Result<Vec<Step>> {
    let steps: Vec<Step> = serde_json::from_value(v["steps"].clone())?;
    if steps.is_empty() {
        bail!("empty plan");
    }
    let mut seen: HashSet<String> = existing.clone();
    let mut out = vec![];
    for mut s in steps.into_iter().take(max) {
        if s.id.trim().is_empty() || seen.contains(&s.id) {
            s.id = format!("s{}", seen.len() + 1);
        }
        s.depends_on.retain(|d| seen.contains(d) && d != &s.id);
        seen.insert(s.id.clone());
        out.push(s);
    }
    Ok(out)
}

pub fn render_plan(steps: &[Step]) -> String {
    let mut s = String::from("Plan:\n");
    for (i, st) in steps.iter().enumerate() {
        let deps = if st.depends_on.is_empty() { String::new() } else { format!(" (after {})", st.depends_on.join(", ")) };
        s += &format!("{}. [{}] {}{deps}\n", i + 1, st.workflow().name(), st.title);
    }
    s
}

struct Meter {
    usage: Usage,
    cost: f64,
    calls: u32,
}

impl<C: ModelClient> Agent<C> {
    /// One planner / checker / writer call: metered and traced under the orchestrator's task.
    #[allow(clippy::too_many_arguments)]
    async fn meta_call(
        &self,
        task_id: i64,
        parent: Option<i64>,
        what: &str,
        system: &str,
        tools: &[Value],
        user: String,
        strong: bool,
        session: &str,
        on_text: &(dyn Fn(&str) + Send + Sync),
        m: &mut Meter,
    ) -> Result<ModelResponse> {
        let span = self.span(task_id, parent, Kind::ModelCall, what, json!({"gen_ai.operation.name": "chat", "wang.orchestration.role": what}));
        let t = Instant::now();
        // step 1 = the strong model under routing; later steps = the cheap one.
        let ctx = CallCtx { session, on_text, step: if strong { 1 } else { 2 } };
        let msgs = vec![json!({"role": "user", "content": [{"type": "text", "text": user}]})];
        let resp = match self.client.create(system, tools, &msgs, &ctx).await {
            Ok(r) => r,
            Err(e) => {
                self.end(span, Status::Error, json!({"error.message": format!("{e:#}")}));
                return Err(e);
            }
        };
        let c = self.ledger.record_call(
            task_id,
            &CallRecord {
                model: &resp.model,
                usage: resp.usage,
                provider_cost: resp.cost_usd,
                latency_ms: t.elapsed().as_millis() as u64,
                ttft_ms: resp.ttft_ms,
                stop_reason: what,
            },
        )?;
        m.usage.add(&resp.usage);
        m.cost += c;
        m.calls += 1;
        self.end(span, Status::Ok, json!({"gen_ai.response.model": resp.model, "wang.cost_usd": c,
                                           "gen_ai.usage.input_tokens": resp.usage.prompt_tokens(), "gen_ai.usage.output_tokens": resp.usage.output_tokens}));
        Ok(resp)
    }

    async fn plan(&self, task_id: i64, root: Option<i64>, session: &str, prompt: String, existing: &HashSet<String>, m: &mut Meter) -> Result<Vec<Step>> {
        let max = self.orchestration.max_steps.max(1);
        let system = PLANNER_SYSTEM.replace("{MAX}", &max.to_string());
        let resp = self.meta_call(task_id, root, "plan", &system, &[plan_tool(max)], prompt, true, session, &|_| {}, m).await?;
        let v = structured(&resp, "submit_plan").ok_or_else(|| anyhow::anyhow!("the planner didn't return a plan"))?;
        parse_plan(&v, max, existing)
    }

    /// Check a step's result; a failed checker call counts as a pass (it must not block the task).
    async fn check(&self, task_id: i64, parent: Option<i64>, session: &str, step: &Step, result: &str, m: &mut Meter) -> (bool, String) {
        if step.done_when.trim().is_empty() {
            return (!result.trim().is_empty(), "no criterion given".into());
        }
        let prompt = format!(
            "Step: {}\nInstruction: {}\nSuccess criterion: {}\n\nWorker's result:\n{}",
            step.title,
            step.instruction,
            step.done_when,
            cap_middle(result, 6_000).text
        );
        match self.meta_call(task_id, parent, "check", CHECKER_SYSTEM, &[verdict_tool()], prompt, false, session, &|_| {}, m).await {
            Ok(r) => match structured(&r, "submit_verdict") {
                Some(v) => (v["ok"].as_bool().unwrap_or(true), v["reason"].as_str().unwrap_or("").to_string()),
                None => (true, "checker gave no verdict".into()),
            },
            Err(e) => (true, format!("checker failed: {e:#}")),
        }
    }

    /// One step: a worker agent in its own conversation (and so its own browser / VM session).
    async fn run_step(&self, task_id: i64, root: Option<i64>, conv_id: &str, step: &Step, context: String, events: Events<'_>) -> (StepOutcome, Meter) {
        let mut m = Meter { usage: Usage::default(), cost: 0.0, calls: 0 };
        let wf = step.workflow();
        let span = self.span(task_id, root, Kind::Step, &format!("step {}: {}", step.id, step.title),
            json!({"wang.step.id": step.id, "wang.step.workflow": wf.name(), "wang.step.depends_on": step.depends_on}));
        let started = Instant::now();
        events(ServerMsg::ToolStart { name: "step".into(), detail: format!("{} [{}] {}", step.id, wf.name(), step.title) });
        // Worker events: tool activity goes to the user, labelled with the step; its text doesn't
        // (the final answer is written from all the results).
        let sid = step.id.clone();
        let fwd = move |msg: ServerMsg| match msg {
            ServerMsg::ToolStart { name, detail } => events(ServerMsg::ToolStart { name: format!("{sid}·{name}"), detail }),
            ServerMsg::ToolEnd { name, ok, ms, trimmed, repeated } => {
                events(ServerMsg::ToolEnd { name: format!("{sid}·{name}"), ok, ms, trimmed, repeated })
            }
            m @ (ServerMsg::ApprovalRequest { .. } | ServerMsg::ApprovalResult { .. }) => events(m),
            _ => {}
        };
        let mut wconv = Conversation::new(format!("{conv_id}/{}", step.id));
        let source = format!("worker:{task_id}");
        let mut msg = step.instruction.clone();
        if !context.is_empty() {
            msg += &format!("\n\n[Results of earlier steps]\n{context}");
        }
        if !step.done_when.is_empty() {
            msg += &format!("\n\n[Done when] {}", step.done_when);
        }
        msg += "\n\nReply with the result itself (facts, numbers, links, code), concisely.";
        let mut out = StepOutcome::default();
        let max_attempts = 1 + self.orchestration.retries;
        while out.attempts < max_attempts {
            out.attempts += 1;
            let r = self.run_loop(&mut wconv, &source, wf, &msg, &fwd).await;
            match r {
                Ok(t) => {
                    m.usage.add(&t.usage);
                    m.cost += t.cost_usd;
                    m.calls += t.calls;
                    out.worker_task = Some(t.task_id);
                    out.result = t.reply;
                }
                Err(e) => {
                    out.result = String::new();
                    out.reason = format!("worker failed: {e:#}");
                    continue;
                }
            }
            let (ok, reason) = self.check(task_id, span, &wconv.id, step, &out.result, &mut m).await;
            out.ok = ok;
            out.reason = reason;
            if ok {
                break;
            }
            msg = format!("A reviewer checked your result and found a problem: {}\nFix it and reply with the corrected result.", out.reason);
        }
        events(ServerMsg::ToolEnd { name: "step".into(), ok: out.ok, ms: started.elapsed().as_millis() as u64, trimmed: false, repeated: false });
        self.end(span, if out.ok { Status::Ok } else { Status::Error },
            json!({"wang.step.ok": out.ok, "wang.step.attempts": out.attempts, "wang.step.reason": out.reason,
                   "wang.step.worker_task": out.worker_task, "wang.cost_usd": m.cost}));
        // Free the step's browser / VM.
        if let Some(p) = &self.ctx.browser {
            p.release(&wconv.id).await;
        }
        if let Some(v) = &self.ctx.vm {
            v.reset(&wconv.id).await;
        }
        (out, m)
    }

    /// Plan the task, run the steps with worker agents, check them, write the answer.
    pub async fn orchestrate(&self, conv: &mut Conversation, source: &str, text: &str, events: Events<'_>) -> Result<TurnResult> {
        let task_id = self.ledger.start_task(source, text)?;
        let root = self.span(task_id, None, Kind::Task, "task",
            json!({"wang.goal": text.chars().take(500).collect::<String>(), "wang.source": source, "wang.session": conv.id,
                   "wang.workflow": "plan", "gen_ai.request.model": self.client.model()}));
        let mut m = Meter { usage: Usage::default(), cost: 0.0, calls: 0 };
        let budget = if self.limits.budget_usd > 0.0 { self.limits.budget_usd * 3.0 } else { f64::INFINITY };

        // 1. Plan. Recent conversation gives the planner context for follow-ups.
        let recent: String = conv
            .messages
            .iter()
            .rev()
            .take(4)
            .rev()
            .map(|msg| format!("{}: {}", msg["role"].as_str().unwrap_or("?"), cap_middle(&text_of(msg["content"].as_array().map(|a| a.as_slice()).unwrap_or(&[])), 800).text))
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = if recent.is_empty() { format!("Task: {text}") } else { format!("Recent conversation:\n{recent}\n\nTask: {text}") };
        let steps = match self.plan(task_id, root, &conv.id, prompt, &HashSet::new(), &mut m).await {
            Ok(s) if s.len() > 1 => s,
            Ok(s) => {
                // One step: no orchestration overhead beyond the planner call.
                let wf = s.first().map(|s| s.workflow()).unwrap_or(Workflow::General);
                self.end(root, Status::Ok, json!({"wang.plan.steps": 1}));
                let _ = self.ledger.finish_task(task_id, m.calls, "done");
                let mut r = self.run_loop(conv, source, wf, text, events).await?;
                r.cost_usd += m.cost;
                r.calls += m.calls;
                r.usage.add(&m.usage);
                return Ok(r);
            }
            Err(e) => {
                tracing::warn!("planning failed ({e:#}); running the task in one loop");
                self.end(root, Status::Error, json!({"error.message": format!("{e:#}")}));
                let _ = self.ledger.finish_task(task_id, m.calls, "plan_failed");
                return self.run_loop(conv, source, Workflow::General, text, events).await;
            }
        };
        let mut steps = steps;
        let plan_text = render_plan(&steps);
        if let Some(r) = root {
            let _ = self.tracer.annotate(r, json!({"wang.plan": plan_text, "wang.plan.steps": steps.len()}));
        }
        events(ServerMsg::Text { delta: format!("{plan_text}\n") });

        // 2. Optional approval of the whole plan.
        if let (true, Some(appr)) = (self.orchestration.approve_plan, &self.approvals) {
            let (aid, rx) = appr.open();
            events(ServerMsg::ApprovalRequest { id: aid.clone(), tool: "plan".into(), detail: plan_text.clone() });
            appr.notify(&aid, task_id, &plan_text).await;
            let (decision, _) = appr.wait(&aid, rx).await;
            events(ServerMsg::ApprovalResult { id: aid, decision: decision.as_str().to_string() });
            if decision != Decision::Approved {
                let reply = format!("{plan_text}\nPlan not approved, so nothing was run. Tell me what to change.");
                self.end(root, Status::Error, json!({"wang.plan.approved": false}));
                let _ = self.ledger.finish_task(task_id, m.calls, "plan_denied");
                conv.push_user_text(text);
                conv.messages.push(json!({"role": "assistant", "content": [{"type": "text", "text": reply}]}));
                return Ok(TurnResult { reply, usage: m.usage, cost_usd: m.cost, calls: m.calls, task_id, outcome: "plan_denied".into(), workflow: Workflow::Plan });
            }
        }

        // 3. Run: every step whose dependencies succeeded, up to max_parallel at once.
        let mut done: HashMap<String, StepOutcome> = HashMap::new();
        let mut replans = 0;
        let mut over_budget = false;
        loop {
            let pending: Vec<&Step> = steps.iter().filter(|s| !done.contains_key(&s.id)).collect();
            if pending.is_empty() {
                break;
            }
            let ready: Vec<Step> = pending
                .iter()
                .filter(|s| s.depends_on.iter().all(|d| done.get(d).is_some_and(|o| o.ok)))
                .take(self.orchestration.max_parallel.max(1))
                .map(|s| (*s).clone())
                .collect();
            if ready.is_empty() || m.cost >= budget {
                over_budget |= m.cost >= budget;
                // The rest depend on failed steps (or the budget ran out): skip them.
                for s in pending {
                    done.insert(s.id.clone(), StepOutcome { skipped: true, reason: if over_budget { "budget".into() } else { "depends on a failed step".into() }, ..Default::default() });
                }
                break;
            }
            let runs = futures::future::join_all(ready.iter().map(|s| {
                let ctx: String = s
                    .depends_on
                    .iter()
                    .filter_map(|d| done.get(d).map(|o| (d, o)))
                    .map(|(d, o)| format!("### {d}\n{}", cap_middle(&o.result, 3_000).text))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                self.run_step(task_id, root, &conv.id, s, ctx, events)
            }))
            .await;
            let mut failed = vec![];
            for (s, (o, sm)) in ready.iter().zip(runs) {
                m.usage.add(&sm.usage);
                m.cost += sm.cost;
                m.calls += sm.calls;
                if !o.ok {
                    failed.push((s.clone(), o.reason.clone()));
                }
                done.insert(s.id.clone(), o);
            }
            // A step failed for good: re-plan what's left, once.
            if let Some((fs, why)) = failed.first()
                && replans < self.orchestration.replans
                && m.cost < budget
            {
                replans += 1;
                let progress: String = steps
                    .iter()
                    .filter_map(|s| done.get(&s.id).map(|o| (s, o)))
                    .map(|(s, o)| format!("- {} ({}): {}{}", s.id, if o.ok { "done" } else { "FAILED" }, s.title,
                        if o.ok { format!("\n  result: {}", cap_middle(&o.result, 600).text.replace('\n', " ")) } else { format!("\n  why: {}", o.reason) }))
                    .collect::<Vec<_>>()
                    .join("\n");
                let prompt = format!(
                    "Task: {text}\n\nProgress so far:\n{progress}\n\nStep {} failed: {why}\nPlan the REMAINING work only, with new step ids. Steps may depend on the done steps above.",
                    fs.id
                );
                let existing: HashSet<String> = done.keys().cloned().collect();
                match self.plan(task_id, root, &conv.id, prompt, &existing, &mut m).await {
                    Ok(new) => {
                        steps.retain(|s| done.contains_key(&s.id));
                        let note = render_plan(&new).replacen("Plan:", "Re-planned the rest:", 1);
                        events(ServerMsg::Text { delta: format!("{note}\n") });
                        if let Some(r) = root {
                            let _ = self.tracer.annotate(r, json!({"wang.replan": note}));
                        }
                        steps.extend(new);
                    }
                    Err(e) => tracing::warn!("re-plan failed: {e:#}"),
                }
            }
        }

        // 4. Write the answer from the results.
        let mut report = format!("Task: {text}\n\nStep results:\n");
        for s in &steps {
            if let Some(o) = done.get(&s.id) {
                let status = if o.ok { "done" } else if o.skipped { "skipped" } else { "failed" };
                report += &format!("\n### {} [{}] {} ({status})\n", s.id, s.workflow().name(), s.title);
                if o.ok || !o.result.is_empty() {
                    report += &cap_middle(&o.result, 4_000).text;
                    report.push('\n');
                }
                if !o.ok {
                    report += &format!("(why: {})\n", o.reason);
                }
            }
        }
        let on_text = |t: &str| events(ServerMsg::Text { delta: t.to_string() });
        let answer = match self.meta_call(task_id, root, "write", WRITER_SYSTEM, &[], report.clone(), true, &conv.id, &on_text, &mut m).await {
            Ok(r) => text_of(&r.content),
            Err(e) => {
                // Without the writer, hand over the raw results rather than nothing.
                let fallback = format!("(Couldn't write a summary: {e:#}. Raw step results below.)\n\n{report}");
                events(ServerMsg::Text { delta: fallback.clone() });
                fallback
            }
        };
        let ok_steps = steps.iter().filter(|s| done.get(&s.id).is_some_and(|o| o.ok)).count();
        let outcome = if over_budget { "budget_paused" } else if ok_steps == steps.len() { "done" } else { "partial" };
        self.end(root, if outcome == "done" { Status::Ok } else { Status::Error },
            json!({"wang.plan.steps": steps.len(), "wang.plan.ok_steps": ok_steps, "wang.plan.replans": replans,
                   "wang.cost_usd": m.cost, "wang.outcome": outcome}));
        let _ = self.ledger.finish_task(task_id, m.calls, outcome);
        // The conversation keeps the request and the answer (not the workers' internals).
        conv.push_user_text(text);
        conv.messages.push(json!({"role": "assistant", "content": [{"type": "text", "text": format!("{plan_text}\n{answer}")}]}));
        Ok(TurnResult {
            reply: format!("{plan_text}\n{answer}"),
            usage: m.usage,
            cost_usd: m.cost,
            calls: m.calls,
            task_id,
            outcome: outcome.into(),
            workflow: Workflow::Plan,
        })
    }
}

#[cfg(test)]
mod tests;

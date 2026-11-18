//! Brains on your own subscription: Claude Code (Claude Pro/Max) or Codex (ChatGPT Plus/Pro)
//! does the thinking; wang stays the hands.
//!
//! One turn = one headless CLI run (`claude -p` / `codex exec`), resumed per conversation. The CLI
//! is given **only wang's tools**, through `wang mcp --via-daemon`: a tiny MCP proxy that sends
//! every tool call back to this daemon, so the call goes through the same approval gate (✅/❌
//! on your phone), sandbox, browser logins, ledger and trace as wang's own agent.
//!
//! Defense in depth: Claude Code runs with its built-in tools switched off (`--tools ""`), and
//! the CLI itself runs inside wang's jail: it sees only its own login/state folder and the
//! daemon socket (not your files), and reaches only its provider's domains.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use wang_trace::{Kind as SpanKind, Status};

use crate::agent::{Agent, Conversation, Events, TurnResult};
use crate::model::ModelClient;
use wang_tokens::Usage;
use crate::protocol::ServerMsg;
use crate::tools;
use crate::workflow::{self, Workflow};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Claude,
    Codex,
}

impl Kind {
    /// `[model] provider` → CLI brain.
    pub fn from_provider(p: &str) -> Option<Kind> {
        match p {
            "claude-code" | "claude" => Some(Kind::Claude),
            "codex" | "chatgpt" => Some(Kind::Codex),
            _ => None,
        }
    }
    pub fn provider(self) -> &'static str {
        match self {
            Kind::Claude => "claude-code",
            Kind::Codex => "codex",
        }
    }
    /// What the user pays with.
    pub fn plan(self) -> &'static str {
        match self {
            Kind::Claude => "Claude subscription",
            Kind::Codex => "ChatGPT subscription",
        }
    }
    pub fn bin(self) -> &'static str {
        match self {
            Kind::Claude => "claude",
            Kind::Codex => "codex",
        }
    }
    /// The CLI's own login + state, the only part of your home it can see (read-write).
    pub fn state_paths(self, home: &Path) -> Vec<PathBuf> {
        match self {
            Kind::Claude => vec![home.join(".claude"), home.join(".claude.json")],
            Kind::Codex => vec![home.join(".codex")],
        }
    }
    /// Where the CLI may connect (through the jail's egress proxy).
    pub fn domains(self) -> Vec<String> {
        let d: &[&str] = match self {
            Kind::Claude => &["api.anthropic.com", "*.anthropic.com", "claude.ai", "*.claude.ai", "claude.com", "*.claude.com"],
            Kind::Codex => &["chatgpt.com", "*.chatgpt.com", "api.openai.com", "*.openai.com", "auth.openai.com"],
        };
        d.iter().map(|s| s.to_string()).collect()
    }
}

/// How the brain introduces itself and what it may use. The persona's system prompt (rules
/// included) is added on top.
const BRAIN_RULES: &str = "\
You are running as wang's brain. You act on the user's computer only through the wang MCP tools \
(shell, read_file, web_fetch, browser, vm, remember, recall, schedule, ...); you have no other tools. \
The user usually writes from their phone, so finish the task end to end and reply briefly. \
Some actions pause for the user's approval on their phone; if one is denied, don't retry it, ask instead.";

/// One parsed line of a CLI's JSON stream.
#[derive(Debug, Clone, PartialEq)]
pub enum Ev {
    Session(String),
    /// A new assistant message started (separate it from the previous text).
    NewMessage,
    Text(String),
    /// The turn finished. `result` is the final reply when the CLI reports one.
    Done { result: Option<String>, api_equiv_usd: f64, usage: Usage },
    Error(String),
    /// Subscription usage window (Claude): `status` allowed | allowed_warning | rejected.
    Limit { status: String, kind: String, utilization: Option<f64>, resets_at: Option<i64> },
}

/// Claude Code `--output-format stream-json --verbose --include-partial-messages`.
pub fn parse_claude(line: &str) -> Vec<Ev> {
    let Ok(v) = serde_json::from_str::<Value>(line) else { return vec![] };
    match v["type"].as_str() {
        Some("system") if v["subtype"] == "init" => v["session_id"].as_str().map(|s| vec![Ev::Session(s.into())]).unwrap_or_default(),
        Some("stream_event") => {
            // Sub-agent chatter (parent_tool_use_id set) isn't part of the reply.
            if !v["parent_tool_use_id"].is_null() {
                return vec![];
            }
            let e = &v["event"];
            match e["type"].as_str() {
                Some("message_start") => vec![Ev::NewMessage],
                Some("content_block_delta") if e["delta"]["type"] == "text_delta" => {
                    e["delta"]["text"].as_str().map(|t| vec![Ev::Text(t.into())]).unwrap_or_default()
                }
                _ => vec![],
            }
        }
        Some("rate_limit_event") => {
            let i = &v["rate_limit_info"];
            let kind = i["rateLimitType"].as_str().unwrap_or("").to_string();
            let utilization = i["unifiedWindows"][&kind]["utilization"].as_f64().or_else(|| i["utilization"].as_f64());
            vec![Ev::Limit { status: i["status"].as_str().unwrap_or("").into(), kind, utilization, resets_at: i["resetsAt"].as_i64() }]
        }
        Some("result") => {
            let u = &v["usage"];
            let usage = Usage {
                input_tokens: u["input_tokens"].as_u64().unwrap_or(0),
                output_tokens: u["output_tokens"].as_u64().unwrap_or(0),
                cache_read_tokens: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
                cache_write_tokens: u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
                ..Default::default()
            };
            if v["is_error"] == true || v["subtype"].as_str().is_some_and(|s| s.starts_with("error")) {
                let msg = v["result"].as_str().filter(|s| !s.is_empty()).map(String::from).unwrap_or_else(|| format!("Claude Code stopped: {}", v["subtype"].as_str().unwrap_or("error")));
                return vec![Ev::Error(msg)];
            }
            vec![Ev::Done { result: v["result"].as_str().map(String::from), api_equiv_usd: v["total_cost_usd"].as_f64().unwrap_or(0.0), usage }]
        }
        _ => vec![],
    }
}

/// Codex `exec --json` (JSON lines: thread.started, item.*, turn.completed / turn.failed).
pub fn parse_codex(line: &str) -> Vec<Ev> {
    let Ok(v) = serde_json::from_str::<Value>(line) else { return vec![] };
    match v["type"].as_str() {
        Some("thread.started") => v["thread_id"].as_str().map(|s| vec![Ev::Session(s.into())]).unwrap_or_default(),
        Some("item.completed") if v["item"]["type"] == "agent_message" => match v["item"]["text"].as_str() {
            Some(t) if !t.is_empty() => vec![Ev::NewMessage, Ev::Text(t.into())],
            _ => vec![],
        },
        Some("turn.completed") => {
            let u = &v["usage"];
            let usage = Usage {
                input_tokens: u["input_tokens"].as_u64().unwrap_or(0).saturating_sub(u["cached_input_tokens"].as_u64().unwrap_or(0)),
                cache_read_tokens: u["cached_input_tokens"].as_u64().unwrap_or(0),
                output_tokens: u["output_tokens"].as_u64().unwrap_or(0),
                ..Default::default()
            };
            vec![Ev::Done { result: None, api_equiv_usd: 0.0, usage }]
        }
        Some("turn.failed") => vec![Ev::Error(v["error"]["message"].as_str().unwrap_or("Codex turn failed").into())],
        Some("error") => vec![Ev::Error(v["message"].as_str().unwrap_or("Codex error").into())],
        _ => vec![],
    }
}

/// A brain CLI and how to run it.
pub struct CliBrain {
    pub kind: Kind,
    /// The CLI binary (resolved on PATH at startup).
    pub bin: PathBuf,
    /// Model to ask for (`--model`/`-m`); None = the CLI's default for your plan.
    pub model: Option<String>,
    /// This wang binary, for the MCP proxy.
    pub wang_exe: PathBuf,
    /// The daemon socket the proxy talks to.
    pub socket: PathBuf,
    /// Run the CLI in wang's jail (on when the sandbox is).
    pub jail: Option<Arc<wang_jail::Jail>>,
    /// Max time for one turn.
    pub timeout: Duration,
}

/// Where the jailed CLI finds the daemon socket and wang binary.
const JAIL_SOCKET: &str = "/run/wang/wang.sock";
const JAIL_WANG: &str = "/run/wang/wang";

pub fn which(cmd: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?.to_str()?.split(':').map(|d| Path::new(d).join(cmd)).find(|p| p.is_file())
}

impl CliBrain {
    pub fn new(kind: Kind, model: Option<String>, jail: Option<Arc<wang_jail::Jail>>) -> Result<Self> {
        let bin = which(kind.bin()).with_context(|| match kind {
            Kind::Claude => "provider = \"claude-code\" needs Claude Code: npm i -g @anthropic-ai/claude-code, then `claude` once to sign in",
            Kind::Codex => "provider = \"codex\" needs Codex: npm i -g @openai/codex, then `codex login`",
        })?;
        let wang_exe = std::env::current_exe()?.canonicalize()?;
        Ok(Self { kind, bin, model, wang_exe, socket: crate::config::socket_path(), jail, timeout: Duration::from_secs(30 * 60) })
    }

    fn proxy_cmd(&self, session: &str) -> (String, Vec<String>) {
        let (exe, sock) = if self.jail.is_some() {
            (JAIL_WANG.to_string(), JAIL_SOCKET.to_string())
        } else {
            (self.wang_exe.to_string_lossy().into_owned(), self.socket.to_string_lossy().into_owned())
        };
        (exe, vec!["mcp".into(), "--via-daemon".into(), "--session".into(), session.into(), "--socket".into(), sock])
    }

    /// The CLI command line for one turn.
    pub fn argv(&self, session: &str, resume: Option<&str>, system: &str, prompt: &str) -> Vec<String> {
        let bin = self.bin.to_string_lossy().into_owned();
        let (exe, args) = self.proxy_cmd(session);
        match self.kind {
            Kind::Claude => {
                let mcp = json!({"mcpServers": {"wang": {"type": "stdio", "command": exe, "args": args}}}).to_string();
                let mut a = vec![
                    bin,
                    "-p".into(),
                    prompt.into(),
                    "--output-format".into(),
                    "stream-json".into(),
                    "--verbose".into(),
                    "--include-partial-messages".into(),
                    // No built-in tools at all: only wang's (sandboxed, approved) ones.
                    "--tools".into(),
                    "".into(),
                    "--strict-mcp-config".into(),
                    "--mcp-config".into(),
                    mcp,
                    "--allowedTools".into(),
                    "mcp__wang__*".into(),
                    "--permission-mode".into(),
                    "dontAsk".into(),
                    // Not the user's own Claude Code settings, hooks or plugins.
                    "--setting-sources".into(),
                    "".into(),
                    "--append-system-prompt".into(),
                    system.into(),
                ];
                if let Some(m) = &self.model {
                    a.extend(["--model".into(), m.clone()]);
                }
                if let Some(r) = resume {
                    a.extend(["--resume".into(), r.into()]);
                }
                a
            }
            Kind::Codex => {
                let toml_args = format!("[{}]", args.iter().map(|s| format!("{s:?}")).collect::<Vec<_>>().join(","));
                let mut a = vec![bin, "exec".into()];
                if let Some(r) = resume {
                    a.extend(["resume".into(), r.into()]);
                }
                a.extend([
                    "--json".into(),
                    "--skip-git-repo-check".into(),
                    // Codex's own shell can't write or reach the network; wang's tools do the work.
                    "-s".into(),
                    "read-only".into(),
                    "-c".into(),
                    format!("mcp_servers.wang.command={exe:?}"),
                    "-c".into(),
                    format!("mcp_servers.wang.args={toml_args}"),
                    "-c".into(),
                    "mcp_servers.wang.tool_timeout_sec=900".into(),
                ]);
                if let Some(m) = &self.model {
                    a.extend(["-m".into(), m.clone()]);
                }
                // Codex has no system-prompt flag: the instructions lead the first message.
                let p = if resume.is_none() { format!("[Instructions for this conversation]\n{system}\n\n[User]\n{prompt}") } else { prompt.to_string() };
                a.push(p);
                a
            }
        }
    }

    pub fn parse(&self, line: &str) -> Vec<Ev> {
        match self.kind {
            Kind::Claude => parse_claude(line),
            Kind::Codex => parse_codex(line),
        }
    }

    /// The jail for one CLI run: its own state folder, the socket and wang binary, the CLI
    /// install, provider domains. None of your files.
    pub fn policy(&self) -> wang_jail::Policy {
        let home = dirs::home_dir().unwrap_or_default();
        let install = std::fs::canonicalize(&self.bin).unwrap_or_else(|_| self.bin.clone());
        // The CLI's package folder (node_modules/<pkg>/...), or just its own directory.
        let pkg = install.ancestors().find(|a| a.parent().is_some_and(|p| p.file_name().is_some_and(|n| n == "@anthropic-ai" || n == "@openai"))).map(Path::to_path_buf);
        let mut read = vec![install.parent().map(Path::to_path_buf).unwrap_or_default()];
        read.extend(pkg);
        if let Some(d) = self.bin.parent() {
            read.push(d.to_path_buf());
        }
        let mut binds: Vec<wang_jail::Bind> = self
            .kind
            .state_paths(&home)
            .into_iter()
            .filter(|p| p.exists())
            .map(|p| wang_jail::Bind { src: p.clone(), dst: p, writable: true })
            .collect();
        binds.push(wang_jail::Bind { src: self.socket.clone(), dst: JAIL_SOCKET.into(), writable: true });
        binds.push(wang_jail::Bind { src: self.wang_exe.clone(), dst: JAIL_WANG.into(), writable: false });
        wang_jail::Policy {
            read,
            domains: self.kind.domains(),
            binds,
            env: vec![("WANG_SOCKET".into(), JAIL_SOCKET.into()), ("DISABLE_AUTOUPDATER".into(), "1".into())],
            limits: wang_jail::Limits {
                memory_bytes: 2 << 30,
                cpus: 2.0,
                pids: 1024,
                timeout: self.timeout,
                tmp_bytes: 256 << 20,
                max_output_bytes: 64 << 10,
            },
            ..Default::default()
        }
    }
}

/// A tool call from the brain's MCP proxy lands in the turn that's running for its session.
pub struct CliTurn {
    pub task_id: i64,
    pub root: Option<i64>,
    pub events: tokio::sync::mpsc::UnboundedSender<ServerMsg>,
    pub calls: Arc<std::sync::atomic::AtomicU32>,
}

/// Lines from either a jailed or a plain child process.
enum Child {
    Jailed(wang_jail::run::Running),
    Plain(tokio::process::Child),
}

impl Child {
    async fn finish(self) -> (i32, String) {
        match self {
            Child::Jailed(r) => {
                let s = r.stop().await;
                let mut err = String::from_utf8_lossy(&s.stderr).to_string();
                if !s.blocked_hosts.is_empty() {
                    err += &format!("\n(jail blocked: {})", s.blocked_hosts.join(", "));
                }
                (s.exit_code, err)
            }
            Child::Plain(mut c) => {
                let mut err = String::new();
                if let Some(mut e) = c.stderr.take() {
                    let _ = tokio::io::AsyncReadExt::read_to_string(&mut e, &mut err).await;
                }
                let _ = c.start_kill();
                let code = c.wait().await.ok().and_then(|s| s.code()).unwrap_or(-1);
                (code, err)
            }
        }
    }
}

/// Does this error mean the subscription's usage limit was hit?
pub fn is_limit(e: &str) -> bool {
    let e = e.to_lowercase();
    ["usage limit", "rate limit", "limit reached", "limit is used up", "quota", "429"].iter().any(|w| e.contains(w))
}

fn reset_hint(resets_at: Option<i64>) -> String {
    match resets_at.and_then(|t| chrono::DateTime::from_timestamp(t, 0)) {
        Some(t) => format!(" (resets {})", t.with_timezone(&chrono::Local).format("%a %H:%M")),
        None => String::new(),
    }
}

impl<C: ModelClient> Agent<C> {
    /// One turn on a CLI brain. Streams text as it's written; tool calls arrive through
    /// [`Agent::brain_tool_call`] and run here, in the daemon.
    pub async fn run_cli_turn(&self, brain: &CliBrain, conv: &mut Conversation, source: &str, text: &str, events: Events<'_>) -> Result<TurnResult> {
        let (wf, body) = workflow::classify(text);
        // The CLI plans for itself: /plan is just a bigger general task.
        let wf = if wf == Workflow::Plan { Workflow::General } else { wf };
        let task_id = self.ledger.start_task(source, text)?;
        let goal: String = text.chars().take(500).collect();
        let root = self.span(
            task_id,
            None,
            SpanKind::Task,
            "task",
            json!({"wang.goal": goal, "wang.source": source, "wang.session": conv.id, "wang.workflow": wf.name(),
                   "gen_ai.provider.name": brain.kind.provider(), "gen_ai.request.model": brain.model.clone().unwrap_or_else(|| "default".into())}),
        );
        let prompt = self.decorate(&body, source, wf);
        let system = format!("{}\n\n{BRAIN_RULES}", self.system);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ServerMsg>();
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        self.cli_turns.lock().unwrap().insert(conv.id.clone(), CliTurn { task_id, root, events: tx, calls: calls.clone() });
        // Stored as "<cli>:<id>": after a /brain switch, the other CLI starts fresh.
        let tag = format!("{}:", brain.kind.bin());
        let resume = conv.cli_session.as_deref().and_then(|s| s.strip_prefix(&tag)).map(String::from);
        let result = self.drive_cli(brain, &conv.id, resume.as_deref(), &system, &prompt, events, &mut rx).await;
        self.cli_turns.lock().unwrap().remove(&conv.id);
        while let Ok(m) = rx.try_recv() {
            events(m);
        }
        let tool_calls = calls.load(std::sync::atomic::Ordering::Relaxed);
        match result {
            Ok(r) => {
                if let Some(s) = r.session {
                    conv.cli_session = Some(format!("{tag}{s}"));
                }
                // Keep a readable history too (for /trace and a switch back to an API brain).
                conv.push_user_text(&body);
                conv.messages.push(json!({"role": "assistant", "content": [{"type": "text", "text": r.reply}]}));
                self.ledger.finish_task(task_id, tool_calls, "done")?;
                self.end(
                    root,
                    Status::Ok,
                    json!({"wang.outcome": "done", "wang.steps": tool_calls, "wang.cost_usd": 0.0, "wang.brain.api_equiv_usd": r.api_equiv_usd,
                           "gen_ai.usage.input_tokens": r.usage.prompt_tokens(), "gen_ai.usage.output_tokens": r.usage.output_tokens}),
                );
                let mut note = format!("via your {}", brain.kind.plan());
                if let Some(l) = r.limit {
                    note += &format!(" · {l}");
                }
                Ok(TurnResult { reply: r.reply, usage: r.usage, cost_usd: 0.0, calls: tool_calls, task_id, outcome: "done".into(), workflow: wf, note: Some(note) })
            }
            Err(e) => {
                // A broken resume id shouldn't wedge the chat: start fresh next time.
                if resume.is_some() {
                    conv.cli_session = None;
                }
                let _ = self.ledger.finish_task(task_id, tool_calls, "error");
                self.end(root, Status::Error, json!({"wang.outcome": "error", "error.message": format!("{e:#}")}));
                Err(e)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn drive_cli(
        &self,
        brain: &CliBrain,
        session: &str,
        resume: Option<&str>,
        system: &str,
        prompt: &str,
        events: Events<'_>,
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<ServerMsg>,
    ) -> Result<CliOutcome> {
        let argv = brain.argv(session, resume, system, prompt);
        let (child, stdout): (Child, Box<dyn tokio::io::AsyncRead + Unpin + Send>) = match &brain.jail {
            Some(jail) => {
                // spawn_piped gives the command fds 3/4; the CLI wants ordinary stdin/stdout.
                let mut wrapped = vec!["/bin/sh".to_string(), "-c".into(), "exec \"$@\" </dev/null >&4".into(), "sh".into()];
                wrapped.extend(argv);
                let mut r = jail.spawn_piped(&brain.policy(), &wrapped).await.context("starting the brain CLI in the jail")?;
                drop(r.stdin.take());
                let out = r.stdout.take().context("brain stdout")?;
                (Child::Jailed(r), Box::new(out))
            }
            None => {
                let mut c = tokio::process::Command::new(&argv[0])
                    .args(&argv[1..])
                    .env("WANG_SOCKET", &brain.socket)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                    .with_context(|| format!("starting {}", brain.bin.display()))?;
                let out = c.stdout.take().context("brain stdout")?;
                (Child::Plain(c), Box::new(out))
            }
        };
        let mut lines = BufReader::new(stdout).lines();
        let deadline = tokio::time::sleep(brain.timeout);
        tokio::pin!(deadline);
        let mut out = CliOutcome::default();
        let mut text = String::new();
        let mut done: Option<Option<String>> = None;
        let mut error: Option<String> = None;
        let mut timed_out = false;
        loop {
            tokio::select! {
                line = lines.next_line() => {
                    let Ok(Some(line)) = line else { break };
                    for ev in brain.parse(&line) {
                        match ev {
                            Ev::Session(s) => out.session = Some(s),
                            Ev::NewMessage => {
                                if !text.is_empty() && !text.ends_with("\n\n") {
                                    let sep = if text.ends_with('\n') { "\n" } else { "\n\n" };
                                    text += sep;
                                    events(ServerMsg::Text { delta: sep.into() });
                                }
                            }
                            Ev::Text(t) => {
                                text += &t;
                                events(ServerMsg::Text { delta: t });
                            }
                            Ev::Limit { status, kind, utilization, resets_at } => {
                                let window = kind.replace('_', "-");
                                if status == "rejected" {
                                    error = Some(format!("Your {} usage limit is used up{}.", brain.kind.plan(), reset_hint(resets_at)));
                                } else if let Some(u) = utilization.filter(|u| *u >= 0.5) {
                                    out.limit = Some(format!("{window} limit {:.0}% used{}", u * 100.0, reset_hint(resets_at)));
                                }
                            }
                            Ev::Done { result, api_equiv_usd, usage } => {
                                out.api_equiv_usd = api_equiv_usd;
                                out.usage = usage;
                                done = Some(result);
                            }
                            // Keep the first (most specific) reason, e.g. the usage limit.
                            Ev::Error(e) => {
                                error.get_or_insert(e);
                            }
                        }
                    }
                }
                Some(m) = rx.recv() => events(m),
                _ = &mut deadline => { timed_out = true; break; }
            }
        }
        let (code, stderr) = child.finish().await;
        if timed_out {
            bail!("{} took longer than {} min and was stopped", brain.kind.bin(), brain.timeout.as_secs() / 60);
        }
        if let Some(e) = error {
            bail!("{e}");
        }
        let Some(result) = done else {
            let tail: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
            let tail = tail[tail.len().saturating_sub(6)..].join("\n");
            bail!("{} exited ({code}) without finishing the turn{}", brain.kind.bin(), if tail.is_empty() { String::new() } else { format!(":\n{tail}") });
        };
        // The streamed text is what the user saw; the CLI's final result is the fallback.
        out.reply = if text.trim().is_empty() { result.unwrap_or_default() } else { text.trim().to_string() };
        if out.reply.is_empty() {
            out.reply = "(done, no reply text)".into();
        }
        Ok(out)
    }

    /// A tool call from the brain's MCP proxy (`wang mcp --via-daemon`). Runs through the same
    /// approval gate, sandbox, ledger and trace as wang's own agent, inside the session's turn.
    pub async fn brain_tool_call(&self, session: &str, name: &str, input: Value) -> (String, bool, Option<String>) {
        let turn = self.cli_turns.lock().unwrap().get(session).map(|t| (t.task_id, t.root, t.events.clone(), t.calls.clone()));
        let (task_id, root, tx, calls) = match turn {
            Some(t) => t,
            None => {
                // Not inside a brain turn (e.g. someone ran the proxy by hand): its own task.
                let Ok(id) = self.ledger.start_task("mcp", &format!("{name} (via daemon)")) else { return ("ledger error".into(), true, None) };
                let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
                (id, None, tx, Arc::new(std::sync::atomic::AtomicU32::new(0)))
            }
        };
        if !tools::definitions_for(&self.ctx).iter().any(|d| d["name"] == name) {
            return (format!("unknown tool: {name}"), true, None);
        }
        calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = format!("brain-{}", calls.load(std::sync::atomic::Ordering::Relaxed));
        let tu = json!({"id": id, "name": name, "input": input});
        let emit = move |m: ServerMsg| {
            let _ = tx.send(m);
        };
        let o = self.exec_tool(session, None, Workflow::General, task_id, root, &tu, false, None, &emit).await;
        let (text, image) = match &o.block["content"] {
            Value::String(s) => (s.clone(), None),
            Value::Array(parts) => (
                parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
                parts.iter().find(|p| p["type"] == "image").and_then(|p| p["source"]["data"].as_str()).map(String::from),
            ),
            v => (v.to_string(), None),
        };
        (text, o.is_error, image)
    }

    /// Tool definitions offered to a brain (all of wang's; approvals happen here).
    pub fn brain_tools(&self) -> Vec<Value> {
        tools::definitions_for(&self.ctx)
    }
}

impl<C: ModelClient> Agent<C> {
    /// Telegram `/brain <name>`: switch now, and remember it in config.toml.
    pub fn switch_brain(&self, to: &str) -> Result<String> {
        let jail = match self.ctx.sandbox.as_ref() {
            crate::tools::Sandbox::Jailed { jail, .. } => Some(jail.clone()),
            _ => None,
        };
        let (provider, msg) = match Kind::from_provider(to).or(match to {
            "chatgpt" | "openai-sub" => Some(Kind::Codex),
            _ => None,
        }) {
            Some(kind) => {
                let b = CliBrain::new(kind, None, jail)?;
                *self.brain.write().unwrap() = Some(Arc::new(b));
                (kind.provider().to_string(), format!("Brain: your {} (via {}).", kind.plan(), kind.bin()))
            }
            None if matches!(to, "api" | "key") => {
                if !self.has_api_model() {
                    bail!("there's no API model set up (add a key and [model] fallback, or set provider in config.toml)");
                }
                *self.brain.write().unwrap() = None;
                (String::new(), format!("Brain: {} / {} (API key, pay per use).", self.client.provider(), self.client.model()))
            }
            None => bail!("unknown brain '{to}': try claude, codex or api"),
        };
        if !provider.is_empty()
            && let Err(e) = crate::config::set_provider(&crate::config::config_path(), &provider)
        {
            return Ok(format!("{msg} (Not saved to config.toml: {e:#}; it lasts until wang restarts.)"));
        }
        Ok(msg)
    }

    /// A short yes/no-style call with no tools (watchers deciding if something is worth a
    /// ping). Uses the API model if there is one, else the subscription brain.
    pub async fn judge(&self, session: &str, prompt: &str) -> Result<String> {
        if self.has_api_model() {
            let noop = |_: &str| {};
            let ctx = crate::model::CallCtx { session, on_text: &noop, step: 0 };
            let msgs = vec![json!({"role": "user", "content": [{"type": "text", "text": prompt}]})];
            let r = self.client.create("You triage notifications for a busy person. Be strict and brief.", &[], &msgs, &ctx).await?;
            return Ok(crate::agent::text_of(&r.content));
        }
        let Some(b) = self.current_brain() else { bail!("no brain to judge with") };
        let mut conv = Conversation::new(format!("{session}-judge"));
        let noop = |_: ServerMsg| {};
        let r = self.run_cli_turn(&b, &mut conv, "watch", &format!("/quick {prompt}\n\nDon't use any tools."), &noop).await?;
        Ok(r.reply)
    }

    /// What `/brain` shows.
    pub fn brain_status(&self) -> String {
        let now = match self.current_brain() {
            Some(b) => format!("your {} ({}{})", b.kind.plan(), b.kind.bin(), b.model.as_deref().map(|m| format!(", {m}")).unwrap_or_default()),
            None => format!("{} / {} (API key)", self.client.provider(), self.client.model()),
        };
        let mut opts = vec![];
        if claude_signed_in().is_some() {
            opts.push("/brain claude – your Claude subscription");
        }
        if codex_signed_in().is_some() {
            opts.push("/brain codex – your ChatGPT subscription");
        }
        if self.has_api_model() {
            opts.push("/brain api – the API key");
        }
        format!("Brain: {now}.\n{}", if opts.is_empty() { "No other brain is signed in on the laptop.".into() } else { opts.join("\n") })
    }
}

#[derive(Default)]
struct CliOutcome {
    reply: String,
    session: Option<String>,
    usage: Usage,
    api_equiv_usd: f64,
    limit: Option<String>,
}

/// Signed in? (for `wang setup` / `wang doctor`).
pub fn claude_signed_in() -> Option<String> {
    let o = std::process::Command::new(which("claude")?).args(["auth", "status"]).stderr(std::process::Stdio::null()).output().ok()?;
    let v: Value = serde_json::from_slice(&o.stdout).ok()?;
    (v["loggedIn"] == true).then(|| v["authMethod"].as_str().unwrap_or("signed in").to_string())
}

pub fn codex_signed_in() -> Option<String> {
    let o = std::process::Command::new(which("codex")?).args(["login", "status"]).output().ok()?;
    let s = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    (o.status.success() && !s.to_lowercase().contains("not logged in")).then(|| s.trim().lines().next().unwrap_or("signed in").to_string())
}

#[cfg(test)]
mod tests;

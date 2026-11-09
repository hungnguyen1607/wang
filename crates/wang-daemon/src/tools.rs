//! Tools: read_file, shell, web_fetch, browser (when enabled), remember/recall.
//!
//! - `shell` and `read_file` run inside `wang-jail` (namespaces, cgroups, seccomp, egress
//!   allow-list) with the policy from `[sandbox]` in the config.
//! - `web_fetch` runs no code, so it stays in-process but is held to a domain allow-list
//!   and refuses private / local addresses (no SSRF into localhost, the LAN or the tailnet).
//!
//! - `browser` drives a headless Chrome in its own jail (one per conversation, see
//!   `wang-browser`); pages come back as compact text with numbered elements.
//!
//! If the sandbox is enabled but unavailable, sandboxed tools fail closed.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use regex::Regex;
use serde_json::{Value, json};
use wang_jail::policy::{domain_allowed, is_hidden, is_private_ip, private_allowed};
use wang_jail::{Io, Jail, Policy};

pub enum Sandbox {
    Jailed { jail: Arc<Jail>, policy: Policy },
    /// Explicitly disabled in config.
    Off,
    /// Enabled but not working on this machine: sandboxed tools refuse to run.
    Unavailable(String),
}

#[derive(Debug, Clone)]
pub struct WebPolicy {
    pub domains: Vec<String>,
    pub allow_private: bool,
}

impl Default for WebPolicy {
    fn default() -> Self {
        Self { domains: vec!["*".into()], allow_private: false }
    }
}

#[derive(Clone)]
pub struct ToolCtx {
    pub workdir: PathBuf,
    pub http: reqwest::Client,
    pub sandbox: Arc<Sandbox>,
    pub web: Arc<WebPolicy>,
    pub memory: Option<Arc<crate::memory::Memory>>,
    pub browser: Option<Arc<wang_browser::Pool>>,
    pub vm: Option<Arc<wang_vm::Sessions>>,
    pub schedules: Option<Arc<crate::schedule::Store>>,
}

fn check_url_static(web: &WebPolicy, url: &reqwest::Url) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err("only http:// and https:// URLs are supported".into());
    }
    let host = url.host_str().ok_or("URL has no host")?;
    let port = url.port_or_known_default().unwrap_or(443);
    if !domain_allowed(&web.domains, host, port) {
        return Err(format!("{host}:{port} is not in web_fetch's allow-list"));
    }
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        if is_private_ip(ip) && !web.allow_private && !private_allowed(&web.domains, host) {
            return Err(format!("{host} is a private/local address"));
        }
    }
    Ok(())
}

impl ToolCtx {
    pub fn new(workdir: PathBuf) -> Self {
        Self::with(workdir, Sandbox::Off, WebPolicy::default())
    }

    pub fn with(workdir: PathBuf, sandbox: Sandbox, web: WebPolicy) -> Self {
        let web = Arc::new(web);
        let w = web.clone();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent("wang-agent/0.1")
            // Every redirect hop is held to the same policy.
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= 10 {
                    return attempt.error("too many redirects");
                }
                match check_url_static(&w, attempt.url()) {
                    Ok(()) => attempt.follow(),
                    Err(e) => attempt.error(format!("redirect blocked: {e}")),
                }
            }))
            .build()
            .expect("http client");
        Self { workdir, http, sandbox: Arc::new(sandbox), web, memory: None, browser: None, vm: None, schedules: None }
    }

    pub fn with_schedules(mut self, s: Option<Arc<crate::schedule::Store>>) -> Self {
        self.schedules = s;
        self
    }

    pub fn with_vm(mut self, vm: Option<Arc<wang_vm::Sessions>>) -> Self {
        self.vm = vm;
        self
    }

    pub fn with_browser(mut self, pool: Option<Arc<wang_browser::Pool>>) -> Self {
        self.browser = pool;
        self
    }

    pub fn with_memory(mut self, m: Option<Arc<crate::memory::Memory>>) -> Self {
        self.memory = m;
        self
    }

    /// Does this call need the user's approval? Privileged shell commands do when they
    /// actually gain something (write paths or network), and every shell command does
    /// when the sandbox is off.
    pub fn needs_approval(&self, name: &str, input: &Value) -> bool {
        if name != "shell" {
            return false;
        }
        match self.sandbox.as_ref() {
            Sandbox::Off => true,
            Sandbox::Unavailable(_) => false,
            Sandbox::Jailed { policy, .. } => {
                input.get("privileged").and_then(Value::as_bool) == Some(true)
                    && (!policy.write.is_empty() || !policy.domains.is_empty())
            }
        }
    }
}

/// What the sandbox did during one tool call (for traces).
#[derive(Debug, Clone, Default)]
pub struct SandboxInfo {
    pub setup_ms: Option<f64>,
    pub wall_ms: f64,
    pub peak_memory_bytes: Option<u64>,
    pub cpu_ms: Option<f64>,
    pub oom_killed: bool,
    pub pids_limit_hits: u64,
    pub timed_out: bool,
    pub blocked: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
    pub sandbox: Option<SandboxInfo>,
}

impl ToolOutput {
    fn ok(text: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: false, sandbox: None }
    }
    fn err(text: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: true, sandbox: None }
    }
}

/// The tools this context offers, in a fixed order so the cached prefix never changes.
pub fn definitions_for(ctx: &ToolCtx) -> Vec<Value> {
    let mut d = definitions();
    if ctx.browser.is_some() {
        d.push(browser_definition());
    }
    if ctx.vm.is_some() {
        d.push(vm_definition());
    }
    if ctx.schedules.is_some() {
        d.push(schedule_definition());
    }
    d
}

fn schedule_definition() -> Value {
    json!({
        "name": "schedule",
        "description": "Schedule a task for wang to do later or repeatedly on its own (a morning briefing, a reminder, a periodic check); the result is sent to the user where this conversation happens (Telegram chat, or their alerts). The task text runs later without this conversation, so make it self-contained. Also lists and removes this conversation's schedules.",
        "input_schema": {
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["add", "list", "remove"]},
                "when": {"type": "string", "description": "add: plain English, e.g. 'every weekday at 8am', 'every monday at 18:30', 'every 2 hours', 'in 20 minutes', 'tomorrow at 9am', 'at 2026-12-01 09:00'"},
                "task": {"type": "string", "description": "add: what to do, self-contained (e.g. 'Summarize the top 5 Hacker News posts with links')"},
                "id": {"type": "integer", "description": "remove: the schedule's number"}
            },
            "required": ["action"]
        }
    })
}

fn vm_definition() -> Value {
    json!({
        "name": "vm",
        "description": "Run shell commands in a throwaway Linux micro-VM (Firecracker, Debian with Python 3.12, root, no access to the user's files) instead of on the user's machine. Use it for big or risky jobs: installing packages, running untrusted or generated code, builds, experiments. Create files there with shell commands (heredocs). The VM persists across calls in this conversation and reaches only allowed domains. checkpoint saves its whole state (memory and files); rollback returns to the last checkpoint; try runs several alternative commands in parallel copies of the VM, scores each (optional score command: its last output line as a number, higher is better, or exit 0 = pass) and keeps the best copy; reset starts clean.",
        "input_schema": {
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["run", "checkpoint", "rollback", "try", "reset"]},
                "command": {"type": "string", "description": "run: the shell command (cwd /work)"},
                "commands": {"type": "array", "items": {"type": "string"}, "description": "try: alternative commands, one per VM copy"},
                "score": {"type": "string", "description": "try: command run after each alternative in its copy to score it"},
                "timeout_secs": {"type": "integer", "description": "Per command. Default 600."}
            },
            "required": ["action"]
        }
    })
}

fn browser_definition() -> Value {
    json!({
        "name": "browser",
        "description": "Use a real web browser (headless Chrome in a sandbox) for pages that need JavaScript, clicking, logging-free forms or several steps. Prefer web_fetch for a simple page you only need to read. Pages are shown as compact text: headings start with #, and every link, button and field appears as [n ...]; actions refer to those numbers, which change when the page changes. Every action returns the updated page. The browser starts empty (no cookies or logins) and can only reach allowed domains.",
        "input_schema": {
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["open", "view", "click", "type", "select", "back"],
                           "description": "open a URL; view the current page (or another part of a long page); click [n]; type text into field [n]; select an option in dropdown [n]; go back"},
                "url": {"type": "string", "description": "open: http(s) URL"},
                "ref": {"type": "integer", "description": "click/type/select: the element's number from the page view"},
                "text": {"type": "string", "description": "type: the text (replaces what's in the field); select: the option's label"},
                "submit": {"type": "boolean", "description": "type: press Enter afterwards (submits most forms)"},
                "part": {"type": "integer", "description": "view: part of a long page to show (default 1)"}
            },
            "required": ["action"]
        }
    })
}

/// Tool definitions, in a fixed order so the cached prefix never changes.
pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "read_file",
            "description": "Read a UTF-8 text file from the local machine. Relative paths resolve against the working directory; ~ expands to the home directory. Use offset/limit (1-based line numbers) to page through large files instead of reading them whole. Runs in a sandbox: some paths (keys, credentials, browser profiles) are hidden.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path"},
                    "offset": {"type": "integer", "description": "First line to return (1-based). Default 1."},
                    "limit": {"type": "integer", "description": "Max lines to return. Default: whole file."}
                },
                "required": ["path"]
            }
        }),
        json!({
            "name": "shell",
            "description": "Run a bash command on the local machine in the working directory and return exit code, stdout and stderr. No stdin; interactive programs will not work. Prefer targeted commands (grep, head, tail, wc, find with filters) over commands that print huge outputs. Long outputs are truncated to their beginning and end. Runs in a sandbox: only some paths are visible or writable, network access is limited to an allow-list, and memory, CPU and process count are capped; the output says when the sandbox blocked something.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The bash command to run"},
                    "timeout_secs": {"type": "integer", "description": "Kill the command after this many seconds. Default 60, max 300."},
                    "privileged": {"type": "boolean", "description": "Default false: the command runs read-only with no network. Set true only when it must change files in the user's writable folders or use the network allow-list; the user is asked to approve first."}
                },
                "required": ["command"]
            }
        }),
        json!({
            "name": "remember",
            "description": "Save a lasting fact about the user or their setup to long-term memory (preferences, names, recurring details). One short fact per call. Don't save secrets or one-off details.",
            "input_schema": {
                "type": "object",
                "properties": {"fact": {"type": "string", "description": "The fact, as a short standalone sentence"}},
                "required": ["fact"]
            }
        }),
        json!({
            "name": "recall",
            "description": "Search long-term memory for facts saved earlier. Relevant facts are often attached to the user's message already; use this to look for something specific.",
            "input_schema": {
                "type": "object",
                "properties": {"query": {"type": "string", "description": "Keywords to search for"}},
                "required": ["query"]
            }
        }),
        json!({
            "name": "web_fetch",
            "description": "Fetch a URL over HTTP(S) with GET and return its text. HTML is converted to plain text; JSON and other text types are returned as-is. Binary content is rejected.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "http:// or https:// URL"}
                },
                "required": ["url"]
            }
        }),
    ]
}

pub async fn run(name: &str, input: &Value, ctx: &ToolCtx) -> ToolOutput {
    run_in(name, input, ctx, "default").await
}

/// `session`: the conversation, so stateful tools (the browser) keep one instance per conversation.
pub async fn run_in(name: &str, input: &Value, ctx: &ToolCtx, session: &str) -> ToolOutput {
    match name {
        "browser" => browser(input, ctx, session).await,
        "vm" => vm_tool(input, ctx, session).await,
        "schedule" => schedule_tool(input, ctx, session),
        "read_file" => read_file(input, ctx).await,
        "shell" => shell(input, ctx).await,
        "web_fetch" => web_fetch(input, ctx).await,
        "remember" | "recall" => memory_tool(name, input, ctx),
        other => ToolOutput::err(format!("unknown tool: {other}")),
    }
}

/// One-line summary of a tool call for traces and the chat stream.
pub fn detail(name: &str, input: &Value) -> String {
    let key = match name {
        "shell" => "command",
        "read_file" => "path",
        "web_fetch" => "url",
        "remember" => "fact",
        "recall" => "query",
        "schedule" => {
            let action = input.get("action").and_then(Value::as_str).unwrap_or("?");
            let when = input.get("when").and_then(Value::as_str).unwrap_or("");
            let task = input.get("task").and_then(Value::as_str).unwrap_or("");
            return format!("{action} {when} {task}").split_whitespace().collect::<Vec<_>>().join(" ").chars().take(160).collect();
        }
        "vm" => {
            let action = input.get("action").and_then(Value::as_str).unwrap_or("?");
            let arg = input
                .get("command")
                .and_then(Value::as_str)
                .map(String::from)
                .or_else(|| input.get("commands").and_then(Value::as_array).map(|c| format!("{} alternatives", c.len())))
                .unwrap_or_default();
            let s = format!("{action} {arg}").trim().replace('\n', " ⏎ ");
            return s.chars().take(160).collect();
        }
        "browser" => {
            let action = input.get("action").and_then(Value::as_str).unwrap_or("?");
            let arg = input
                .get("url")
                .and_then(Value::as_str)
                .map(String::from)
                .or_else(|| input.get("ref").and_then(Value::as_u64).map(|r| format!("[{r}]")))
                .unwrap_or_default();
            let text = input.get("text").and_then(Value::as_str).map(|t| format!(" \"{t}\"")).unwrap_or_default();
            return format!("{action} {arg}{text}").trim().chars().take(160).collect();
        }
        _ => "",
    };
    let s = input.get(key).and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| input.to_string());
    let s = s.replace('\n', " ⏎ ");
    if s.chars().count() > 160 { format!("{}…", s.chars().take(160).collect::<String>()) } else { s }
}

fn schedule_tool(input: &Value, ctx: &ToolCtx, session: &str) -> ToolOutput {
    let Some(store) = &ctx.schedules else { return ToolOutput::err("scheduling isn't available here") };
    let action = match str_arg(input, "action") {
        Ok(a) => a,
        Err(e) => return e,
    };
    let r: anyhow::Result<String> = (|| match action {
        "add" => {
            let when = input.get("when").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("add needs 'when'"))?;
            let task = input.get("task").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("add needs 'task'"))?;
            let s = store.add(when, task, session, chrono::Local::now())?;
            Ok(format!(
                "Scheduled #{}: {} (next {}). Results go to {}.",
                s.id,
                s.rule.describe(),
                s.next_run.map(|t| t.format("%a %b %-d %H:%M").to_string()).unwrap_or_default(),
                if session.starts_with("tg-") { "this Telegram chat" } else { "the user's alerts" }
            ))
        }
        "list" => Ok(crate::schedule::render(&store.list(Some(session))?)),
        "remove" => {
            let id = input.get("id").and_then(Value::as_i64).ok_or_else(|| anyhow::anyhow!("remove needs 'id'"))?;
            Ok(if store.remove(id, Some(session))? { format!("Removed schedule #{id}.") } else { format!("No schedule #{id} in this conversation.") })
        }
        other => Err(anyhow::anyhow!("unknown action '{other}' (add, list, remove)")),
    })();
    match r {
        Ok(t) => ToolOutput::ok(t),
        Err(e) => ToolOutput::err(format!("{e:#}")),
    }
}

async fn vm_tool(input: &Value, ctx: &ToolCtx, session: &str) -> ToolOutput {
    let Some(vms) = &ctx.vm else { return ToolOutput::err("VMs are not available (run `wang vm setup`, and [vm] enabled)") };
    let action = match str_arg(input, "action") {
        Ok(a) => a,
        Err(e) => return e,
    };
    let timeout = Duration::from_secs(int_arg(input, "timeout_secs").unwrap_or(600).clamp(1, 3600));
    let fmt = |r: &wang_vm::Exec| {
        let mut notes = vec![];
        if r.timed_out {
            notes.push(format!("killed after the {}s time limit", timeout.as_secs()));
        }
        if r.truncated {
            notes.push("output truncated at 1 MiB".to_string());
        }
        format_exec(&r.exit.to_string(), r.stdout.as_bytes(), r.stderr.as_bytes(), &notes)
    };
    match action {
        "run" => {
            let cmd = match str_arg(input, "command") {
                Ok(c) => c,
                Err(e) => return e,
            };
            match vms.run(session, cmd, timeout).await {
                Ok(r) => ToolOutput { is_error: !r.ok(), ..ToolOutput::ok(fmt(&r)) },
                Err(e) => ToolOutput::err(format!("vm: {e:#}")),
            }
        }
        "checkpoint" => match vms.checkpoint(session).await {
            Ok(ms) => ToolOutput::ok(format!("checkpoint saved ({ms:.0} ms); rollback returns here")),
            Err(e) => ToolOutput::err(format!("vm: {e:#}")),
        },
        "rollback" => match vms.rollback(session).await {
            Ok(true) => ToolOutput::ok("rolled back to the last checkpoint"),
            Ok(false) => ToolOutput::ok("no checkpoint yet: started a clean VM"),
            Err(e) => ToolOutput::err(format!("vm: {e:#}")),
        },
        "reset" => {
            vms.reset(session).await;
            ToolOutput::ok("VM reset; the next command starts a clean VM")
        }
        "try" => {
            let commands: Vec<String> = input
                .get("commands")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|c| c.as_str().map(String::from)).collect())
                .unwrap_or_default();
            if commands.len() < 2 {
                return ToolOutput::err("try needs 'commands' with at least two alternatives");
            }
            let score = input.get("score").and_then(Value::as_str);
            match vms.try_parallel(session, &commands, score, timeout).await {
                Err(e) => ToolOutput::err(format!("vm: {e:#}")),
                Ok(t) => {
                    let mut out = format!("tried {} alternatives in parallel VM copies ({:.0} ms):\n", commands.len(), t.total_ms);
                    for (i, a) in t.attempts.iter().enumerate() {
                        let mark = if Some(i) == t.best { " ← kept" } else { "" };
                        out += &format!("\n## [{}] {}{mark}\n", i + 1, a.command);
                        match &a.run {
                            Ok(r) => {
                                if score.is_some() || !r.ok() {
                                    out += &format!("score: {}\n", a.score);
                                }
                                out += &cap_middle_str(&fmt(r), 2_000);
                            }
                            Err(e) => out += &format!("failed: {e}\n"),
                        }
                        if let Some(o) = &a.score_output {
                            out += &format!("score output: {}\n", cap_middle_str(o, 300));
                        }
                    }
                    out += &match t.best {
                        Some(b) => format!("\nThe VM now continues from alternative [{}].", b + 1),
                        None => "\nNo alternative finished; the VM is unchanged.".to_string(),
                    };
                    ToolOutput::ok(out)
                }
            }
        }
        other => ToolOutput::err(format!("unknown action '{other}' (run, checkpoint, rollback, try, reset)")),
    }
}

fn cap_middle_str(s: &str, n: usize) -> String {
    wang_tokens::cap_middle(s, n).text
}

async fn browser(input: &Value, ctx: &ToolCtx, session: &str) -> ToolOutput {
    let Some(pool) = &ctx.browser else { return ToolOutput::err("the browser is not enabled ([browser] in config)") };
    let action = match str_arg(input, "action") {
        Ok(a) => a,
        Err(e) => return e,
    };
    let need_ref = || int_arg(input, "ref").map(|r| r as usize).ok_or_else(|| ToolOutput::err(format!("{action} needs 'ref' (an element number from the page view)")));
    let lease = match pool.get(session).await {
        Ok(l) => l,
        Err(e) => return ToolOutput::err(format!("browser unavailable: {e:#}")),
    };
    let mut b = lease.lock().await;
    let blocked_before = b.blocked_hosts();
    let mut note = String::new();
    let mut part = 1;
    let done: anyhow::Result<()> = async {
        match action {
            "open" => b.open(str_arg(input, "url").map_err(|e| anyhow::anyhow!(e.text))?).await,
            "view" => {
                part = int_arg(input, "part").unwrap_or(1).max(1) as usize;
                Ok(())
            }
            "click" => b.click(need_ref().map_err(|e| anyhow::anyhow!(e.text))?).await,
            "type" => {
                let r = need_ref().map_err(|e| anyhow::anyhow!(e.text))?;
                let text = str_arg(input, "text").map_err(|e| anyhow::anyhow!(e.text))?;
                b.type_text(r, text, input.get("submit").and_then(Value::as_bool).unwrap_or(false)).await
            }
            "select" => {
                let r = need_ref().map_err(|e| anyhow::anyhow!(e.text))?;
                let chosen = b.select(r, str_arg(input, "text").map_err(|e| anyhow::anyhow!(e.text))?).await?;
                note = format!("selected \"{chosen}\"\n");
                Ok(())
            }
            "back" => b.back().await,
            other => Err(anyhow::anyhow!("unknown action '{other}' (open, view, click, type, select, back)")),
        }
    }
    .await;
    if let Err(e) = done {
        return ToolOutput::err(format!("{e:#}"));
    }
    let view = match b.view(200_000).await {
        Ok(v) => v,
        Err(e) => return ToolOutput::err(format!("{e:#}")),
    };
    let size = pool.cfg.view_chars;
    let (text, parts) = wang_browser::view_part(&view.text, part, size);
    let mut out = format!("{note}{text}");
    if parts > 1 {
        out.push_str(&format!("\n\n[part {} of {parts}; action=view part={} for more]", part.min(parts), (part + 1).min(parts)));
    }
    let newly: Vec<String> = b.blocked_hosts().into_iter().filter(|h| !blocked_before.contains(h)).collect();
    if !newly.is_empty() {
        out.push_str(&format!("\n[sandbox blocked: {}]", newly.join(", ")));
    }
    ToolOutput::ok(out)
}

fn str_arg<'a>(input: &'a Value, key: &str) -> Result<&'a str, ToolOutput> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolOutput::err(format!("missing or non-string argument '{key}'")))
}

fn int_arg(input: &Value, key: &str) -> Option<u64> {
    input.get(key).and_then(Value::as_u64)
}

fn resolve(ctx: &ToolCtx, p: &str) -> PathBuf {
    let p = crate::config::expand_tilde(p);
    let p = if p.is_absolute() { p } else { ctx.workdir.join(p) };
    // Normalize `..` without touching the filesystem, so policy checks see the real target.
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

fn unavailable(reason: &str) -> ToolOutput {
    ToolOutput::err(format!(
        "sandbox unavailable, so this tool is disabled: {reason}\nFix the sandbox (run `jail check`), or set [sandbox] enabled = false in the wang config to run tools unsandboxed (not recommended)."
    ))
}

fn sandbox_info(r: &wang_jail::JailResult) -> SandboxInfo {
    SandboxInfo {
        setup_ms: r.setup_ms,
        wall_ms: r.wall_ms,
        peak_memory_bytes: r.usage.peak_memory_bytes,
        cpu_ms: r.usage.cpu_usec.map(|u| u as f64 / 1e3),
        oom_killed: r.oom_killed(),
        pids_limit_hits: r.usage.pids_limit_hits,
        timed_out: r.timed_out,
        blocked: r.blocked_hosts.clone(),
    }
}

/// Is `path` inside what the jail shows (declared paths + the read-only system dirs)?
fn visible_in(policy: &Policy, path: &Path) -> bool {
    ["/usr", "/etc", "/bin", "/lib", "/lib64", "/sbin"].iter().any(|s| path.starts_with(s))
        || policy.read.iter().chain(&policy.write).chain(&policy.overlay).any(|v| path.starts_with(v))
}

async fn read_file(input: &Value, ctx: &ToolCtx) -> ToolOutput {
    let path = match str_arg(input, "path") {
        Ok(p) => resolve(ctx, p),
        Err(e) => return e,
    };
    let offset = int_arg(input, "offset").unwrap_or(1).max(1);
    let limit = int_arg(input, "limit").unwrap_or(0);
    match ctx.sandbox.as_ref() {
        Sandbox::Unavailable(r) => unavailable(r),
        Sandbox::Off => read_file_direct(&path, offset as usize, (limit > 0).then_some(limit as usize)).await,
        Sandbox::Jailed { jail, policy } => {
            if is_hidden(&path, &policy.hide) {
                return ToolOutput::err(format!("{} is hidden by the sandbox (secrets and credentials are never readable)", path.display()));
            }
            if !visible_in(policy, &path) {
                return ToolOutput::err(format!(
                    "{} is outside the sandbox's visible paths; add it to [sandbox] read in the wang config to allow it",
                    path.display()
                ));
            }
            let argv = vec![
                wang_jail::helper::HELPER_PATH.to_string(),
                wang_jail::helper::READ.to_string(),
                path.to_string_lossy().into_owned(),
                offset.to_string(),
                limit.to_string(),
            ];
            let mut p = policy.clone();
            p.domains.clear();
            p.limits.timeout = Duration::from_secs(20);
            match jail.run(&p, &argv, Io::Capture).await {
                Ok(r) => {
                    let info = sandbox_info(&r);
                    let (text, is_error) = if r.exit_code == 0 {
                        (String::from_utf8_lossy(&r.stdout).into_owned(), false)
                    } else {
                        (String::from_utf8_lossy(&r.stderr).trim().to_string(), true)
                    };
                    ToolOutput { text, is_error, sandbox: Some(info) }
                }
                Err(e) => ToolOutput::err(format!("sandbox error: {e:#}")),
            }
        }
    }
}

const MAX_READ_BYTES: u64 = 5 * 1024 * 1024;

async fn read_file_direct(path: &Path, offset: usize, limit: Option<usize>) -> ToolOutput {
    match tokio::fs::metadata(path).await {
        Ok(m) if m.is_dir() => return ToolOutput::err(format!("{} is a directory; use shell with ls", path.display())),
        Ok(m) if m.len() > MAX_READ_BYTES => {
            return ToolOutput::err(format!("{} is {} bytes (limit {MAX_READ_BYTES}); use shell with head/tail/grep", path.display(), m.len()));
        }
        Ok(_) => {}
        Err(e) => return ToolOutput::err(format!("{}: {e}", path.display())),
    }
    let bytes = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(e) => return ToolOutput::err(format!("{}: {e}", path.display())),
    };
    if bytes.contains(&0) {
        return ToolOutput::err(format!("{} looks like a binary file", path.display()));
    }
    let text = String::from_utf8_lossy(&bytes);
    if offset == 1 && limit.is_none() {
        return ToolOutput::ok(text.into_owned());
    }
    let total = text.lines().count();
    let selected: Vec<&str> = text.lines().skip(offset - 1).take(limit.unwrap_or(usize::MAX)).collect();
    let end = offset - 1 + selected.len();
    ToolOutput::ok(format!("[lines {offset}-{end} of {total}]\n{}", selected.join("\n")))
}

fn format_exec(code: &str, stdout: &[u8], stderr: &[u8], notes: &[String]) -> String {
    let mut s = format!("exit code: {code}\n");
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    if !stdout.is_empty() {
        s += &format!("--- stdout ---\n{stdout}");
        if !stdout.ends_with('\n') {
            s.push('\n');
        }
    }
    if !stderr.is_empty() {
        s += &format!("--- stderr ---\n{stderr}");
        if !stderr.ends_with('\n') {
            s.push('\n');
        }
    }
    if !notes.is_empty() {
        s += "--- sandbox ---\n";
        for n in notes {
            s += &format!("- {n}\n");
        }
    }
    s
}

async fn shell(input: &Value, ctx: &ToolCtx) -> ToolOutput {
    let command = match str_arg(input, "command") {
        Ok(c) => c,
        Err(e) => return e,
    };
    let secs = int_arg(input, "timeout_secs").unwrap_or(60).clamp(1, 300);
    match ctx.sandbox.as_ref() {
        Sandbox::Unavailable(r) => unavailable(r),
        Sandbox::Off => run_shell(command, &ctx.workdir, Duration::from_secs(secs)).await,
        Sandbox::Jailed { jail, policy } => {
            let mut p = policy.clone();
            if input.get("privileged").and_then(Value::as_bool) != Some(true) {
                // Least privilege: write paths become read-only, no network.
                let w = std::mem::take(&mut p.write);
                p.read.extend(w);
                p.domains.clear();
            }
            p.cwd = Some(ctx.workdir.clone());
            p.limits.timeout = Duration::from_secs(secs);
            let argv = vec!["bash".to_string(), "-c".to_string(), command.to_string()];
            match jail.run(&p, &argv, Io::Capture).await {
                Ok(r) => {
                    let notes = r.notes(&p);
                    let text = format_exec(&r.exit_code.to_string(), &r.stdout, &r.stderr, &notes);
                    ToolOutput { text, is_error: r.exit_code != 0 || r.timed_out, sandbox: Some(sandbox_info(&r)) }
                }
                Err(e) => ToolOutput::err(format!("sandbox error: {e:#}")),
            }
        }
    }
}

/// Unsandboxed shell (only with `[sandbox] enabled = false`).
pub async fn run_shell(command: &str, workdir: &Path, timeout: Duration) -> ToolOutput {
    let child = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(workdir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn();
    let child = match child {
        Ok(c) => c,
        Err(e) => return ToolOutput::err(format!("failed to start bash: {e}")),
    };
    let pid = child.id();
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => {
            let code = out.status.code().map_or("killed by signal".to_string(), |c| c.to_string());
            ToolOutput { text: format_exec(&code, &out.stdout, &out.stderr, &[]), is_error: !out.status.success(), sandbox: None }
        }
        Ok(Err(e)) => ToolOutput::err(format!("command failed: {e}")),
        Err(_) => {
            if let Some(pid) = pid {
                let _ = std::process::Command::new("kill").arg("-KILL").arg(format!("-{pid}")).status();
            }
            ToolOutput::err(format!("command timed out after {}s and was killed", timeout.as_secs()))
        }
    }
}

fn memory_tool(name: &str, input: &Value, ctx: &ToolCtx) -> ToolOutput {
    let Some(mem) = &ctx.memory else { return ToolOutput::err("memory is disabled") };
    if name == "remember" {
        let fact = match str_arg(input, "fact") {
            Ok(f) => f,
            Err(e) => return e,
        };
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        return match mem.add(fact, &date) {
            Ok(m) => ToolOutput::ok(m),
            Err(e) => ToolOutput::err(format!("{e:#}")),
        };
    }
    let q = match str_arg(input, "query") {
        Ok(q) => q,
        Err(e) => return e,
    };
    let hits = mem.search(q, 8);
    if hits.is_empty() {
        return ToolOutput::ok("no matching facts in memory");
    }
    ToolOutput::ok(hits.iter().map(|(_, f)| format!("- {f}")).collect::<Vec<_>>().join("\n"))
}

const MAX_FETCH_BYTES: usize = 2 * 1024 * 1024;

/// Allow-list + DNS check before fetching (redirects are re-checked by the client's policy).
async fn check_url(web: &WebPolicy, url: &reqwest::Url) -> Result<(), String> {
    check_url_static(web, url)?;
    let host = url.host_str().unwrap_or_default();
    let port = url.port_or_known_default().unwrap_or(443);
    if web.allow_private || private_allowed(&web.domains, host) {
        return Ok(());
    }
    let addrs = tokio::net::lookup_host((host.trim_start_matches('[').trim_end_matches(']'), port))
        .await
        .map_err(|e| format!("can't resolve {host}: {e}"))?;
    for a in addrs {
        if is_private_ip(a.ip()) {
            return Err(format!("{host} resolves to a private/local address ({})", a.ip()));
        }
    }
    Ok(())
}

async fn web_fetch(input: &Value, ctx: &ToolCtx) -> ToolOutput {
    let url = match str_arg(input, "url") {
        Ok(u) => u,
        Err(e) => return e,
    };
    let parsed = match reqwest::Url::parse(url) {
        Ok(u) => u,
        Err(e) => return ToolOutput::err(format!("bad URL: {e}")),
    };
    if let Err(e) = check_url(&ctx.web, &parsed).await {
        return ToolOutput::err(format!("blocked: {e}"));
    }
    let mut resp = match ctx.http.get(parsed).send().await {
        Ok(r) => r,
        Err(e) => return ToolOutput::err(format!("fetch failed: {e}")),
    };
    let status = resp.status();
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_text = ctype.is_empty()
        || ctype.starts_with("text/")
        || ctype.contains("json")
        || ctype.contains("xml")
        || ctype.contains("javascript");
    if !is_text {
        return ToolOutput::err(format!("HTTP {status}, non-text content-type '{ctype}'"));
    }
    let mut body = Vec::new();
    let mut truncated = false;
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                body.extend_from_slice(&chunk);
                if body.len() > MAX_FETCH_BYTES {
                    body.truncate(MAX_FETCH_BYTES);
                    truncated = true;
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => return ToolOutput::err(format!("reading body failed: {e}")),
        }
    }
    let raw = String::from_utf8_lossy(&body);
    let text = if ctype.contains("html") || (ctype.is_empty() && raw.trim_start().starts_with('<')) {
        html_to_text(&raw)
    } else {
        raw.into_owned()
    };
    let mut out = format!("HTTP {status} {url}\n\n{text}");
    if truncated {
        out += "\n[body truncated at 2 MB]";
    }
    ToolOutput { text: out, is_error: !status.is_success(), sandbox: None }
}

static RE_DROP: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    ["script", "style", "noscript", "svg", "head", "template"]
        .iter()
        .map(|t| Regex::new(&format!(r"(?is)<{t}\b[^>]*>.*?</{t}\s*>")).unwrap())
        .chain([Regex::new(r"(?s)<!--.*?-->").unwrap()])
        .collect()
});
static RE_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)<(br|/p|/div|/li|/h[1-6]|/tr|/section|/article|/header|/footer|/pre|/blockquote)\b[^>]*>").unwrap()
});
static RE_LI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<li\b[^>]*>").unwrap());
static RE_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]+>").unwrap());
static RE_SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t\u{a0}]+").unwrap());

/// Cheap HTML → text: drop scripts/styles, keep block structure, decode common entities.
pub fn html_to_text(html: &str) -> String {
    let mut s = html.to_string();
    for re in RE_DROP.iter() {
        s = re.replace_all(&s, " ").into_owned();
    }
    s = RE_BLOCK.replace_all(&s, "\n").into_owned();
    s = RE_LI.replace_all(&s, "\n- ").into_owned();
    s = RE_TAG.replace_all(&s, "").into_owned();
    for (ent, ch) in [("&nbsp;", " "), ("&lt;", "<"), ("&gt;", ">"), ("&quot;", "\""), ("&#39;", "'"), ("&#x27;", "'"), ("&amp;", "&")] {
        s = s.replace(ent, ch);
    }
    let mut out = String::with_capacity(s.len() / 2);
    let mut blank = 0;
    for line in s.lines() {
        let line = RE_SPACES.replace_all(line.trim(), " ");
        if line.is_empty() {
            blank += 1;
            if blank == 1 && !out.is_empty() {
                out.push('\n');
            }
        } else {
            blank = 0;
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_strip() {
        let h = "<html><head><title>x</title><style>a{}</style></head><body><h1>Hi</h1><script>evil()</script><p>a &amp; b</p><ul><li>one</li><li>two</li></ul></body></html>";
        let t = html_to_text(h);
        assert!(t.contains("Hi"));
        assert!(t.contains("a & b"));
        assert!(t.contains("- one"));
        assert!(!t.contains("evil"));
        assert!(!t.contains("a{}"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shell_runs_and_times_out_unsandboxed() {
        let dir = std::env::temp_dir();
        let ok = run_shell("echo hi; echo err >&2; exit 3", &dir, Duration::from_secs(5)).await;
        assert!(ok.is_error);
        assert!(ok.text.contains("exit code: 3"));
        assert!(ok.text.contains("hi"));
        assert!(ok.text.contains("err"));
        let slow = run_shell("sleep 30", &dir, Duration::from_secs(1)).await;
        assert!(slow.is_error);
        assert!(slow.text.contains("timed out"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_file_paging_unsandboxed() {
        let dir = std::env::temp_dir().join(format!("wang-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), "a\nb\nc\nd\n").unwrap();
        let ctx = ToolCtx::new(dir.clone());
        let out = run("read_file", &json!({"path": "f.txt", "offset": 2, "limit": 2}), &ctx).await;
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(out.text, "[lines 2-3 of 4]\nb\nc");
        assert!(run("read_file", &json!({}), &ctx).await.is_error);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unavailable_sandbox_fails_closed() {
        let ctx = ToolCtx::with(std::env::temp_dir(), Sandbox::Unavailable("bwrap missing".into()), WebPolicy::default());
        let out = run("shell", &json!({"command": "echo should-not-run"}), &ctx).await;
        assert!(out.is_error);
        assert!(out.text.contains("sandbox unavailable"));
        assert!(!out.text.contains("should-not-run"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn web_policy_blocks_local_and_unlisted() {
        let ctx = ToolCtx::with(std::env::temp_dir(), Sandbox::Off, WebPolicy::default());
        for url in ["http://127.0.0.1:8080/", "http://localhost/", "http://[::1]/", "http://192.168.1.1/", "http://100.100.100.100/", "file:///etc/passwd"] {
            let out = run("web_fetch", &json!({"url": url}), &ctx).await;
            assert!(out.is_error && (out.text.contains("blocked") || out.text.contains("supported")), "{url}: {}", out.text);
        }
        let ctx = ToolCtx::with(std::env::temp_dir(), Sandbox::Off, WebPolicy { domains: vec!["example.com".into()], allow_private: false });
        let out = run("web_fetch", &json!({"url": "https://evil.example.org/"}), &ctx).await;
        assert!(out.text.contains("not in web_fetch's allow-list"), "{}", out.text);
    }

    #[test]
    fn resolve_normalizes_dotdot() {
        let ctx = ToolCtx::new(PathBuf::from("/home/u/work"));
        assert_eq!(resolve(&ctx, "../.ssh/id_rsa"), PathBuf::from("/home/u/.ssh/id_rsa"));
    }

    /// Real sandbox, when this machine supports it (skipped otherwise).
    #[tokio::test(flavor = "current_thread")]
    async fn sandboxed_tools() {
        // The test harness can't act as the in-jail helper; use the built `jail` binary.
        let helper = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("jail");
        let jail = match Jail::with_helper(helper.clone()) {
            Ok(j) if helper.exists() && j.check().await.is_ok() => Arc::new(j),
            _ => {
                eprintln!("skipping: sandbox or {} not available", helper.display());
                return;
            }
        };
        let dir = std::env::temp_dir().join(format!("wang-sbx-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("secret")).unwrap();
        std::fs::write(dir.join("notes.txt"), "line1\nline2\n").unwrap();
        std::fs::write(dir.join("secret/key"), "SECRET").unwrap();
        let policy = Policy { read: vec![dir.clone()], hide: vec![dir.join("secret")], ..Default::default() };
        let ctx = ToolCtx::with(dir.clone(), Sandbox::Jailed { jail, policy }, WebPolicy::default());

        let r = run("read_file", &json!({"path": "notes.txt", "offset": 2, "limit": 1}), &ctx).await;
        assert_eq!(r.text, "[lines 2-2 of 2]\nline2", "{r:?}");
        assert!(r.sandbox.is_some());
        let r = run("read_file", &json!({"path": "secret/key"}), &ctx).await;
        assert!(r.is_error && r.text.contains("hidden"), "{r:?}");
        let r = run("read_file", &json!({"path": "/var/log/syslog"}), &ctx).await;
        assert!(r.is_error && r.text.contains("outside the sandbox"), "{r:?}");

        let r = run("shell", &json!({"command": "cat notes.txt; cat secret/key; echo x > notes.txt"}), &ctx).await;
        assert!(r.text.contains("line1"), "{}", r.text);
        assert!(!r.text.contains("SECRET"), "{}", r.text);
        assert!(r.text.contains("Read-only file system"), "{}", r.text);
        assert!(r.sandbox.as_ref().unwrap().setup_ms.is_some());
        let r = run("shell", &json!({"command": "sleep 5", "timeout_secs": 1}), &ctx).await;
        assert!(r.is_error && r.text.contains("time limit"), "{}", r.text);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn memory_tools_and_approval_rules() {
        let p = std::env::temp_dir().join(format!("wang-memtool-{}.md", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let ctx = ToolCtx::new(std::env::temp_dir()).with_memory(Some(Arc::new(crate::memory::Memory::new(p.clone()))));
        assert!(run("remember", &json!({"fact": "Hung's bike is a blue Brompton"}), &ctx).await.text.starts_with("remembered"));
        assert!(run("recall", &json!({"query": "bike"}), &ctx).await.text.contains("Brompton"));
        std::fs::remove_file(p).ok();
        // sandbox off: every shell call needs approval
        assert!(ctx.needs_approval("shell", &json!({"command": "ls"})));
        assert!(!ctx.needs_approval("read_file", &json!({"path": "x"})));
    }

    #[test]
    fn definitions_have_schemas() {
        for d in definitions() {
            assert!(d["name"].is_string());
            assert_eq!(d["input_schema"]["type"], "object");
        }
    }

    /// The browser tool end to end: real Chrome in the real jail, driven through the tool API
    /// the agent uses (skipped without a sandbox, the `jail` helper or a Chrome).
    #[tokio::test(flavor = "current_thread")]
    async fn browser_tool_in_the_jail() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let helper = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("jail");
        let jail = match Jail::with_helper(helper.clone()) {
            Ok(j) if helper.exists() && wang_browser::find_chrome().is_ok() && j.check().await.is_ok() => Arc::new(j),
            _ => {
                eprintln!("skipping: sandbox, {} or Chrome not available", helper.display());
                return;
            }
        };
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = l.accept().await else { return };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let path = String::from_utf8_lossy(&buf[..n]).split(' ').nth(1).unwrap_or("/").to_string();
                    let body = if path.starts_with("/find") {
                        format!("<title>Found</title><h1>You searched {}</h1>", path.split("q=").nth(1).unwrap_or(""))
                    } else {
                        let rows: String = (0..400).map(|i| format!("<p>filler paragraph {i} to make the page long</p>")).collect();
                        format!("<title>Start</title><form action=/find><input name=q aria-label=Query><button>Find</button></form>{rows}")
                    };
                    let r = format!("HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                    let _ = sock.write_all(r.as_bytes()).await;
                });
            }
        });
        let cfg = wang_browser::BrowserCfg { domains: vec![format!("127.0.0.1:{port}")], view_chars: 2_000, min_free_bytes: 0, ..Default::default() };
        let pool = wang_browser::Pool::new(jail, cfg);
        let ctx = ToolCtx::new(std::env::temp_dir()).with_browser(Some(pool.clone()));
        assert!(definitions_for(&ctx).iter().any(|d| d["name"] == "browser"));
        assert!(!definitions_for(&ToolCtx::new(std::env::temp_dir())).iter().any(|d| d["name"] == "browser"));

        let r = run_in("browser", &json!({"action": "open", "url": format!("http://127.0.0.1:{port}/")}), &ctx, "s1").await;
        assert!(!r.is_error, "{}", r.text);
        assert!(r.text.starts_with("Start\nhttp://127.0.0.1:") && r.text.contains("[1 input text \"Query\"]"), "{}", r.text);
        assert!(r.text.contains("[part 1 of ") && r.text.len() < 2_300, "long pages come in parts: {}", r.text.len());
        let r2 = run_in("browser", &json!({"action": "view", "part": 2}), &ctx, "s1").await;
        assert!(r2.text.contains("[part 2 of ") && !r2.text.contains("[1 input"), "{}", r2.text);
        let r = run_in("browser", &json!({"action": "type", "ref": 1, "text": "tea", "submit": true}), &ctx, "s1").await;
        assert!(r.text.contains("# You searched tea"), "{}", r.text);
        // Another conversation gets its own browser, on its own page.
        let other = run_in("browser", &json!({"action": "view"}), &ctx, "s2").await;
        assert!(other.text.contains("about:blank"), "{}", other.text);
        // Errors are explained.
        let r = run_in("browser", &json!({"action": "click"}), &ctx, "s1").await;
        assert!(r.is_error && r.text.contains("needs 'ref'"), "{}", r.text);
        let r = run_in("browser", &json!({"action": "open", "url": "file:///etc/passwd"}), &ctx, "s1").await;
        assert!(r.is_error && r.text.contains("only http"), "{}", r.text);
        assert_eq!(detail("browser", &json!({"action": "type", "ref": 3, "text": "hi"})), "type [3] \"hi\"");
        pool.shutdown().await;
    }

    /// The vm tool through the API the agent uses (skipped without `wang vm setup`).
    #[tokio::test(flavor = "current_thread")]
    async fn vm_tool_run_checkpoint_try() {
        let helper = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("jail");
        let cfg = wang_vm::VmCfg::default();
        let (Ok(assets), true) = (wang_vm::Assets::find(None), helper.exists()) else {
            eprintln!("skipping: needs `wang vm setup` and {}", helper.display());
            return;
        };
        if assets.base(&cfg).is_none() {
            return;
        }
        let jail = Arc::new(Jail::with_helper(helper).unwrap());
        let vms = wang_vm::Sessions::new(jail, assets, cfg, 4);
        let ctx = ToolCtx::new(std::env::temp_dir()).with_vm(Some(vms.clone()));
        assert!(definitions_for(&ctx).iter().any(|d| d["name"] == "vm"));

        let r = run_in("vm", &json!({"action": "run", "command": "echo hi > /work/x; python3 -c 'print(2+2)'"}), &ctx, "t").await;
        assert!(!r.is_error && r.text.contains("exit code: 0") && r.text.contains("4"), "{}", r.text);
        let r = run_in("vm", &json!({"action": "run", "command": "exit 3"}), &ctx, "t").await;
        assert!(r.is_error && r.text.contains("exit code: 3"), "{}", r.text);
        assert!(!run_in("vm", &json!({"action": "checkpoint"}), &ctx, "t").await.is_error);
        let r = run_in("vm", &json!({"action": "try", "commands": ["echo 1 > s", "echo 7 > s", "echo 4 > s"], "score": "cat s"}), &ctx, "t").await;
        assert!(r.text.contains("## [2] echo 7 > s ← kept") && r.text.contains("continues from alternative [2]"), "{}", r.text);
        let r = run_in("vm", &json!({"action": "run", "command": "cat s x"}), &ctx, "t").await;
        assert!(r.text.contains("7\nhi"), "{}", r.text);
        assert!(run_in("vm", &json!({"action": "rollback"}), &ctx, "t").await.text.contains("rolled back"));
        let r = run_in("vm", &json!({"action": "run", "command": "cat s"}), &ctx, "t").await;
        assert!(r.is_error, "rollback should drop s: {}", r.text);
        assert!(run_in("vm", &json!({"action": "try", "commands": ["true"]}), &ctx, "t").await.text.contains("at least two"));
        assert_eq!(detail("vm", &json!({"action": "try", "commands": ["a", "b"]})), "try 2 alternatives");
        vms.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn schedule_tool_add_list_remove() {
        let store = Arc::new(crate::schedule::Store::in_memory().unwrap());
        let ctx = ToolCtx::new(std::env::temp_dir()).with_schedules(Some(store.clone()));
        assert!(definitions_for(&ctx).iter().any(|d| d["name"] == "schedule"));
        let r = run_in("schedule", &json!({"action": "add", "when": "every weekday at 8am", "task": "summarize top HN posts"}), &ctx, "tg-7").await;
        assert!(!r.is_error && r.text.contains("Scheduled #1: weekdays at 08:00") && r.text.contains("this Telegram chat"), "{}", r.text);
        let r = run_in("schedule", &json!({"action": "add", "when": "whenever", "task": "x"}), &ctx, "tg-7").await;
        assert!(r.is_error && r.text.contains("every day at 8:00"), "{}", r.text);
        // Another conversation can't see or remove it.
        assert!(run_in("schedule", &json!({"action": "list"}), &ctx, "cli").await.text.contains("No scheduled tasks"));
        assert!(run_in("schedule", &json!({"action": "remove", "id": 1}), &ctx, "cli").await.text.contains("No schedule #1"));
        assert!(run_in("schedule", &json!({"action": "list"}), &ctx, "tg-7").await.text.contains("summarize top HN posts"));
        assert!(run_in("schedule", &json!({"action": "remove", "id": 1}), &ctx, "tg-7").await.text.contains("Removed"));
        assert_eq!(detail("schedule", &json!({"action": "add", "when": "in 2h", "task": "stretch"})), "add in 2h stretch");
    }
}

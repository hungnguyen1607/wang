use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub model: ModelCfg,
    pub agent: AgentCfg,
    pub ntfy: NtfyCfg,
    pub sandbox: SandboxCfg,
    pub web_fetch: WebFetchCfg,
    pub approvals: ApprovalsCfg,
    pub memory: MemoryCfg,
    pub routing: RoutingCfg,
    pub telegram: TelegramCfg,
    pub browser: BrowserConfig,
    pub vm: VmConfig,
    pub local: LocalConfig,
    pub mcp: McpConfig,
    pub orchestration: OrchestrationConfig,
}

/// `[orchestration]`: multi-part tasks get a planner, worker agents per step and a checker.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OrchestrationConfig {
    /// Plan multi-part tasks automatically (`/plan` always plans).
    pub enabled: bool,
    /// Worker agents at once (each may hold a browser or a VM).
    pub max_parallel: usize,
    pub max_steps: usize,
    /// Show the plan and wait for your OK before running it.
    pub approve_plan: bool,
}

impl Default for OrchestrationConfig {
    fn default() -> Self {
        Self { enabled: true, max_parallel: 2, max_steps: 6, approve_plan: false }
    }
}

impl OrchestrationConfig {
    pub fn to_cfg(&self) -> crate::orchestrate::OrchestrationCfg {
        crate::orchestrate::OrchestrationCfg {
            enabled: self.enabled,
            max_parallel: self.max_parallel.clamp(1, 8),
            max_steps: self.max_steps.clamp(2, 12),
            approve_plan: self.approve_plan,
            ..Default::default()
        }
    }
}

/// `[mcp]`: `wang mcp`, wang's tools for Claude Code / Claude Desktop.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    /// Allow shell commands that write files or use the network. wang's own approval flow isn't
    /// available over MCP, so the client's permission prompt is then the only check. Off by default.
    pub allow_privileged: bool,
}

/// `[local]`: a small local model (llama.cpp, in the jail) for routine work: history summaries.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalConfig {
    /// Off by default: on this laptop a 1.5B model summarizes in 20–40 s and sometimes invents or
    /// mislabels facts, to save ~$0.0002 per summary (bench/m9-local.md).
    pub enabled: bool,
    pub threads: u32,
    /// Context window in tokens; summaries of longer histories go to the main model.
    pub context: u32,
    /// RAM cap for the model server (weights ~1.1 GB + KV cache).
    pub memory: String,
    /// Stop the server after this long unused.
    pub idle_timeout: String,
    /// Default ~/.cache/wang/local.
    pub dir: String,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self { enabled: false, threads: 4, context: 16384, memory: "3G".into(), idle_timeout: "10m".into(), dir: String::new() }
    }
}

impl LocalConfig {
    pub fn root(&self) -> PathBuf {
        if self.dir.trim().is_empty() {
            dirs::cache_dir().unwrap_or_else(|| PathBuf::from(".")).join("wang/local")
        } else {
            expand_tilde(self.dir.trim())
        }
    }

    pub fn to_cfg(&self) -> Result<crate::model::local::LocalCfg> {
        use wang_jail::policy::{parse_duration, parse_size};
        Ok(crate::model::local::LocalCfg {
            dir: self.root(),
            threads: self.threads.clamp(1, 64),
            ctx: self.context.clamp(2048, 32768),
            memory_bytes: parse_size(&self.memory)?,
            idle_timeout: parse_duration(&self.idle_timeout)?,
        })
    }
}

/// `[vm]`: Firecracker micro-VMs for big or risky jobs (`wang vm setup` first).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VmConfig {
    pub enabled: bool,
    pub vcpus: u32,
    /// Guest RAM per VM.
    pub memory: String,
    /// Guest network allow-list (via wang's egress proxy). Empty = no network.
    pub domains: Vec<String>,
    /// VMs at once (one per conversation, plus forks while trying approaches).
    pub max_vms: usize,
    /// Merge identical memory pages across VMs (KSM).
    pub ksm: bool,
    pub timeout_secs: u64,
    /// Where Firecracker, the kernel, the image and snapshots live (default ~/.cache/wang/vm).
    pub dir: String,
}

impl Default for VmConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            vcpus: 2,
            memory: "512M".into(),
            domains: vec!["*".into()],
            max_vms: 8,
            ksm: true,
            timeout_secs: 600,
            dir: String::new(),
        }
    }
}

impl VmConfig {
    pub fn to_cfg(&self) -> Result<wang_vm::VmCfg> {
        Ok(wang_vm::VmCfg {
            vcpus: self.vcpus.clamp(1, 32),
            mem_mib: (wang_jail::policy::parse_size(&self.memory)? >> 20).max(128),
            domains: self.domains.clone(),
            ksm: self.ksm,
        })
    }

    pub fn root(&self) -> PathBuf {
        if self.dir.trim().is_empty() { wang_vm::Assets::default_root() } else { expand_tilde(self.dir.trim()) }
    }
}

/// `[browser]`: headless Chrome for the `browser` tool, each one in its own jail.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserConfig {
    pub enabled: bool,
    /// Chrome / chrome-headless-shell path; found automatically when empty.
    pub binary: String,
    /// What pages may load (jail egress allow-list). Private addresses need an exact entry.
    pub domains: Vec<String>,
    /// RAM cap per browser (no swap).
    pub memory: String,
    pub cpus: f64,
    /// Browsers at once (one per conversation).
    pub max_browsers: usize,
    /// Close a browser after this long unused.
    pub idle_timeout: String,
    /// Don't start a browser when less than this is free.
    pub min_free: String,
    /// Characters per page view part (~4 per token).
    pub view_chars: usize,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            binary: String::new(),
            domains: vec!["*".into()],
            memory: "1G".into(),
            cpus: 2.0,
            max_browsers: 2,
            idle_timeout: "5m".into(),
            min_free: "768M".into(),
            view_chars: 12_000,
        }
    }
}

impl BrowserConfig {
    pub fn to_cfg(&self) -> Result<wang_browser::BrowserCfg> {
        use wang_jail::policy::{parse_duration, parse_size};
        Ok(wang_browser::BrowserCfg {
            binary: (!self.binary.trim().is_empty()).then(|| expand_tilde(self.binary.trim())),
            domains: self.domains.clone(),
            memory_bytes: parse_size(&self.memory)?,
            cpus: self.cpus,
            max_browsers: self.max_browsers.max(1),
            idle_timeout: parse_duration(&self.idle_timeout)?,
            min_free_bytes: parse_size(&self.min_free)?,
            view_chars: self.view_chars.clamp(1_000, 100_000),
            ..Default::default()
        })
    }
}

/// `[telegram]`: chat with wang from your phone. The bot token is TELEGRAM_BOT_TOKEN in .env.
/// Only the listed user ids are served; everyone else is ignored (and logged, so you can find
/// your own id). Bot chats are not end-to-end encrypted: Telegram sees prompts and replies.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TelegramCfg {
    pub allowed_user_ids: Vec<i64>,
    /// Edit the reply in place as it streams (false = one message at the end).
    pub stream: bool,
    /// Also send alerts and approval requests (from any session) to Telegram.
    pub alerts: bool,
    /// API endpoint (tests point this at a fake server).
    pub api_base: String,
}

impl Default for TelegramCfg {
    fn default() -> Self {
        Self { allowed_user_ids: vec![], stream: true, alerts: true, api_base: "https://api.telegram.org".into() }
    }
}

/// `[routing]`: send step 1 (planning) to the main model and later tool-loop steps to a
/// cheaper one. Off by default; turn on only if `wang eval --routing` shows success holds.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingCfg {
    pub enabled: bool,
    pub cheap_model: String,
}

impl Default for RoutingCfg {
    fn default() -> Self {
        Self { enabled: false, cheap_model: "anthropic/claude-haiku-5.5".into() }
    }
}

impl RoutingCfg {
    pub fn cheap(&self) -> Option<&str> {
        self.enabled.then_some(self.cheap_model.as_str())
    }
}

/// `[approvals]`: privileged actions wait for your yes/no (phone via ntfy, or terminal).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApprovalsCfg {
    pub enabled: bool,
    /// Unanswered requests are denied after this long ("10m", "90s").
    pub timeout: String,
}

impl Default for ApprovalsCfg {
    fn default() -> Self {
        Self { enabled: true, timeout: "10m".into() }
    }
}

/// `[memory]`: facts in ~/.local/share/wang/memory.md.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryCfg {
    pub enabled: bool,
}

impl Default for MemoryCfg {
    fn default() -> Self {
        Self { enabled: true }
    }
}

pub fn memory_path() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("wang/memory.md")
}

/// `[sandbox]`: the jail for `shell` and `read_file`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxCfg {
    /// false = run tools unsandboxed (not recommended).
    pub enabled: bool,
    /// Visible read-only. Secrets (~/.ssh, ~/.gnupg, credentials, browser profiles, wang's own config) are always hidden.
    pub read: Vec<String>,
    /// Visible read-write: writes reach your real files.
    pub write: Vec<String>,
    /// Visible copy-on-write: writes are thrown away after each command.
    pub overlay: Vec<String>,
    /// Extra paths to hide.
    pub hide: Vec<String>,
    /// Network allow-list for shell commands (empty = no network).
    pub domains: Vec<String>,
    pub memory: String,
    pub cpus: f64,
    pub pids: u32,
    pub tmp: String,
}

impl Default for SandboxCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            read: vec!["~".into()],
            write: vec![],
            overlay: vec![],
            hide: vec![],
            domains: vec![],
            memory: "512M".into(),
            cpus: 1.0,
            pids: 128,
            tmp: "256M".into(),
        }
    }
}

impl SandboxCfg {
    pub fn policy(&self) -> Result<wang_jail::Policy> {
        use wang_jail::policy::{expand, parse_size};
        let paths = |v: &[String]| v.iter().map(|p| expand(p)).collect::<Vec<_>>();
        let mut p = wang_jail::Policy {
            read: paths(&self.read),
            write: paths(&self.write),
            overlay: paths(&self.overlay),
            hide: paths(&self.hide),
            domains: self.domains.clone(),
            ..Default::default()
        };
        p.limits.memory_bytes = parse_size(&self.memory)?;
        p.limits.tmp_bytes = parse_size(&self.tmp)?;
        p.limits.cpus = self.cpus;
        p.limits.pids = self.pids;
        Ok(p)
    }
}

/// `[web_fetch]`: in-process, but held to a domain allow-list; private addresses refused.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebFetchCfg {
    pub domains: Vec<String>,
    /// Allow localhost / LAN / tailnet addresses (default false).
    pub allow_private: bool,
}

impl Default for WebFetchCfg {
    fn default() -> Self {
        Self { domains: vec!["*".into()], allow_private: false }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ModelCfg {
    /// "openrouter" (default) or "anthropic" (direct Claude API).
    pub provider: String,
    /// Model id; defaults per provider (see `model_id`).
    pub model: Option<String>,
    pub max_tokens: u32,
    /// OpenRouter `reasoning.effort`: none | minimal | low | medium | high | xhigh | max. Unset = model default.
    pub reasoning_effort: Option<String>,
    /// Anthropic provider only: output_config.effort.
    pub effort: String,
    /// Anthropic provider only: server-side refusal fallback.
    pub fallbacks: bool,
    /// Override the API base URL (e.g. the M6 token proxy, or the M5 fake model server).
    pub base_url: Option<String>,
}

impl Default for ModelCfg {
    fn default() -> Self {
        Self {
            provider: "openrouter".into(),
            model: None,
            max_tokens: 16_000,
            reasoning_effort: None,
            effort: "medium".into(),
            fallbacks: true,
            base_url: None,
        }
    }
}

impl ModelCfg {
    pub fn model_id(&self) -> String {
        self.model.clone().unwrap_or_else(|| match self.provider.as_str() {
            "anthropic" => "claude-opus-5-5".into(),
            _ => "anthropic/claude-haiku-5.5".into(),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AgentCfg {
    /// Max model calls per task before wang stops and asks you.
    pub max_steps: u32,
    /// Working directory for the shell tool and relative paths.
    pub workdir: String,
    /// Tool outputs longer than this (chars) keep head + tail only.
    pub tool_output_cap: usize,
    /// Abort the task when the same tool call (same tool + same input) fails this many times.
    pub loop_fail_limit: u32,
    /// Abort the task when the same tool call is made this many times, even if it succeeds.
    pub loop_repeat_limit: u32,
    /// Max spend per task in USD before pausing to ask you (0 = no budget).
    pub budget_usd: f64,
    /// Summarize older history once a prompt exceeds this many tokens (0 = never).
    pub compact_at_tokens: u64,
    /// User turns kept word-for-word after summarizing.
    pub compact_keep_turns: usize,
}

impl Default for AgentCfg {
    fn default() -> Self {
        Self { max_steps: 20, workdir: "~".into(), tool_output_cap: 12_000, loop_fail_limit: 3, loop_repeat_limit: 5, budget_usd: 0.25, compact_at_tokens: 60_000, compact_keep_turns: 2 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NtfyCfg {
    pub server: String,
    /// Empty = alerts off. On ntfy.sh the topic name is the only secret: make it long and random.
    pub topic: String,
    /// Where Approve / Deny buttons post; default "<topic>-reply".
    pub reply_topic: String,
}

impl Default for NtfyCfg {
    fn default() -> Self {
        Self { server: "https://ntfy.sh".into(), topic: String::new(), reply_topic: String::new() }
    }
}

pub const EXAMPLE_CONFIG: &str = r#"# wang config — ~/.config/wang/config.toml
# API keys live in ~/.config/wang/.env, not here.

[model]
provider = "openrouter"                  # openrouter | anthropic
model = "anthropic/claude-haiku-5.5"     # cheap default; strong option: "anthropic/claude-opus-5.5"
max_tokens = 16000
# reasoning_effort = "low"               # openrouter: none | minimal | low | medium | high | xhigh | max
# base_url = "http://127.0.0.1:8787"     # token proxy (M6) or fake model server (M5)

[agent]
max_steps = 20
workdir = "~"
tool_output_cap = 12000                  # chars; longer tool outputs keep head + tail
loop_fail_limit = 3                      # same call failing this often aborts the task (+ ntfy alert)
loop_repeat_limit = 5                    # same call made this often aborts the task
budget_usd = 0.25                        # per task; when reached, wang pauses and asks (0 = no budget)
compact_at_tokens = 60000                # summarize older history past this prompt size (0 = never)
compact_keep_turns = 2                   # recent user turns kept word-for-word

[approvals]
enabled = true                           # privileged shell commands (write paths / network) ask first
timeout = "10m"                          # no answer = denied

[routing]                                # main model for step 1, cheap model for later steps
enabled = false                          # turn on only if `wang eval --routing` shows success holds
cheap_model = "anthropic/claude-haiku-5.5"

[memory]
enabled = true                           # facts in ~/.local/share/wang/memory.md (plain Markdown, edit freely)

[sandbox]                                # shell and read_file run in wang-jail
enabled = true
read = ["~"]                             # visible read-only; secrets (~/.ssh, keys, tokens, browser profiles) always hidden
write = []                               # visible read-write, e.g. ["~/notes"]
overlay = []                             # copy-on-write: writes are discarded after each command
hide = []                                # extra paths to hide
domains = []                             # shell network allow-list, e.g. ["api.github.com", "*.pypi.org"]; empty = none
memory = "512M"
cpus = 1.0
pids = 128

[web_fetch]
domains = ["*"]                          # allow-list; private/local addresses are refused unless listed exactly
allow_private = false

[browser]                                # headless Chrome for the browser tool, each in its own jail
enabled = true
binary = ""                              # empty = find one (`npx playwright install chromium-headless-shell`)
domains = ["*"]                          # what pages may load; private/local addresses need an exact entry
memory = "1G"                            # RAM cap per browser
max_browsers = 2                         # at once (one per conversation); idle ones close after idle_timeout
idle_timeout = "5m"

[vm]                                     # Firecracker micro-VMs; run `wang vm setup` once
enabled = true
vcpus = 2
memory = "512M"                          # guest RAM per VM
domains = ["*"]                          # guest network via wang's egress proxy; [] = none
max_vms = 8

[local]                                  # small local model for history summaries ($0); `wang local setup` once
enabled = false                          # off: slow on CPU and less accurate than the main model (bench/m9-local.md)
threads = 4
context = 16384                          # longer histories are summarized by the main model
memory = "3G"

[orchestration]                          # multi-part tasks: planner → worker agents per step → checker → answer
enabled = true                           # `/plan <task>` always plans
max_parallel = 2                         # worker agents at once
approve_plan = false                     # true = show the plan and wait for your OK first

[mcp]                                    # `wang mcp` for Claude Code / Claude Desktop
allow_privileged = false                 # true = shell may write/use the network, approved only by Claude's prompt

[telegram]                               # bot token: TELEGRAM_BOT_TOKEN in .env (from @BotFather)
allowed_user_ids = []                    # your numeric id; message the bot once and it's printed in the log
stream = true                            # edit the reply live as it's written
alerts = true                            # alerts + Approve/Deny buttons for every session go to Telegram too

[ntfy]
server = "https://ntfy.sh"
topic = ""                               # empty = alerts off; use a long random name on ntfy.sh
# access token (optional): NTFY_TOKEN in .env
"#;

pub const EXAMPLE_ENV: &str = "# wang secrets — keep this file chmod 600\nOPENROUTER_API_KEY=\n# ANTHROPIC_API_KEY=\n# NTFY_TOKEN=\n# TELEGRAM_BOT_TOKEN=\n";

fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("wang")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn env_path() -> PathBuf {
    config_dir().join(".env")
}

/// One-time move from the project's old name: `warp` folders (config, data, cache, state) become
/// `wang`, and `warp.db` becomes `wang.db`. Only when the new folder doesn't exist yet.
pub fn migrate_from_warp() {
    for base in [dirs::config_dir(), dirs::data_dir(), dirs::cache_dir(), dirs::state_dir()].into_iter().flatten() {
        let (old, new) = (base.join("warp"), base.join("wang"));
        if old.is_dir() && !new.exists() && std::fs::rename(&old, &new).is_ok() {
            eprintln!("moved {} → {}", old.display(), new.display());
            for ext in ["", "-wal", "-shm"] {
                let _ = std::fs::rename(new.join(format!("warp.db{ext}")), new.join(format!("wang.db{ext}")));
            }
        }
    }
}

/// One SQLite file holds the ledger and the traces.
pub fn db_path() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("wang/wang.db")
}

/// `$XDG_RUNTIME_DIR/wang.sock` (a 0700 per-user dir), else under the data dir.
pub fn socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("WANG_SOCKET") {
        return PathBuf::from(dir);
    }
    match std::env::var("XDG_RUNTIME_DIR") {
        Ok(d) if !d.is_empty() => PathBuf::from(d).join("wang.sock"),
        _ => dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("wang/wang.sock"),
    }
}

/// Load `~/.config/wang/.env`, then `./.env`. Never overrides variables already set.
pub fn load_env() {
    let _ = dotenvy::from_path(env_path());
    let _ = dotenvy::dotenv();
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if p == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    }
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = dirs::home_dir() {
            return h.join(rest);
        }
    }
    PathBuf::from(p)
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let cfg: Config = if path.exists() {
            let s = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
            toml::from_str(&s).with_context(|| format!("parsing {}", path.display()))?
        } else {
            Config::default()
        };
        if !matches!(cfg.model.provider.as_str(), "openrouter" | "anthropic") {
            bail!("unknown provider '{}' (expected openrouter or anthropic)", cfg.model.provider);
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_parses() {
        let c: Config = toml::from_str(EXAMPLE_CONFIG).unwrap();
        assert_eq!(c.model.provider, "openrouter");
        assert_eq!(c.model.model_id(), "anthropic/claude-haiku-5.5");
        assert_eq!(c.agent.max_steps, 20);
        assert_eq!(c.agent.loop_fail_limit, 3);
        assert!(c.ntfy.topic.is_empty());
        assert!(c.sandbox.enabled);
        let p = c.sandbox.policy().unwrap();
        assert_eq!(p.limits.memory_bytes, 512 << 20);
        assert_eq!(p.read, vec![wang_jail::policy::home()]);
        assert_eq!(c.web_fetch.domains, vec!["*"]);
        assert!(c.telegram.allowed_user_ids.is_empty() && c.telegram.stream && c.telegram.alerts);
        let t: Config = toml::from_str("[telegram]\nallowed_user_ids = [42]\nstream = false").unwrap();
        assert_eq!(t.telegram.allowed_user_ids, vec![42]);
        assert!(!t.telegram.stream);
    }

    #[test]
    fn defaults_per_provider() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.model.model_id(), "anthropic/claude-haiku-5.5");
        let c: Config = toml::from_str("[model]\nprovider = \"anthropic\"").unwrap();
        assert_eq!(c.model.model_id(), "claude-opus-5-5");
    }
}

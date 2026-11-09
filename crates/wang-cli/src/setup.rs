//! First run and health checks: `wang doctor`, `wang setup`, and what bare `wang` does.
//!
//! Bare `wang`: set up on first run (asking before anything outside wang's own files), then make
//! sure the daemon is running (systemd service, or a background process) and open the chat.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use wang_daemon::config::{self, Config};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    /// Optional feature not available.
    Off,
    /// Needed for the core to work.
    Missing,
}

pub struct Check {
    pub name: &'static str,
    pub level: Level,
    pub detail: String,
    pub fix: Option<String>,
}

fn which(cmd: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?.to_str()?.split(':').map(|d| Path::new(d).join(cmd)).find(|p| p.is_file())
}

fn exe() -> PathBuf {
    std::env::current_exe().and_then(|p| p.canonicalize()).unwrap_or_else(|_| PathBuf::from("wang"))
}

/// Is `wang mcp` registered with Claude Code (user scope)? Read-only: looks at ~/.claude.json.
pub fn claude_has_wang() -> bool {
    let Some(home) = dirs::home_dir() else { return false };
    std::fs::read_to_string(home.join(".claude.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .is_some_and(|v| v["mcpServers"].get("wang").is_some())
}

pub fn api_key_set() -> bool {
    ["OPENROUTER_API_KEY", "ANTHROPIC_API_KEY"].iter().any(|k| std::env::var(k).is_ok_and(|v| !v.trim().is_empty()))
}

/// What works on this machine, what doesn't, and how to fix it. Changes nothing.
pub async fn doctor(cfg: &Config) -> Vec<Check> {
    let mut c = vec![];
    let mut add = |name, level, detail: String, fix: Option<&str>| c.push(Check { name, level, detail, fix: fix.map(String::from) });

    // Core: the sandbox.
    match wang_jail::Jail::new() {
        Ok(j) => match j.check().await {
            Ok(()) => add("sandbox", Level::Ok, format!("bubblewrap {} · cgroups v2", j.bwrap.display()), None),
            Err(e) => add("sandbox", Level::Missing, format!("{e:#}"), Some("check that user namespaces are allowed and cgroups v2 is delegated (systemd user session)")),
        },
        Err(e) => add("sandbox", Level::Missing, format!("{e:#}"), Some("sudo apt install bubblewrap")),
    }
    // Brains: Claude Code (via MCP) and/or an API key for wang's own agent.
    let claude = which("claude");
    match (&claude, claude_has_wang()) {
        (Some(_), true) => add("Claude Code", Level::Ok, "wang is plugged in (`wang mcp`)".into(), None),
        (Some(_), false) => add("Claude Code", Level::Off, "installed, wang not plugged in".into(), Some("wang setup   (or: claude mcp add --scope user wang -- wang mcp)")),
        (None, _) => add("Claude Code", Level::Off, "not installed".into(), Some("optional: https://claude.com/claude-code, then `wang setup`")),
    }
    if api_key_set() {
        add("API key", Level::Ok, "set: wang's own agent can run (terminal, Telegram)".into(), None);
    } else {
        add("API key", Level::Off, "not set: wang's own agent is off; use wang from Claude instead".into(), Some(&format!("put OPENROUTER_API_KEY=... in {}", config::env_path().display())));
    }
    // Optional infrastructure.
    match wang_browser::find_chrome() {
        Ok(p) => add("browser", Level::Ok, p.display().to_string(), None),
        Err(_) => add("browser", Level::Off, "no Chrome that can run in the sandbox".into(), Some("npx playwright install chromium-headless-shell")),
    }
    let kvm = std::fs::File::options().read(true).write(true).open("/dev/kvm").is_ok();
    let vm_ready = wang_vm::Assets::find(Some(&cfg.vm.root())).ok().and_then(|a| cfg.vm.to_cfg().ok().map(|v| a.base(&v).is_some())).unwrap_or(false);
    if vm_ready {
        add("micro-VMs", Level::Ok, "Firecracker + image + base snapshot ready".into(), None);
    } else if !kvm {
        add("micro-VMs", Level::Off, "no usable /dev/kvm".into(), Some("enable virtualization in the BIOS and add yourself to the kvm group"));
    } else {
        let mut need = vec![];
        if which("docker").is_none() {
            need.push("docker");
        }
        if which("mksquashfs").is_none() {
            need.push("squashfs-tools");
        }
        let fix = if need.is_empty() { "wang vm setup".to_string() } else { format!("sudo apt install {}, then `wang vm setup`", need.join(" ")) };
        add("micro-VMs", Level::Off, "KVM works; VM image not built yet".into(), Some(&fix));
    }
    if which("systemctl").is_some() && dirs::config_dir().is_some_and(|d| d.join("systemd/user/wang.service").exists()) {
        add("background service", Level::Ok, "systemd user service installed".into(), None);
    } else {
        add("background service", Level::Off, "not installed (bare `wang` starts the daemon itself)".into(), Some("wang service install"));
    }
    c
}

pub fn print_doctor(checks: &[Check]) {
    let color = std::io::stdout().is_terminal();
    let paint = |l: Level, s: &str| -> String {
        if !color {
            return s.to_string();
        }
        let code = match l {
            Level::Ok => "32",
            Level::Off => "33",
            Level::Missing => "31",
        };
        format!("\x1b[{code}m{s}\x1b[0m")
    };
    for c in checks {
        let mark = match c.level {
            Level::Ok => "✓",
            Level::Off => "–",
            Level::Missing => "✗",
        };
        println!("  {} {:<19} {}", paint(c.level, mark), c.name, c.detail);
        if let (Some(f), false) = (&c.fix, c.level == Level::Ok) {
            println!("    {:<19} → {f}", "");
        }
    }
}

fn ask(prompt: &str, lines: &mut impl Iterator<Item = String>) -> String {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    lines.next().unwrap_or_default().trim().to_string()
}

fn yes(answer: &str, default: bool) -> bool {
    match answer.to_ascii_lowercase().as_str() {
        "" => default,
        a => a.starts_with('y'),
    }
}

fn marker() -> PathBuf {
    config::env_path().with_file_name(".onboarded")
}

pub fn onboarded() -> bool {
    marker().exists()
}

/// Write `KEY=value` into the .env file (replacing an existing line), keeping it chmod 600.
fn set_env(key: &str, value: &str) -> Result<()> {
    let path = config::env_path();
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let mut found = false;
    let mut out: Vec<String> = old
        .lines()
        .map(|l| {
            let bare = l.trim_start_matches('#').trim();
            if bare.starts_with(&format!("{key}=")) {
                found = true;
                format!("{key}={value}")
            } else {
                l.to_string()
            }
        })
        .collect();
    if !found {
        out.push(format!("{key}={value}"));
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&path, out.join("\n") + "\n")?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// The guided first run. Every change outside wang's own files is asked first.
pub async fn setup(cfg_path: &Path) -> Result<()> {
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines().map_while(Result::ok);
    println!("wang setup\n");
    crate::init(cfg_path)?;
    config::load_env();
    let cfg = Config::load(cfg_path)?;
    println!("\nChecking this machine:");
    let checks = doctor(&cfg).await;
    print_doctor(&checks);
    if checks.iter().any(|c| c.level == Level::Missing) {
        println!("\nThe sandbox isn't working, so wang can't run tools safely yet. Fix the ✗ line above and run `wang setup` again.");
        return Ok(());
    }

    // 1. Claude Code: the no-API-key way to use wang.
    if which("claude").is_some() && !claude_has_wang() {
        println!("\nClaude Code is installed. Plugging wang in gives Claude a sandboxed shell, browser and VMs, using your own Claude login (no API key).");
        if yes(&ask("Add wang to Claude Code? [Y/n] ", &mut lines), true) {
            let exe = exe();
            let st = Command::new("claude").args(["mcp", "add", "--scope", "user", "wang", "--"]).arg(&exe).arg("mcp").status();
            match st {
                Ok(s) if s.success() => println!("  ✓ added: in Claude Code, ask e.g. \"use wang's VM to try this script\""),
                _ => println!("  couldn't add it; run: claude mcp add --scope user wang -- {} mcp", exe.display()),
            }
        }
    }

    // 2. API key for wang's own agent (terminal, Telegram).
    if !api_key_set() {
        println!("\nwang's own agent (`wang` chat, Telegram) needs a model API key. Your key stays in {} and goes only to the provider.", config::env_path().display());
        let k = ask("Paste an OpenRouter key (sk-or-...), or press Enter to skip: ", &mut lines);
        if !k.is_empty() {
            if !k.starts_with("sk-") {
                println!("  that doesn't look like a key; skipped (edit {} later)", config::env_path().display());
            } else {
                set_env("OPENROUTER_API_KEY", &k)?;
                // SAFETY: single-threaded setup flow, before the daemon starts.
                unsafe { std::env::set_var("OPENROUTER_API_KEY", &k) };
                println!("  ✓ saved");
            }
        }
    }

    // 3. Background service.
    let unit = dirs::config_dir().map(|d| d.join("systemd/user/wang.service"));
    if api_key_set() && which("systemctl").is_some() && unit.as_ref().is_some_and(|u| !u.exists()) {
        if yes(&ask("\nStart wang in the background at login (systemd user service)? [Y/n] ", &mut lines), true) {
            crate::service_install()?;
            let ok = Command::new("systemctl").args(["--user", "daemon-reload"]).status().is_ok_and(|s| s.success())
                && Command::new("systemctl").args(["--user", "enable", "--now", "wang"]).status().is_ok_and(|s| s.success());
            println!("{}", if ok { "  ✓ running" } else { "  couldn't start it; try: systemctl --user enable --now wang" });
        }
    }

    std::fs::write(marker(), "")?;
    println!("\nDone. Optional extras: `wang doctor` lists them (browser, micro-VMs, Telegram).");
    if api_key_set() {
        println!("Run `wang` to chat.");
    } else if claude_has_wang() {
        println!("Open Claude Code and ask it to use wang.");
    }
    Ok(())
}

/// Make sure the daemon answers on its socket: start the systemd service, or a background
/// `wang run` with its log under the state dir.
pub async fn ensure_daemon() -> Result<()> {
    let sock = config::socket_path();
    if wang_daemon::server::Client::connect(&sock).await.is_ok() {
        return Ok(());
    }
    let unit = dirs::config_dir().map(|d| d.join("systemd/user/wang.service"));
    let via_systemd = unit.is_some_and(|u| u.exists())
        && Command::new("systemctl").args(["--user", "start", "wang"]).status().is_ok_and(|s| s.success());
    let log = dirs::state_dir().or_else(dirs::data_dir).unwrap_or_else(std::env::temp_dir).join("wang/daemon.log");
    if !via_systemd {
        if let Some(d) = log.parent() {
            std::fs::create_dir_all(d)?;
        }
        let f = std::fs::File::options().create(true).append(true).open(&log)?;
        use std::os::unix::process::CommandExt;
        Command::new(exe())
            .arg("run")
            .stdin(Stdio::null())
            .stdout(f.try_clone()?)
            .stderr(f)
            .process_group(0) // not tied to this terminal
            .spawn()
            .context("starting the wang daemon")?;
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if wang_daemon::server::Client::connect(&sock).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!(
        "the wang daemon didn't start; see {}",
        if via_systemd { "journalctl --user -u wang".to_string() } else { log.display().to_string() }
    )
}

//! First run and health checks: `wang doctor`, `wang setup`, and what bare `wang` does.
//!
//! Bare `wang`: set up on first run (asking before anything outside wang's own files), then make
//! sure the daemon is running (systemd service, or a background process) and open the chat.

use std::io::{IsTerminal, Write};
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

pub(crate) fn which(cmd: &str) -> Option<PathBuf> {
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

/// Can wang's own agent think? An API key, or a subscription brain (Claude Code / Codex).
pub fn api_key_set() -> bool {
    let env = |k: &str| std::env::var(k).is_ok_and(|v| !v.trim().is_empty());
    if env("OPENROUTER_API_KEY") || env("ANTHROPIC_API_KEY") {
        return true;
    }
    let provider = Config::load(&config::config_path()).map(|c| c.model.provider).unwrap_or_default();
    match provider.as_str() {
        "deepseek" => env("DEEPSEEK_API_KEY"),
        "openai" => env("OPENAI_API_KEY"),
        p => wang_daemon::brain::Kind::from_provider(p).is_some(),
    }
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
    match (cfg.model.provider.as_str(), api_key_set()) {
        ("claude-code", _) => match wang_daemon::brain::claude_signed_in() {
            Some(_) => add("brain", Level::Ok, "your Claude subscription (Claude Code, jailed, wang's tools only)".into(), None),
            None => add("brain", Level::Missing, "set to your Claude subscription, but Claude Code isn't signed in".into(), Some("run `claude` once and log in")),
        },
        ("codex", _) => match wang_daemon::brain::codex_signed_in() {
            Some(_) => add("brain", Level::Ok, "your ChatGPT subscription (Codex, jailed, wang's tools only)".into(), None),
            None => add("brain", Level::Missing, "set to your ChatGPT subscription, but Codex isn't signed in".into(), Some("codex login")),
        },
        (p, true) => add("brain", Level::Ok, format!("{p} API key: wang's own agent can run (terminal, Telegram)"), None),
        (_, false) => add("brain", Level::Off, "none: wang's own agent is off; use wang from Claude Code instead".into(), Some("wang setup   (use your Claude/ChatGPT subscription, or a DeepSeek/OpenRouter key)")),
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
        add("background service", Level::Off, "not installed (bare `wang` starts the daemon itself)".into(), Some("wang setup awake"));
    }
    if cfg.desktop.enabled {
        match wang_desktop::Desktop::available() {
            Ok(()) if wang_desktop::Desktop::new(wang_desktop::Desktop::default_dir()).screen_ready() => {
                add("desktop", Level::Ok, "apps (accessibility) + screen control".into(), None)
            }
            Ok(()) => add("desktop", Level::Off, "apps through accessibility; no screenshots/keys yet".into(), Some("wang setup screen")),
            Err(e) => add("desktop", Level::Off, format!("{e:#}"), None),
        }
    }
    let token = std::env::var("TELEGRAM_BOT_TOKEN").is_ok_and(|t| !t.trim().is_empty());
    match (token, cfg.telegram.allowed_user_ids.len()) {
        (true, 0) => add("phone", Level::Off, "bot token set, no phone paired (the bot ignores everyone)".into(), Some("wang setup phone")),
        (true, n) => add("phone", Level::Ok, format!("Telegram bot, {n} paired account(s)"), None),
        (false, _) => add("phone", Level::Off, "Telegram not set up".into(), Some("wang setup phone")),
    }
    if which("systemctl").is_some() {
        let (ok, detail, fix) = crate::phone::always_on_status();
        add("always on", if ok { Level::Ok } else { Level::Off }, detail, fix.as_deref());
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

/// Answers typed at the terminal, one line each. Not holding stdin's lock between prompts, so
/// `wang login`'s "press Enter when done" can read stdin in between.
pub(crate) type Lines = Box<dyn Iterator<Item = String>>;

pub(crate) fn stdin_lines() -> Lines {
    Box::new(std::iter::from_fn(|| {
        let mut s = String::new();
        match std::io::stdin().read_line(&mut s) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(s),
        }
    }))
}

pub(crate) fn ask(prompt: &str, lines: &mut Lines) -> String {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    lines.next().unwrap_or_default().trim().to_string()
}

pub(crate) fn yes(answer: &str, default: bool) -> bool {
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
pub(crate) fn set_env(key: &str, value: &str) -> Result<()> {
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

/// Which part of `wang setup` to run (all of them by default).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Step {
    /// Telegram: create/check your bot, pair your phone (QR code)
    Phone,
    /// Background service, start at boot, keep running with the lid closed / when idle
    Awake,
    /// Sign wang's browser in to your sites
    Logins,
    /// Let wang see and use the screen (screenshots, clicks, keys): GNOME asks once
    Screen,
}

/// Does the OpenRouter key work? Some(true/false), or None if OpenRouter couldn't be reached.
async fn openrouter_key_ok(key: &str) -> Option<bool> {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(15)).build().ok()?;
    let r = http.get("https://openrouter.ai/api/v1/key").bearer_auth(key).send().await.ok()?;
    match r.status().as_u16() {
        200 => Some(true),
        401 | 403 => Some(false),
        _ => None,
    }
}

/// The guided first run. Every change outside wang's own files is asked first.
/// With `only`, just that step (e.g. `wang setup phone` to pair another phone).
pub async fn setup(cfg_path: &Path, only: Option<Step>) -> Result<()> {
    let mut lines = stdin_lines();
    let lines = &mut lines;
    println!("wang setup\n");
    crate::init(cfg_path)?;
    config::load_env();
    let cfg = Config::load(cfg_path)?;
    if only.is_none() {
        println!("\nChecking this machine:");
        let checks = doctor(&cfg).await;
        print_doctor(&checks);
        if checks.iter().any(|c| c.level == Level::Missing) {
            println!("\nThe sandbox isn't working, so wang can't run tools safely yet. Fix the ✗ line above and run `wang setup` again.");
            return Ok(());
        }
        brains(cfg_path, lines).await?;
    }
    let run = |s: Step| only.is_none_or(|o| o == s);

    let mut phone = None;
    if run(Step::Phone) {
        if api_key_set() {
            phone = crate::phone::phone(cfg_path, lines).await?;
        } else {
            println!("\n── Phone (Telegram) ──\n  – needs wang's own agent, so an API key first: run `wang setup` and paste one.");
        }
    } else if let Some(t) = std::env::var("TELEGRAM_BOT_TOKEN").ok().filter(|t| !t.trim().is_empty()) {
        // Not pairing this time, but still prove the phone path works at the end.
        let c = Config::load(cfg_path)?;
        if let Ok(bot) = wang_daemon::telegram::Bot::new(&c.telegram.api_base, &t)
            && let Ok(me) = bot.call("getMe", serde_json::json!({})).await
        {
            phone = Some(crate::phone::Phone { bot_username: me["username"].as_str().unwrap_or("").into(), user_ids: c.telegram.allowed_user_ids });
        }
    }
    if run(Step::Awake) {
        crate::phone::always_on(lines)?;
    }
    if run(Step::Logins) {
        logins(cfg_path, lines).await?;
    }
    if only == Some(Step::Screen) || (only.is_none() && wang_desktop::Desktop::available().is_ok() && !wang_desktop::Desktop::new(wang_desktop::Desktop::default_dir()).screen_ready()) {
        screen(lines).await?;
    }
    // Pick up the new token / paired id / service file, and prove the phone path end to end.
    let verified = if only.is_none() || only == Some(Step::Phone) || only == Some(Step::Awake) {
        crate::phone::finish(phone.as_ref()).await?
    } else {
        false
    };

    std::fs::write(marker(), "")?;
    println!("\n── Done ──");
    if verified {
        println!("  From your phone: just message the bot. From here: `wang <what you want>`, or `wang` to chat.");
    } else if phone.as_ref().is_some_and(|p| !p.user_ids.is_empty()) && !crate::phone::service_enabled() {
        println!("  Your phone is paired, but the bot only answers while wang runs. Keep it running in the background: `wang setup awake`.");
        println!("  Meanwhile: `wang` to chat here, or `wang <what you want>`.");
    } else if api_key_set() {
        println!("  Run `wang` to chat, or `wang <what you want>`.");
    } else if claude_has_wang() {
        println!("  Open Claude Code and ask it to use wang.");
    }
    println!("  `wang doctor` shows what's on; `wang setup phone|awake|logins` redoes one part.");
    Ok(())
}

/// Claude Code hookup and the model API key.
async fn brains(cfg_path: &Path, lines: &mut Lines) -> Result<()> {
    // 1. Claude Code: the no-API-key way to use wang.
    if which("claude").is_some() && !claude_has_wang() {
        println!("\nClaude Code is installed. Plugging wang in gives Claude a sandboxed shell, browser and VMs, using your own Claude login (no API key).");
        if yes(&ask("Add wang to Claude Code? [Y/n] ", lines), true) {
            let exe = exe();
            let st = Command::new("claude").args(["mcp", "add", "--scope", "user", "wang", "--"]).arg(&exe).arg("mcp").status();
            match st {
                Ok(s) if s.success() => println!("  ✓ added: in Claude Code, ask e.g. \"use wang's VM to try this script\""),
                _ => println!("  couldn't add it; run: claude mcp add --scope user wang -- {} mcp", exe.display()),
            }
        }
    }

    // 2. The brain for wang's own agent (terminal, Telegram).
    println!("\n── Brain (what does the thinking) ──");
    let cfg = Config::load(cfg_path)?;
    let current = cfg.model.provider.clone();
    let claude = wang_daemon::brain::claude_signed_in();
    let codex_installed = which("codex").is_some();
    let codex = wang_daemon::brain::codex_signed_in();
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let label = |p: &str| match p {
        "claude-code" => "your Claude subscription (Claude Code)".to_string(),
        "codex" => "your ChatGPT subscription (Codex)".to_string(),
        "deepseek" => "DeepSeek API key".to_string(),
        "anthropic" => "Anthropic API key".to_string(),
        "openai" => "OpenAI-compatible API key".to_string(),
        _ => "OpenRouter API key".to_string(),
    };
    println!("  Now: {}.", label(&current));
    println!("  Options:");
    println!(
        "    1) Your Claude subscription (Pro/Max) {}",
        match &claude {
            Some(_) => "✓ signed in: no API key, uses your plan's limits",
            None if which("claude").is_some() => "(run `claude` once to sign in)",
            None => "(needs Claude Code installed)",
        }
    );
    println!(
        "    2) Your ChatGPT subscription (Plus/Pro) {}",
        match (&codex, codex_installed) {
            (Some(_), _) => "✓ signed in via Codex",
            (None, true) => "(Codex is installed; I'll run `codex login`)",
            (None, false) => "(needs Codex: npm i -g @openai/codex)",
        }
    );
    println!("    3) DeepSeek API key (very cheap pay-per-use: https://platform.deepseek.com/api_keys)");
    println!("    4) OpenRouter API key (any model, pay-per-use: https://openrouter.ai/keys){}", if env("OPENROUTER_API_KEY").is_some() { " ✓ key saved" } else { "" });
    println!("  Subscriptions think through their own app (in wang's jail, with only wang's tools); your phone approvals, sandbox and logins work the same.");
    let pick = ask("  Pick 1-4 (Enter keeps the current one): ", lines);
    let chosen = match pick.trim() {
        "1" => {
            if claude.is_none() {
                println!("  Sign in first: run `claude`, log in with your Claude account, then `wang setup` again.");
                return Ok(());
            }
            Some("claude-code")
        }
        "2" => {
            if !codex_installed {
                println!("  Install Codex first: npm i -g @openai/codex");
                return Ok(());
            }
            if codex.is_none() {
                println!("  Opening `codex login` (sign in with ChatGPT in the browser)...");
                let _ = Command::new("codex").arg("login").status();
                if wang_daemon::brain::codex_signed_in().is_none() {
                    println!("  Not signed in yet; run `codex login`, then `wang setup` again.");
                    return Ok(());
                }
            }
            Some("codex")
        }
        "3" => {
            if env("DEEPSEEK_API_KEY").is_none() {
                let k = ask("  Paste your DeepSeek key (sk-...): ", lines);
                if !k.starts_with("sk-") {
                    println!("  that doesn't look like a DeepSeek key; skipped");
                    return Ok(());
                }
                set_env("DEEPSEEK_API_KEY", &k)?;
                // SAFETY: single-threaded setup flow, before the daemon starts.
                unsafe { std::env::set_var("DEEPSEEK_API_KEY", &k) };
            }
            Some("deepseek")
        }
        "4" => {
            if env("OPENROUTER_API_KEY").is_none() && !openrouter_key(lines).await? {
                return Ok(());
            }
            Some("openrouter")
        }
        _ => None,
    };
    match chosen {
        Some(p) if p != current => {
            config::set_provider(cfg_path, p)?;
            println!("  ✓ brain: {} (saved in {}; switch any time with /brain on Telegram)", label(p), cfg_path.display());
        }
        Some(p) => println!("  ✓ keeping {}", label(p)),
        None if current == "openrouter" && env("OPENROUTER_API_KEY").is_some() => match openrouter_key_ok(env("OPENROUTER_API_KEY").unwrap().trim()).await {
            Some(true) => println!("  ✓ OpenRouter key works"),
            Some(false) => println!("  ✗ OpenRouter rejected the saved key; paste a new one in {} (OPENROUTER_API_KEY=...)", config::env_path().display()),
            None => println!("  – OpenRouter key set (couldn't reach openrouter.ai to check it)"),
        },
        None if !api_key_set() => println!("  – no brain yet: wang's own agent (Telegram, `wang`) is off until you pick one (`wang setup`)."),
        None => {}
    }
    Ok(())
}

/// Ask for an OpenRouter key, check it, save it. Returns whether one was saved.
async fn openrouter_key(lines: &mut Lines) -> Result<bool> {
    println!("  Your key stays in {} (hidden from wang's sandbox) and goes only to the provider.", config::env_path().display());
    for _ in 0..3 {
        let k = ask("  Paste an OpenRouter key (sk-or-...), or press Enter to skip: ", lines);
        if k.is_empty() {
            return Ok(false);
        }
        if !k.starts_with("sk-") {
            println!("  that doesn't look like a key (it starts with sk-or-)");
            continue;
        }
        if openrouter_key_ok(&k).await == Some(false) {
            println!("  OpenRouter says that key isn't valid");
            continue;
        }
        set_env("OPENROUTER_API_KEY", &k)?;
        // SAFETY: single-threaded setup flow, before the daemon starts.
        unsafe { std::env::set_var("OPENROUTER_API_KEY", &k) };
        println!("  ✓ saved");
        return Ok(true);
    }
    Ok(false)
}

/// Desktop control: accessibility for all apps, then GNOME's one-time screen permission.
async fn screen(lines: &mut Lines) -> Result<()> {
    println!("\n── Using your apps ──");
    if let Err(e) = wang_desktop::Desktop::available() {
        println!("  – {e:#}");
        return Ok(());
    }
    println!("  wang can open apps and read/click/type in their windows (through accessibility, like a screen reader).");
    println!("  In chat, mail, browser and settings apps, anything that acts as you asks you first, on your phone.");
    let a11y = Command::new("gsettings").args(["get", "org.gnome.desktop.interface", "toolkit-accessibility"]).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    if a11y.as_deref() == Some("false") {
        println!("  Chrome, Slack, Discord and other Electron apps only show their buttons to accessibility tools when it's switched on.");
        if yes(&ask("  Switch on accessibility support for those apps? (reopen them afterwards) [Y/n] ", lines), true) {
            let ok = Command::new("gsettings").args(["set", "org.gnome.desktop.interface", "toolkit-accessibility", "true"]).status().is_ok_and(|s| s.success());
            println!("{}", if ok { "  ✓ on (undo: gsettings set org.gnome.desktop.interface toolkit-accessibility false)" } else { "  couldn't switch it on" });
        }
    }
    let d = wang_desktop::Desktop::new(wang_desktop::Desktop::default_dir());
    if d.screen_ready() {
        println!("  ✓ screen control already allowed");
    } else {
        println!(
            "\n  Screenshots, clicks on the screen and key presses need GNOME's permission, once.\n\
             \x20 A dialog \"Allow remote interaction?\" will open: choose your screen, tick \"Remember\" if shown, and click Share / Allow.\n\
             \x20 (It's saved, so you're never asked while away. Undo: Settings → Privacy → Screen Sharing / Remote Desktop, or delete {}.)",
            wang_desktop::Desktop::default_dir().join("portal-restore-token").display()
        );
        if !yes(&ask("  Open the dialog now? [Y/n] ", lines), true) {
            println!("  – skipped (wang still uses apps through accessibility; `wang setup screen` any time)");
            return Ok(());
        }
        match d.grant_screen().await {
            Ok(v) => println!("  ✓ allowed (screen {})", v["screen"].as_array().map(|a| format!("{}x{}", a[0], a[1])).unwrap_or_else(|| "?".into())),
            Err(e) => {
                println!("  ✗ {e:#}");
                return Ok(());
            }
        }
    }
    match d.screenshot(1280).await {
        Ok(s) => {
            let p = std::env::temp_dir().join("wang-test-screenshot.jpg");
            std::fs::write(&p, &s.jpeg)?;
            println!("  ✓ test screenshot: {} ({} KB)", p.display(), s.jpeg.len() / 1024);
        }
        Err(e) => println!("  ✗ screenshot failed: {e:#}"),
    }
    d.shutdown().await;
    Ok(())
}

/// Sign wang's browser in to the user's sites (`wang login`).
async fn logins(cfg_path: &Path, lines: &mut Lines) -> Result<()> {
    println!("\n── Signed-in browsing ──");
    let cfg = Config::load(cfg_path)?;
    if !cfg.browser.enabled || wang_browser::find_chrome().is_err() {
        println!("  – the browser isn't set up (`wang doctor`), skipping");
        return Ok(());
    }
    if !matches!(cfg.browser.logins_cfg().map(|l| l.source), Some(wang_browser::logins::LoginSource::Jar(_))) {
        println!("  – [browser] logins isn't \"wang\" in the config, skipping");
        return Ok(());
    }
    let have = crate::phone::signed_in_sites(&cfg);
    if !have.is_empty() {
        let shown: Vec<&str> = have.iter().take(8).map(String::as_str).collect();
        println!("  ✓ wang's browser has logins for {} site(s): {}{}", have.len(), shown.join(", "), if have.len() > 8 { ", ..." } else { "" });
    }
    println!(
        "  wang's own browser can use sites as you (YouTube, Instagram, Gmail, ...). You sign in once in a window here;\n\
         \x20 it's a separate browser, not your Firefox. Reading and browsing are free; anything that acts as you\n\
         \x20 (send, post, buy, delete, settings) asks you first, on your phone."
    );
    if std::env::var("WAYLAND_DISPLAY").is_err() && std::env::var("DISPLAY").is_err() {
        println!("  – no desktop session in this terminal; run `wang login youtube.com ...` from a terminal on the desktop");
        return Ok(());
    }
    let prompt = if have.is_empty() { "  Sites to sign in to now (e.g. youtube.com instagram.com), or Enter to skip: " } else { "  More sites to sign in to (Enter to skip): " };
    let sites: Vec<String> = ask(prompt, lines).split([' ', ',']).filter(|s| !s.is_empty()).map(String::from).collect();
    if sites.is_empty() {
        return Ok(());
    }
    if let Err(e) = crate::login(&cfg, &sites).await {
        println!("  couldn't open the login window: {e:#}\n  (try again any time: wang login {})", sites.join(" "));
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

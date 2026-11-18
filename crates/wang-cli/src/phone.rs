//! The parts of `wang setup` that make wang usable from your phone, anywhere:
//!
//! - **phone**: create/check the Telegram bot, pair your phone with a one-time code (QR code or
//!   link) so your user id lands in the config by itself, and set the bot's command menu.
//! - **always on**: the systemd user service (enabled, started at boot without logging in),
//!   and a laptop that keeps running with the lid closed while plugged in and doesn't
//!   suspend when idle.
//! - **finish**: restart the service and prove it works by having it come online and sending a
//!   message to your phone.
//!
//! Everything outside wang's own files (sudo, GNOME settings, linger) is asked first, says
//! exactly what it changes, and says how to undo it.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::json;
use wang_daemon::config::{self, Config};
use wang_daemon::telegram::Bot;
use wang_daemon::telegram::pair::{self, Event};

use crate::setup::{Lines, ask, set_env, which, yes};

// ---------- Config editing ----------

/// Add `id` to `[telegram] allowed_user_ids`, keeping the file's comments and layout.
/// Returns the new text, or None if it was already there.
pub fn add_allowed_user(text: &str, id: i64) -> Result<Option<String>> {
    let mut doc: toml_edit::DocumentMut = text.parse().context("config.toml isn't valid TOML")?;
    if !doc.contains_key("telegram") {
        doc["telegram"] = toml_edit::table();
    }
    let tg = doc["telegram"].as_table_like_mut().context("[telegram] isn't a table")?;
    if tg.get("allowed_user_ids").is_none() {
        tg.insert("allowed_user_ids", toml_edit::value(toml_edit::Array::new()));
    }
    let arr = tg.get_mut("allowed_user_ids").and_then(|i| i.as_array_mut()).context("[telegram] allowed_user_ids isn't a list")?;
    if arr.iter().any(|v| v.as_integer() == Some(id)) {
        return Ok(None);
    }
    arr.push(id);
    Ok(Some(doc.to_string()))
}

/// Replace a file atomically (temp file + rename), keeping its permissions.
pub fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("toml.wang-tmp");
    std::fs::write(&tmp, text)?;
    if let Ok(m) = std::fs::metadata(path) {
        std::fs::set_permissions(&tmp, m.permissions())?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

// ---------- QR code ----------

/// A QR code drawn with half blocks (two modules per character), with a quiet zone.
/// `ansi`: force black-on-white, so it scans on dark terminals too.
pub fn qr(text: &str, ansi: bool) -> Result<String> {
    let code = qrcode::QrCode::new(text.as_bytes()).context("making the QR code")?;
    let w = code.width();
    let colors = code.to_colors();
    let quiet = 2i64;
    let dark = |x: i64, y: i64| -> bool {
        x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < w && colors[y as usize * w + x as usize] == qrcode::Color::Dark
    };
    let mut out = String::new();
    let mut y = -quiet;
    while y < w as i64 + quiet {
        if ansi {
            out.push_str("\x1b[38;5;16;48;5;231m");
        }
        for x in -quiet..w as i64 + quiet {
            out.push(match (dark(x, y), dark(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        if ansi {
            out.push_str("\x1b[0m");
        }
        out.push('\n');
        y += 2;
    }
    Ok(out)
}

// ---------- Telegram ----------

/// Find a bot token in what was pasted (BotFather's whole message is fine).
pub fn find_token(pasted: &str) -> Option<String> {
    pasted
        .split(|c: char| c.is_whitespace() || c == '`' || c == '"' || c == '\'')
        .find(|w| {
            let Some((id, secret)) = w.split_once(':') else { return false };
            (6..=14).contains(&id.len())
                && id.bytes().all(|b| b.is_ascii_digit())
                && secret.len() >= 30
                && secret.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
        .map(String::from)
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| "this computer".into())
}

/// The bot's @username, if the token works.
async fn bot_username(bot: &Bot) -> Result<String> {
    let me = bot.call("getMe", json!({})).await?;
    me["username"].as_str().map(String::from).context("getMe returned no username")
}

fn env_token() -> Option<String> {
    std::env::var("TELEGRAM_BOT_TOKEN").ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

pub struct Phone {
    pub bot_username: String,
    pub user_ids: Vec<i64>,
}

/// Get a working bot token: the saved one, or walk through @BotFather and save the new one.
async fn get_bot(api_base: &str, lines: &mut Lines) -> Result<Option<(Bot, String)>> {
    if let Some(t) = env_token() {
        let bot = Bot::new(api_base, &t)?;
        match bot_username(&bot).await {
            Ok(u) => {
                println!("  ✓ bot @{u} (token in {})", config::env_path().display());
                return Ok(Some((bot, u)));
            }
            Err(e) => println!("  the saved TELEGRAM_BOT_TOKEN doesn't work ({e:#}); let's set a new one."),
        }
    }
    println!(
        "  You need your own Telegram bot (free, ~1 minute). Your phone talks to it; wang on this laptop reads it.\n\
         \n\
         \x20   1. In Telegram, open @BotFather: https://t.me/BotFather\n\
         \x20   2. Send /newbot, pick a name (e.g. Wang) and a username ending in \"bot\"\n\
         \x20   3. BotFather replies with a token like 123456789:AAH4...; paste it (or its whole message) here\n"
    );
    for _ in 0..3 {
        let pasted = ask("  Token (Enter to skip): ", lines);
        if pasted.is_empty() {
            return Ok(None);
        }
        let Some(token) = find_token(&pasted) else {
            println!("  that doesn't look like a bot token (digits, a colon, then ~35 letters).");
            continue;
        };
        let bot = Bot::new(api_base, &token)?;
        match bot_username(&bot).await {
            Ok(u) => {
                set_env("TELEGRAM_BOT_TOKEN", &token)?;
                // SAFETY: setup runs these steps one at a time, before the daemon (re)starts.
                unsafe { std::env::set_var("TELEGRAM_BOT_TOKEN", &token) };
                println!("  ✓ bot @{u} works; token saved to {} (chmod 600, hidden from wang's sandbox)", config::env_path().display());
                return Ok(Some((bot, u)));
            }
            Err(e) => println!("  Telegram didn't accept it: {e:#}"),
        }
    }
    println!("  skipped; run `wang setup phone` to try again.");
    Ok(None)
}

/// Set when pairing stopped the service, so it's started again even if it isn't enabled.
pub static PAUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Stop the background service while pairing: only one program may read a bot's messages.
fn pause_service() -> bool {
    let active = Command::new("systemctl").args(["--user", "is-active", "--quiet", "wang"]).status().is_ok_and(|s| s.success());
    if active && env_token().is_some() {
        println!("  (pausing the background wang while pairing; it restarts at the end)");
        let ok = Command::new("systemctl").args(["--user", "stop", "wang"]).status().is_ok_and(|s| s.success());
        PAUSED.store(ok, std::sync::atomic::Ordering::Relaxed);
        return ok;
    }
    false
}

/// The phone step. Returns the bot and who's paired, or None if skipped.
pub async fn phone(cfg_path: &Path, lines: &mut Lines) -> Result<Option<Phone>> {
    println!("\n── Phone (Telegram) ──");
    println!("  Message wang from anywhere: \"find me a lo-fi playlist on YouTube\", \"what's eating my disk?\".");
    let cfg = Config::load(cfg_path)?;
    let Some((bot, username)) = get_bot(&cfg.telegram.api_base, lines).await? else { return Ok(None) };
    let mut ids = cfg.telegram.allowed_user_ids.clone();
    if !ids.is_empty() && !yes(&ask(&format!("  Already paired with user id {ids:?}. Pair another phone/account? [y/N] "), lines), false) {
        return Ok(Some(Phone { bot_username: username, user_ids: ids }));
    }
    pause_service();
    let code = pair::new_code()?;
    let link = pair::link(&username, &code);
    println!("\n  Pair your phone: scan this with your phone's camera, then tap Start in Telegram.\n");
    let color = std::io::IsTerminal::is_terminal(&std::io::stdout());
    for l in qr(&link, color)?.lines() {
        println!("    {l}");
    }
    println!("  No camera handy? Open {link}\n  or open @{username} and send:  {}", pair::pretty(&code));
    println!("  Waiting (10 min; Ctrl-C to stop)...");
    let paired = pair::pair(&bot, &code, Duration::from_secs(600), |e| match e {
        Event::Wrong { user_id, name } => println!("  · ignored a message from {name} (id {user_id}): no pairing code"),
        Event::NotPrivate => println!("  · ignored a group message: pair in a private chat with the bot"),
    })
    .await?;
    let Some(p) = paired else {
        println!("  timed out; run `wang setup phone` for a new code.");
        return Ok(None);
    };
    let text = std::fs::read_to_string(cfg_path)?;
    if let Some(new) = add_allowed_user(&text, p.user_id)? {
        write_atomic(cfg_path, &new)?;
    }
    ids.push(p.user_id);
    println!("  ✓ paired with {} (Telegram id {}); only this account can use the bot", p.name, p.user_id);
    let _ = bot.call("setMyCommands", pair::commands()).await;
    let _ = bot
        .send_text(
            p.user_id,
            &format!("👋 Paired with wang on {}. Only this Telegram account can use me.", hostname()),
            None,
        )
        .await;
    println!(
        "\n  Keep it yours:\n\
         \x20   • Anyone who gets into your Telegram account can drive this laptop. Turn on Telegram →\n\
         \x20     Settings → Privacy and Security → Two-Step Verification.\n\
         \x20   • Bot chats aren't end-to-end encrypted (Telegram's servers see them). Risky actions\n\
         \x20     still come to you as ✅/❌ buttons first."
    );
    Ok(Some(Phone { bot_username: username, user_ids: ids }))
}

// ---------- Always on ----------

/// What logind does when the lid closes.
#[derive(Debug, Clone, PartialEq)]
pub struct Lid {
    pub on_ac: String,
    pub on_battery: String,
}

impl Lid {
    pub fn keeps_running_on_ac(&self) -> bool {
        matches!(self.on_ac.as_str(), "ignore" | "lock")
    }
}

/// Apply logind.conf and its drop-ins in order (later wins), with systemd's defaults.
pub fn parse_logind(files: &[String]) -> Lid {
    let (mut lid, mut ext) = (None::<String>, None::<String>);
    for f in files {
        let mut in_login = false;
        for l in f.lines() {
            let l = l.trim();
            if l.starts_with('[') {
                in_login = l == "[Login]";
                continue;
            }
            if !in_login || l.starts_with('#') || l.starts_with(';') {
                continue;
            }
            if let Some((k, v)) = l.split_once('=') {
                let v = v.trim().to_ascii_lowercase();
                match k.trim() {
                    "HandleLidSwitch" => lid = Some(v),
                    "HandleLidSwitchExternalPower" => ext = Some(v),
                    _ => {}
                }
            }
        }
    }
    let on_battery = lid.unwrap_or_else(|| "suspend".into());
    // Unset, HandleLidSwitchExternalPower follows HandleLidSwitch.
    Lid { on_ac: ext.unwrap_or_else(|| on_battery.clone()), on_battery }
}

/// logind's config files in the order systemd reads them.
fn logind_files() -> Vec<String> {
    let mut out = vec![];
    let main = ["/etc/systemd/logind.conf", "/run/systemd/logind.conf", "/usr/local/lib/systemd/logind.conf", "/usr/lib/systemd/logind.conf"]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok());
    out.extend(main);
    // Drop-ins: by file name across directories; the same name in /etc beats /run beats /usr.
    let mut by_name = std::collections::BTreeMap::new();
    for d in ["/usr/lib/systemd/logind.conf.d", "/usr/local/lib/systemd/logind.conf.d", "/run/systemd/logind.conf.d", "/etc/systemd/logind.conf.d"] {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.ends_with(".conf") {
                by_name.insert(n, e.path());
            }
        }
    }
    out.extend(by_name.values().filter_map(|p| std::fs::read_to_string(p).ok()));
    out
}

/// A laptop: has a lid or a system battery (not a mouse's).
pub fn is_laptop() -> bool {
    if Path::new("/proc/acpi/button/lid").exists() {
        return true;
    }
    std::fs::read_dir("/sys/class/power_supply").into_iter().flatten().flatten().any(|e| {
        let read = |f: &str| std::fs::read_to_string(e.path().join(f)).unwrap_or_default().trim().to_string();
        read("type") == "Battery" && read("scope") != "Device"
    })
}

const LID_DROPIN: &str = "/etc/systemd/logind.conf.d/50-wang-lid.conf";
const LID_CONF: &str = "# Written by `wang setup`: keep running with the lid closed while plugged in, so wang stays\n\
# reachable from your phone. On battery the lid still suspends. Delete this file to undo.\n\
[Login]\n\
HandleLidSwitchExternalPower=ignore\n\
HandleLidSwitchDocked=ignore\n";

fn gsettings_power(key: &str) -> Option<String> {
    which("gsettings")?;
    let o = Command::new("gsettings").args(["get", "org.gnome.settings-daemon.plugins.power", key]).stderr(Stdio::null()).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().trim_matches('\'').to_string())
}

fn user() -> String {
    std::env::var("USER").unwrap_or_default()
}

pub fn linger() -> Option<bool> {
    let o = Command::new("loginctl").args(["show-user", &user(), "-p", "Linger", "--value"]).stderr(Stdio::null()).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim() == "yes")
}

fn systemctl_user(args: &[&str]) -> bool {
    Command::new("systemctl").arg("--user").args(args).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

pub fn service_enabled() -> bool {
    systemctl_user(&["is-enabled", "--quiet", "wang"])
}

/// What keeps wang reachable, for `wang doctor`: (all good, detail, fix).
pub fn always_on_status() -> (bool, String, Option<String>) {
    let mut bad = vec![];
    let mut fix = None;
    if !service_enabled() {
        bad.push("background service not enabled");
        fix = Some("wang setup awake".to_string());
    }
    if linger() == Some(false) {
        bad.push("stops when you log out");
        fix = Some("wang setup awake".into());
    }
    if is_laptop() && !parse_logind(&logind_files()).keeps_running_on_ac() {
        bad.push("closing the lid suspends it");
        fix = Some("wang setup awake".into());
    }
    if gsettings_power("sleep-inactive-ac-type").is_some_and(|v| v != "nothing") {
        bad.push("suspends when idle");
        fix = Some("wang setup awake".into());
    }
    if bad.is_empty() { (true, "runs at boot, survives logout, lid closed and idle (on power)".into(), None) } else { (false, bad.join(" · "), fix) }
}

/// The always-on step: service, linger, lid, idle suspend. Each change is asked.
pub fn always_on(lines: &mut Lines) -> Result<()> {
    println!("\n── Always on ──");
    println!("  So wang answers while you're away: running at boot, with the lid closed, and when idle.");
    if which("systemctl").is_none() {
        println!("  – no systemd here; start wang yourself with `wang run`.");
        return Ok(());
    }
    // 1. The service: written fresh each time (picks up a moved binary), enabled.
    let enabled = service_enabled();
    if !enabled {
        if yes(&ask("  Start wang in the background whenever this computer is on (systemd user service)? [Y/n] ", lines), true) {
            let (unit, _) = crate::write_service_unit()?;
            let ok = systemctl_user(&["daemon-reload"]) && systemctl_user(&["enable", "wang"]);
            println!("{}", if ok { format!("  ✓ enabled ({})", unit.display()) } else { "  couldn't enable it; try: systemctl --user enable wang".into() });
        } else {
            println!("  – skipped (bare `wang` still starts it when you use the terminal)");
        }
    } else {
        if crate::write_service_unit()?.1 {
            systemctl_user(&["daemon-reload"]);
        }
        println!("  ✓ background service enabled");
    }
    // 2. Linger: user services run from boot, not only while you're logged in.
    match linger() {
        Some(true) => println!("  ✓ runs from boot, even before you log in (linger)"),
        Some(false) => {
            println!("  After a reboot (power cut, update), your user services wait until you log in, so wang would be offline.");
            if yes(&ask("  Let wang start at boot without logging in (`loginctl enable-linger`)? [Y/n] ", lines), true) {
                let ok = Command::new("loginctl").args(["enable-linger", &user()]).status().is_ok_and(|s| s.success());
                println!("{}", if ok { "  ✓ done (undo: loginctl disable-linger)" } else { "  couldn't; try: sudo loginctl enable-linger $USER" });
            }
        }
        None => {}
    }
    // 3. The lid.
    if is_laptop() {
        let lid = parse_logind(&logind_files());
        if lid.keeps_running_on_ac() {
            println!("  ✓ keeps running with the lid closed while plugged in (on battery: {})", lid.on_battery);
        } else {
            println!(
                "  Closing the lid now {}s the laptop, even plugged in, so wang goes offline.\n\
                 \x20 Fix: keep running with the lid closed *while plugged in* (on battery it still {}s, so it won't cook in a bag).\n\
                 \x20 This writes {LID_DROPIN} (needs your password; delete the file to undo).",
                lid.on_ac, lid.on_battery
            );
            if yes(&ask("  Do it? [Y/n] ", lines), true) {
                match write_lid_dropin() {
                    Ok(true) => println!("  ✓ done: lid closed + plugged in = keeps running"),
                    Ok(false) => println!("  ✓ written; takes effect after a reboot"),
                    Err(e) => println!("  couldn't: {e:#}"),
                }
            }
        }
    }
    // 4. Idle suspend (GNOME).
    if let Some(v) = gsettings_power("sleep-inactive-ac-type") {
        if v == "nothing" {
            println!("  ✓ doesn't suspend when idle on power");
        } else {
            let mins = gsettings_power("sleep-inactive-ac-timeout").and_then(|t| t.parse::<u64>().ok()).map(|s| s / 60).unwrap_or(0);
            println!("  Plugged in and idle for {mins} min, GNOME {v}s the computer, so wang goes offline.");
            if yes(&ask("  Turn off automatic suspend while plugged in? (screen lock/blanking unchanged) [Y/n] ", lines), true) {
                let ok = Command::new("gsettings")
                    .args(["set", "org.gnome.settings-daemon.plugins.power", "sleep-inactive-ac-type", "nothing"])
                    .status()
                    .is_ok_and(|s| s.success());
                println!(
                    "{}",
                    if ok {
                        format!("  ✓ done (undo: Settings → Power → Automatic Suspend, or gsettings set org.gnome.settings-daemon.plugins.power sleep-inactive-ac-type {v})")
                    } else {
                        "  couldn't; Settings → Power → Automatic Suspend → off when plugged in".into()
                    }
                );
            }
        }
    }
    Ok(())
}

/// Write the lid drop-in with sudo, then reload logind. Ok(true) = in effect now.
fn write_lid_dropin() -> Result<bool> {
    let dir = Path::new(LID_DROPIN).parent().unwrap();
    let script = format!("mkdir -p '{}' && cat > '{LID_DROPIN}' && chmod 644 '{LID_DROPIN}'", dir.display());
    let mut child = Command::new("sudo").args(["sh", "-c", &script]).stdin(Stdio::piped()).spawn().context("running sudo")?;
    std::io::Write::write_all(&mut child.stdin.take().context("sudo stdin")?, LID_CONF.as_bytes())?;
    if !child.wait()?.success() {
        bail!("sudo failed (wrong password, or not an admin)");
    }
    // Reloading re-reads the config; it doesn't touch running sessions.
    let reloaded = Command::new("sudo").args(["systemctl", "reload", "systemd-logind"]).status().is_ok_and(|s| s.success());
    Ok(reloaded && parse_logind(&logind_files()).keeps_running_on_ac())
}

// ---------- Finish: prove it works ----------

/// Restart the service with the new settings, wait for it (and its bot) to come online, and send
/// a "ready" message to the paired phone. Returns whether the phone path is verified end to end.
pub async fn finish(phone: Option<&Phone>) -> Result<bool> {
    if !service_enabled() {
        if PAUSED.load(std::sync::atomic::Ordering::Relaxed) {
            systemctl_user(&["start", "wang"]);
        }
        return Ok(false);
    }
    println!("\n── Checking it all works ──");
    let since = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    if !systemctl_user(&["restart", "wang"]) {
        println!("  ✗ the service didn't start: journalctl --user -u wang -e");
        return Ok(false);
    }
    let sock = config::socket_path();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut up = false;
    while Instant::now() < deadline {
        if wang_daemon::server::Client::connect(&sock).await.is_ok() {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    if !up {
        println!("  ✗ wang didn't come up: journalctl --user -u wang -e");
        return Ok(false);
    }
    println!("  ✓ wang is running in the background");
    let Some(p) = phone.filter(|p| !p.user_ids.is_empty()) else { return Ok(false) };
    // The daemon logs this once its bot answers; then a message really reaches it.
    let online = format!("telegram bot @{} online", p.bot_username);
    let mut seen = false;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && !seen {
        let log = Command::new("journalctl").args(["--user", "-u", "wang", "--since", &since, "-o", "cat", "--no-pager"]).output();
        seen = log.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&online));
        if !seen {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    if !seen {
        println!("  ✗ wang started but its Telegram bot didn't come online: journalctl --user -u wang -e");
        return Ok(false);
    }
    let cfg = Config::load(&config::config_path()).unwrap_or_default();
    let bot = Bot::new(&cfg.telegram.api_base, &env_token().context("no bot token")?)?;
    let msg = format!(
        "✅ wang is ready on {}.\n\nJust write what you need, e.g.\n• what's using my disk space?\n• find me a lo-fi playlist on YouTube\n• summarize the newest PDF in my Downloads\n\nRisky actions come here as ✅/❌ buttons first. /help for commands.",
        hostname()
    );
    let mut sent = 0;
    for id in &p.user_ids {
        if bot.send_text(*id, &msg, None).await.is_ok() {
            sent += 1;
        }
    }
    if sent == 0 {
        println!("  ✗ couldn't message your phone (did you block the bot?)");
        return Ok(false);
    }
    println!("  ✓ @{} is listening; check your phone for the \"wang is ready\" message", p.bot_username);
    Ok(true)
}

/// Sites wang's browser has logins for (for display).
pub fn signed_in_sites(cfg: &Config) -> Vec<String> {
    let Some(l) = cfg.browser.logins_cfg() else { return vec![] };
    let mut hosts = l.load().map(|c| wang_browser::logins::hosts_of(&c)).unwrap_or_default();
    // Show sites, not every tracker subdomain: keep the registrable-looking part.
    for h in &mut hosts {
        let parts: Vec<&str> = h.trim_start_matches('.').split('.').collect();
        *h = parts[parts.len().saturating_sub(2)..].join(".");
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_adds_the_id_and_keeps_comments() {
        let cfg = "[model]\nmodel = \"x\"   # keep me\n\n[telegram]                               # bot token\nallowed_user_ids = []                    # your numeric id\nstream = true\n";
        let out = add_allowed_user(cfg, 42).unwrap().unwrap();
        assert!(out.contains("# keep me") && out.contains("# bot token") && out.contains("# your numeric id"), "{out}");
        let v: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(v["telegram"]["allowed_user_ids"].as_array().unwrap(), &vec![toml::Value::Integer(42)]);
        assert_eq!(v["telegram"]["stream"].as_bool(), Some(true));
        assert!(add_allowed_user(&out, 42).unwrap().is_none(), "already there");
        let two = add_allowed_user(&out, 7).unwrap().unwrap();
        let v: toml::Value = toml::from_str(&two).unwrap();
        assert_eq!(v["telegram"]["allowed_user_ids"].as_array().unwrap().len(), 2);
        // No [telegram] section at all.
        let v: toml::Value = toml::from_str(&add_allowed_user("[model]\nmodel = \"x\"\n", 5).unwrap().unwrap()).unwrap();
        assert_eq!(v["telegram"]["allowed_user_ids"][0].as_integer(), Some(5));
        assert!(add_allowed_user("[telegram\n", 5).is_err());
    }

    #[test]
    fn the_real_example_config_round_trips() {
        let out = add_allowed_user(config::EXAMPLE_CONFIG, 123456789).unwrap().unwrap();
        let c: Config = toml::from_str(&out).unwrap();
        assert_eq!(c.telegram.allowed_user_ids, vec![123456789]);
        assert_eq!(out.lines().count(), config::EXAMPLE_CONFIG.lines().count(), "only the one line changed");
    }

    #[test]
    fn tokens_are_found_in_pastes() {
        let t = "123456789:AAH4fakeFAKEfake_fake-FAKEfake12345";
        assert_eq!(find_token(t).as_deref(), Some(t));
        let botfather = format!("Done! Congratulations on your new bot.\nUse this token to access the HTTP API:\n{t}\nKeep your token secure");
        assert_eq!(find_token(&botfather).as_deref(), Some(t));
        assert_eq!(find_token(&format!("`{t}`")).as_deref(), Some(t));
        for bad in ["sk-or-v1-abc", "12:short", "abc:AAH4fakeFAKEfake_fake-FAKEfake12345", ""] {
            assert_eq!(find_token(bad), None, "{bad}");
        }
    }

    #[test]
    fn qr_codes_decode_shape() {
        let q = qr("https://t.me/wang_bot?start=ABCDE23456", false).unwrap();
        let rows: Vec<&str> = q.lines().collect();
        let w = rows[0].chars().count();
        assert!(rows.iter().all(|r| r.chars().count() == w), "square-ish block");
        assert!((29..=45).contains(&w), "a small version QR: {w}");
        assert!(rows[0].trim().is_empty(), "quiet zone");
        let ansi = qr("x", true).unwrap();
        assert!(ansi.contains("\x1b[38;5;16;48;5;231m"), "black on white");
    }

    #[test]
    fn logind_rules() {
        // Ubuntu default: nothing set → suspend everywhere.
        assert_eq!(parse_logind(&["[Login]\n#HandleLidSwitch=suspend\n".into()]), Lid { on_ac: "suspend".into(), on_battery: "suspend".into() });
        assert!(!parse_logind(&[]).keeps_running_on_ac());
        // Our drop-in.
        let l = parse_logind(&["[Login]\n".into(), LID_CONF.into()]);
        assert!(l.keeps_running_on_ac() && l.on_battery == "suspend");
        // ExternalPower unset follows HandleLidSwitch; later files win; other sections ignored.
        assert_eq!(parse_logind(&["[Login]\nHandleLidSwitch=ignore\n".into()]).on_ac, "ignore");
        assert_eq!(parse_logind(&["[Login]\nHandleLidSwitchExternalPower=ignore\n".into(), "[Login]\nHandleLidSwitchExternalPower=Suspend\n".into()]).on_ac, "suspend");
        assert_eq!(parse_logind(&["[Other]\nHandleLidSwitch=ignore\n".into()]).on_ac, "suspend");
        assert!(parse_logind(&["[Login]\nHandleLidSwitchExternalPower=lock\n".into()]).keeps_running_on_ac());
    }
}

#[cfg(test)]
mod print_qr {
    /// `cargo test -p wang-cli print_qr -- --ignored --nocapture` prints a QR to scan by hand.
    #[test]
    #[ignore]
    fn print() {
        print!("{}", super::qr("https://t.me/wang_bot?start=ABCDE23456", false).unwrap());
    }
}

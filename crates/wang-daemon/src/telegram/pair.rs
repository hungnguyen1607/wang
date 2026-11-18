//! Pairing your phone with wang's bot (`wang setup`), so you never dig your user id out of a log.
//!
//! The laptop shows a one-time code (as a t.me link / QR code that sends `/start <code>`). The
//! first private-chat message carrying that code is you: its sender id is what goes into
//! `[telegram] allowed_user_ids`. Someone who merely finds the bot can't pair, because they
//! don't have the code. The code is ~50 bits and pairing gives up after a few wrong guesses.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::Bot;

/// No 0/O, 1/I/L: easy to read off a screen and type on a phone.
const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
pub const CODE_LEN: usize = 10;
/// Messages without the code before pairing gives up (someone is guessing).
pub const MAX_WRONG: usize = 20;

/// A fresh one-time code from the OS's random source.
pub fn new_code() -> Result<String> {
    let mut buf = [0u8; CODE_LEN * 2];
    std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf)).context("reading /dev/urandom")?;
    // Rejection sampling keeps every character equally likely.
    let limit = 256 - 256 % ALPHABET.len();
    let mut out = String::new();
    let mut extra = buf.to_vec();
    while out.len() < CODE_LEN {
        if extra.is_empty() {
            extra = vec![0u8; CODE_LEN];
            std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut extra))?;
        }
        let b = extra.remove(0) as usize;
        if b < limit {
            out.push(ALPHABET[b % ALPHABET.len()] as char);
        }
    }
    Ok(out)
}

/// "ABCDE23456" → "ABCDE-23456" for display.
pub fn pretty(code: &str) -> String {
    let (a, b) = code.split_at(code.len() / 2);
    format!("{a}-{b}")
}

/// Does this message carry the code? Accepts `/start CODE` (the link), the bare code, and the
/// code typed by hand with dashes, spaces or lowercase.
pub fn has_code(text: &str, code: &str) -> bool {
    let norm = |s: &str| s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_uppercase();
    let want = norm(code);
    if want.is_empty() {
        return false;
    }
    let t = text.trim();
    let t = t.strip_prefix("/start").map(str::trim_start).unwrap_or(t);
    // A "/start@botname CODE" form, too.
    let t = if t.starts_with('@') { t.split_once(char::is_whitespace).map(|(_, r)| r).unwrap_or("") } else { t };
    norm(t) == want
}

/// The deep link that opens the bot and sends `/start CODE` with one tap.
pub fn link(bot_username: &str, code: &str) -> String {
    format!("https://t.me/{bot_username}?start={code}")
}

#[derive(Debug, Clone, PartialEq)]
pub struct Paired {
    pub user_id: i64,
    /// For showing who paired: "@name" or the first name.
    pub name: String,
}

/// What happened while waiting, for the terminal.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A message without the code (not you, or a typo).
    Wrong { user_id: i64, name: String },
    /// A message in a group: wang only pairs in a private chat.
    NotPrivate,
}

fn who(from: &Value) -> String {
    match from["username"].as_str() {
        Some(u) => format!("@{u}"),
        None => from["first_name"].as_str().unwrap_or("someone").to_string(),
    }
}

/// Skip everything already queued, so only messages sent after the code was shown count.
async fn skip_old(bot: &Bot) -> Result<i64> {
    let last = bot.call("getUpdates", json!({"offset": -1, "timeout": 0})).await.map_err(explain)?;
    Ok(last.as_array().and_then(|a| a.last()).and_then(|u| u["update_id"].as_i64()).map(|id| id + 1).unwrap_or(0))
}

fn explain(e: anyhow::Error) -> anyhow::Error {
    let s = format!("{e:#}");
    if s.contains("Conflict") {
        e.context("another program is reading this bot's messages (a running wang daemon?). Stop it first: systemctl --user stop wang")
    } else if s.contains("Unauthorized") || s.contains("Not Found") {
        e.context("Telegram rejected the bot token; copy it again from @BotFather")
    } else {
        e
    }
}

/// Wait up to `timeout` for the code. Returns None on timeout; errors after [`MAX_WRONG`]
/// messages without it. The pairing message is acknowledged, so the daemon won't see it again.
pub async fn pair(bot: &Bot, code: &str, timeout: Duration, mut on_event: impl FnMut(Event)) -> Result<Option<Paired>> {
    let deadline = Instant::now() + timeout;
    let mut offset = skip_old(bot).await?;
    let mut wrong = 0;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        let wait = left.as_secs().clamp(1, 25);
        let updates = bot
            .call("getUpdates", json!({"offset": offset, "timeout": wait, "allowed_updates": ["message"]}))
            .await
            .map_err(explain)?;
        for u in updates.as_array().into_iter().flatten() {
            offset = offset.max(u["update_id"].as_i64().unwrap_or(0) + 1);
            let Some(m) = u.get("message") else { continue };
            let from = &m["from"];
            let user_id = from["id"].as_i64().unwrap_or(0);
            if m["chat"]["type"] != "private" {
                on_event(Event::NotPrivate);
                continue;
            }
            if user_id != 0 && from["is_bot"] != true && m["text"].as_str().is_some_and(|t| has_code(t, code)) {
                // Acknowledge it (and anything before it).
                let _ = bot.call("getUpdates", json!({"offset": offset, "timeout": 0})).await;
                return Ok(Some(Paired { user_id, name: who(from) }));
            }
            wrong += 1;
            on_event(Event::Wrong { user_id, name: who(from) });
            if wrong >= MAX_WRONG {
                let _ = bot.call("getUpdates", json!({"offset": offset, "timeout": 0})).await;
                bail!("{MAX_WRONG} messages without the code: stopped pairing in case someone is guessing. Run `wang setup phone` again for a new code.");
            }
        }
    }
}

/// The command menu Telegram shows next to the message box.
pub fn commands() -> Value {
    json!({"commands": [
        {"command": "new", "description": "Start a fresh conversation"},
        {"command": "brief", "description": "Your day: calendar + important mail"},
        {"command": "watches", "description": "What wang checks on by itself"},
        {"command": "quiet", "description": "Pause pings (/quiet 2h, /quiet off)"},
        {"command": "screen", "description": "See the laptop's screen"},
        {"command": "brain", "description": "Claude / ChatGPT subscription or API key"},
        {"command": "cost", "description": "Tokens and $ (today | week | month)"},
        {"command": "stats", "description": "Agent health, last 7 days"},
        {"command": "trace", "description": "Step-by-step timeline of the last task"},
        {"command": "schedules", "description": "Tasks wang runs on a schedule"},
        {"command": "plan", "description": "Split a bigger task into steps"},
        {"command": "help", "description": "What wang can do"},
    ]})
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn codes_are_random_readable_and_the_right_length() {
        let a = new_code().unwrap();
        let b = new_code().unwrap();
        assert_eq!(a.len(), CODE_LEN);
        assert_ne!(a, b);
        assert!(a.bytes().all(|c| ALPHABET.contains(&c)), "{a}");
        // Telegram's deep-link start parameter: 1-64 chars of A-Z a-z 0-9 _ -
        assert!(link("wang_bot", &a).ends_with(&format!("?start={a}")));
        assert_eq!(pretty("ABCDE23456"), "ABCDE-23456");
    }

    #[test]
    fn code_matching() {
        let c = "ABCDE23456";
        for ok in ["/start ABCDE23456", "ABCDE23456", " abcde-23456 ", "ABCDE 23456", "/start@wang_bot ABCDE23456"] {
            assert!(has_code(ok, c), "{ok}");
        }
        for bad in ["/start", "hi", "ABCDE2345", "ABCDE234567", "/start ABCDE23457", "my code is ABCDE23456", ""] {
            assert!(!has_code(bad, c), "{bad}");
        }
        assert!(!has_code("", ""));
    }
}

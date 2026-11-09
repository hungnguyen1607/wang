// ARCHIVED: the M1 Telegram channel from the first plan. Not built. The current plan reaches the
// phone through the PWA (M5.5); this can be ported to the `Channel` trait in server.rs if wanted.

//! Telegram front end: long-polls getUpdates, one conversation per chat, allow-listed users only.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::json;

use crate::agent::{Agent, Conversation};
use crate::model::ModelClient;

#[derive(Deserialize)]
struct TgResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct Update {
    update_id: i64,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    chat: Chat,
    from: Option<User>,
    text: Option<String>,
}

#[derive(Deserialize)]
struct Chat {
    id: i64,
}

#[derive(Deserialize)]
struct User {
    id: i64,
    username: Option<String>,
}

pub struct Bot {
    http: reqwest::Client,
    base: String,
}

impl Bot {
    pub fn new(token: &str) -> Result<Self> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(75)).build()?;
        Ok(Self { http, base: format!("https://api.telegram.org/bot{token}") })
    }

    async fn call<T: for<'de> Deserialize<'de>>(&self, method: &str, body: serde_json::Value) -> Result<T> {
        let r: TgResponse<T> = self
            .http
            .post(format!("{}/{method}", self.base))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("telegram {method}"))?
            .json()
            .await
            .with_context(|| format!("decoding telegram {method}"))?;
        if !r.ok {
            bail!("telegram {method}: {}", r.description.unwrap_or_default());
        }
        r.result.context("telegram: empty result")
    }

    pub async fn send(&self, chat_id: i64, text: &str) -> Result<()> {
        for chunk in split_message(text, 4000) {
            self.call::<serde_json::Value>("sendMessage", json!({"chat_id": chat_id, "text": chunk})).await?;
        }
        Ok(())
    }

    async fn typing(&self, chat_id: i64) {
        let _ = self.call::<serde_json::Value>("sendChatAction", json!({"chat_id": chat_id, "action": "typing"})).await;
    }
}

/// Split on line boundaries where possible, never inside a UTF-8 char.
pub fn split_message(text: &str, max_chars: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in text.split_inclusive('\n') {
        if cur.chars().count() + line.chars().count() > max_chars && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        if line.chars().count() > max_chars {
            let chars: Vec<char> = line.chars().collect();
            for c in chars.chunks(max_chars) {
                out.push(c.iter().collect());
            }
        } else {
            cur.push_str(line);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::from("(empty)"));
    }
    out
}

pub async fn run<C: ModelClient>(
    bot: Bot,
    agent: Agent<C>,
    allowed: Vec<i64>,
    cost_footer: bool,
) -> Result<()> {
    let me: serde_json::Value = bot.call("getMe", json!({})).await.context("checking bot token")?;
    tracing::info!("telegram bot @{} online", me["username"].as_str().unwrap_or("?"));
    if allowed.is_empty() {
        tracing::warn!("telegram.allowed_user_ids is empty: every message will be ignored. Message the bot once and copy your id from the log.");
    }

    let mut convs: HashMap<i64, Conversation> = HashMap::new();
    let mut offset = 0i64;
    loop {
        let updates: Vec<Update> = match bot
            .call("getUpdates", json!({"offset": offset, "timeout": 50, "allowed_updates": ["message"]}))
            .await
        {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!("getUpdates failed: {e:#}; retrying in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        for upd in updates {
            offset = upd.update_id + 1;
            let Some(msg) = upd.message else { continue };
            let Some(text) = msg.text else { continue };
            let Some(from) = msg.from else { continue };
            if !allowed.contains(&from.id) {
                tracing::warn!(
                    "ignored message from unknown user id {} (@{}); add it to telegram.allowed_user_ids to allow",
                    from.id,
                    from.username.as_deref().unwrap_or("-")
                );
                continue;
            }
            let chat_id = msg.chat.id;

            match text.trim() {
                "/start" | "/help" => {
                    let _ = bot
                        .send(chat_id, "wang is online.\n/new – start a fresh conversation\n/cost – today's tokens and $")
                        .await;
                    continue;
                }
                "/new" => {
                    convs.remove(&chat_id);
                    let _ = bot.send(chat_id, "Started a new conversation.").await;
                    continue;
                }
                "/cost" => {
                    let s = agent.ledger.summary(1, "today").map(|s| s.short()).unwrap_or_else(|e| format!("error: {e}"));
                    let _ = bot.send(chat_id, &s).await;
                    continue;
                }
                _ => {}
            }

            let conv = convs.entry(chat_id).or_default();
            bot.typing(chat_id).await;
            let turn = agent.run_turn(conv, "telegram", &text);
            tokio::pin!(turn);
            let mut tick = tokio::time::interval(Duration::from_secs(4));
            tick.tick().await;
            let result = loop {
                tokio::select! {
                    r = &mut turn => break r,
                    _ = tick.tick() => bot.typing(chat_id).await,
                }
            };
            let reply = match result {
                Ok(r) if cost_footer => format!("{}\n\n{}", r.reply, r.footer()),
                Ok(r) => r.reply,
                Err(e) => {
                    tracing::error!("turn failed: {e:#}");
                    format!("Error: {e:#}")
                }
            };
            if let Err(e) = bot.send(chat_id, &reply).await {
                tracing::error!("sending reply failed: {e:#}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split() {
        assert_eq!(split_message("hi", 10), vec!["hi"]);
        let parts = split_message("aaaa\nbbbb\ncccc\n", 10);
        assert_eq!(parts, vec!["aaaa\nbbbb\n", "cccc\n"]);
        let long = "é".repeat(25);
        let parts = split_message(&long, 10);
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.chars().count() <= 10));
    }
}

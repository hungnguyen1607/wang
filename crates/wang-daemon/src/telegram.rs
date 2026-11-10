//! Telegram: chat with wang from your phone, plus alerts and Approve / Deny buttons.
//!
//! - Long-polls `getUpdates`, so nothing listens on the network.
//! - Only `[telegram] allowed_user_ids` are served, only in private chats. Everyone else is
//!   ignored without a reply, and logged once with their id (that's how you find your own).
//! - One conversation per chat (`tg-<chat id>`). Each message runs as its own task, so the
//!   poll loop keeps reading button taps while a turn waits for your approval.
//! - Replies stream by editing one message in place (~1 edit/s, under Telegram's limits),
//!   rolling over to a new message before the 4096-char cap.
//! - As a [`Notify`] target it sends alerts, and approval requests from any session (also
//!   `wang chat`) with inline buttons.
//!
//! The token is part of every API URL, so errors are stripped of URLs before logging.
//! Plain text only (no parse_mode): model output never needs escaping.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::config::TelegramCfg;
use crate::model::ModelClient;
use crate::notify::{Notice, Notify};
use crate::protocol::ServerMsg;
use crate::server::{Channel, Hub};

/// Telegram's limit is 4096; keep room for a status line.
const MAX_CHARS: usize = 3900;
const EDIT_EVERY: Duration = Duration::from_millis(1200);

pub struct Bot {
    http: reqwest::Client,
    base: String,
}

impl Bot {
    pub fn new(api_base: &str, token: &str) -> Result<Self> {
        if token.trim().is_empty() {
            bail!("TELEGRAM_BOT_TOKEN is empty");
        }
        // Longer than getUpdates' 50 s long poll.
        let http = reqwest::Client::builder().timeout(Duration::from_secs(75)).connect_timeout(Duration::from_secs(15)).build()?;
        Ok(Self { http, base: format!("{}/bot{}", api_base.trim_end_matches('/'), token.trim()) })
    }

    /// Call a Bot API method. Retries on 429 (flood control) using Telegram's retry_after.
    pub async fn call(&self, method: &str, body: Value) -> Result<Value> {
        for attempt in 0..3 {
            let resp = self
                .http
                .post(format!("{}/{method}", self.base))
                .json(&body)
                .send()
                .await
                .map_err(|e| anyhow!("telegram {method}: {}", e.without_url()))?;
            let v: Value = resp.json().await.map_err(|e| anyhow!("telegram {method}: bad response: {}", e.without_url()))?;
            if v["ok"] == true {
                return Ok(v["result"].clone());
            }
            if v["error_code"] == 429 && attempt < 2 {
                let wait = v["parameters"]["retry_after"].as_u64().unwrap_or(1).min(30);
                tracing::warn!("telegram {method}: rate limited, waiting {wait}s");
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            bail!("telegram {method}: {}", v["description"].as_str().unwrap_or("unknown error"));
        }
        bail!("telegram {method}: still rate limited")
    }

    /// Returns the new message's id.
    pub async fn send_text(&self, chat: i64, text: &str, markup: Option<Value>) -> Result<i64> {
        let mut body = json!({"chat_id": chat, "text": text, "link_preview_options": {"is_disabled": true}});
        if let Some(m) = markup {
            body["reply_markup"] = m;
        }
        let r = self.call("sendMessage", body).await?;
        r["message_id"].as_i64().context("telegram sendMessage: no message_id")
    }

    /// Replace a message's text (no markup = buttons removed). Unchanged text is not an error.
    pub async fn edit_text(&self, chat: i64, msg: i64, text: &str) -> Result<()> {
        match self
            .call("editMessageText", json!({"chat_id": chat, "message_id": msg, "text": text, "link_preview_options": {"is_disabled": true}}))
            .await
        {
            Err(e) if e.to_string().contains("message is not modified") => Ok(()),
            r => r.map(|_| ()),
        }
    }

    pub async fn send_long(&self, chat: i64, text: &str) -> Result<()> {
        for part in split_message(text, MAX_CHARS) {
            self.send_text(chat, &part, None).await?;
        }
        Ok(())
    }
}

/// Split on line boundaries where possible, never inside a UTF-8 char.
pub fn split_message(text: &str, max_chars: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0;
    for line in text.split_inclusive('\n') {
        let n = line.chars().count();
        if cur_len + n > max_chars && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            cur_len = 0;
        }
        if n > max_chars {
            let chars: Vec<char> = line.chars().collect();
            for c in chars.chunks(max_chars) {
                out.push(c.iter().collect());
            }
        } else {
            cur.push_str(line);
            cur_len += n;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push("(empty)".into());
    }
    out
}

pub fn approval_markup(id: &str) -> Value {
    json!({"inline_keyboard": [[
        {"text": "✅ Approve", "callback_data": format!("ap:y:{id}")},
        {"text": "❌ Deny", "callback_data": format!("ap:n:{id}")}
    ]]})
}

/// "ap:y:<id>" → (id, allow)
pub fn parse_callback(data: &str) -> Option<(&str, bool)> {
    let rest = data.strip_prefix("ap:")?;
    let (flag, id) = rest.split_once(':')?;
    let allow = match flag {
        "y" => true,
        "n" => false,
        _ => return None,
    };
    (!id.is_empty()).then_some((id, allow))
}

/// Alerts and approval requests to every allowed user (a private chat id is the user id).
pub struct Telegram {
    pub bot: Arc<Bot>,
    pub users: Vec<i64>,
}

impl Notify for Telegram {
    fn send<'a>(&'a self, n: &'a Notice) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            if self.users.is_empty() {
                bail!("telegram: no allowed_user_ids to notify");
            }
            let text = format!("{}\n\n{}", n.title, n.message);
            let mut last_err = None;
            let mut sent = 0;
            for &u in &self.users {
                match self.bot.send_text(u, &text, n.approval_id.as_deref().map(approval_markup)).await {
                    Ok(_) => sent += 1,
                    Err(e) => last_err = Some(e),
                }
            }
            match (sent, last_err) {
                (0, Some(e)) => Err(e),
                _ => Ok(()),
            }
        })
    }
}

pub struct TelegramChannel {
    pub bot: Arc<Bot>,
    pub allowed: Vec<i64>,
    pub stream: bool,
    /// The notifier already sends approval buttons; don't send them twice.
    pub buttons_via_notifier: bool,
    last_task: Arc<StdMutex<HashMap<i64, i64>>>,
    warned: StdMutex<HashSet<i64>>,
}

impl TelegramChannel {
    pub fn new(bot: Arc<Bot>, cfg: &TelegramCfg, buttons_via_notifier: bool) -> Self {
        Self {
            bot,
            allowed: cfg.allowed_user_ids.clone(),
            stream: cfg.stream,
            buttons_via_notifier,
            last_task: Default::default(),
            warned: Default::default(),
        }
    }
}

const HELP: &str = "wang is online. Just write what you need.\n\n\
/new – start a fresh conversation\n\
/cost [today|week|month] – tokens and $\n\
/stats – agent health (last 7 days)\n\
/trace [id] – step-by-step timeline of the last task\n\
/good, /bad – rate the last task\n\n\
wang picks a workflow for each message; force one with\n\
/research, /code (in a throwaway VM), /quick or /general before your message,\n\
or /plan to split a bigger task into steps run by separate worker agents.\n\n\
/schedules – tasks wang does on a schedule (ask: \"every morning at 8 …\")\n\
/unschedule <n> – stop one";

impl<C: ModelClient> Channel<C> for TelegramChannel {
    fn name(&self) -> &'static str {
        "telegram"
    }

    async fn serve(self, hub: Arc<Hub<C>>) -> Result<()> {
        let me = self.bot.call("getMe", json!({})).await.context("checking TELEGRAM_BOT_TOKEN")?;
        tracing::info!("telegram bot @{} online", me["username"].as_str().unwrap_or("?"));
        if self.allowed.is_empty() {
            tracing::warn!("telegram: allowed_user_ids is empty, so every message is ignored. Message the bot once and copy your id from this log.");
        }
        let mut offset = 0i64;
        let mut backoff = 1u64;
        loop {
            let updates = match self
                .bot
                .call("getUpdates", json!({"offset": offset, "timeout": 50, "allowed_updates": ["message", "callback_query"]}))
                .await
            {
                Ok(u) => {
                    backoff = 1;
                    u
                }
                Err(e) => {
                    tracing::warn!("telegram getUpdates failed: {e:#}; retrying in {backoff}s");
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(60);
                    continue;
                }
            };
            for u in updates.as_array().into_iter().flatten() {
                offset = offset.max(u["update_id"].as_i64().unwrap_or(0) + 1);
                if let Some(cb) = u.get("callback_query") {
                    self.on_callback(&hub, cb).await;
                } else if let Some(m) = u.get("message") {
                    self.on_message(&hub, m).await;
                }
            }
        }
    }
}

impl TelegramChannel {
    fn allowed(&self, from: &Value, what: &str) -> bool {
        let id = from["id"].as_i64().unwrap_or(0);
        if id != 0 && self.allowed.contains(&id) {
            return true;
        }
        if self.warned.lock().unwrap().insert(id) {
            tracing::warn!(
                "telegram: ignored {what} from user id {id} (@{}). If that's you, add {id} to [telegram] allowed_user_ids.",
                from["username"].as_str().unwrap_or("-")
            );
        }
        false
    }

    async fn reply(&self, chat: i64, text: &str) {
        if let Err(e) = self.bot.send_long(chat, text).await {
            tracing::warn!("telegram reply failed: {e:#}");
        }
    }

    async fn on_message<C: ModelClient>(&self, hub: &Arc<Hub<C>>, m: &Value) {
        if !self.allowed(&m["from"], "a message") {
            return;
        }
        let chat = m["chat"]["id"].as_i64().unwrap_or(0);
        if m["chat"]["type"] != "private" {
            tracing::warn!("telegram: ignored a message in non-private chat {chat}; wang only answers in a private chat");
            return;
        }
        let Some(text) = m["text"].as_str().map(str::trim).filter(|t| !t.is_empty()) else {
            self.reply(chat, "I only read text messages for now.").await;
            return;
        };
        let session = format!("tg-{chat}");
        let (cmd, arg) = match text.strip_prefix('/') {
            Some(c) => {
                let (c, a) = c.split_once(char::is_whitespace).unwrap_or((c, ""));
                (Some(c.split('@').next().unwrap_or(c).to_ascii_lowercase()), a.trim())
            }
            None => (None, ""),
        };
        let a = &hub.agent;
        let last = self.last_task.lock().unwrap().get(&chat).copied();
        match cmd.as_deref() {
            Some("start" | "help") => self.reply(chat, HELP).await,
            Some("new") => {
                hub.reset(&session).await;
                self.reply(chat, "Started a new conversation.").await;
            }
            Some("cost") => {
                let (days, label) = match arg {
                    "week" => (7, "last 7 days"),
                    "month" => (30, "last 30 days"),
                    _ => (1, "today"),
                };
                let s = a.ledger.summary(days, label).map(|s| s.to_string()).unwrap_or_else(|e| format!("error: {e:#}"));
                self.reply(chat, &s).await;
            }
            Some("stats") => {
                let s = a.ledger.stats(Some(7), "last 7 days").map(|s| s.to_string()).unwrap_or_else(|e| format!("error: {e:#}"));
                self.reply(chat, &s).await;
            }
            Some("trace") => {
                let id = arg.parse::<i64>().ok().or(last).or_else(|| a.tracer.latest_task().ok().flatten());
                let s = match id {
                    Some(id) => a.tracer.spans(id).map(|s| wang_trace::render(&s)).unwrap_or_else(|e| format!("error: {e:#}")),
                    None => "No tasks traced yet.".into(),
                };
                // A long trace becomes many messages; keep the first few.
                let parts = split_message(&s, MAX_CHARS);
                for p in parts.iter().take(3) {
                    self.reply(chat, p).await;
                }
                if parts.len() > 3 {
                    self.reply(chat, &format!("(trace cut; full version: wang trace {})", id.unwrap_or(0))).await;
                }
            }
            Some(v @ ("good" | "bad")) => {
                let good = v == "good";
                let s = match last {
                    Some(id) => match a.ledger.set_feedback(id, good) {
                        Ok(true) => {
                            if let Ok(Some(root)) = a.tracer.task_root(id) {
                                let _ = a.tracer.annotate(root, json!({"wang.feedback": v}));
                            }
                            format!("Task #{id} rated {}", if good { "👍" } else { "👎" })
                        }
                        Ok(false) => format!("No task #{id}"),
                        Err(e) => format!("error: {e:#}"),
                    },
                    None => "No task in this chat yet.".into(),
                };
                self.reply(chat, &s).await;
            }
            Some("schedules") => {
                let s = match &a.ctx.schedules {
                    Some(st) => st.list(Some(&session)).map(|l| crate::schedule::render(&l)).unwrap_or_else(|e| format!("error: {e:#}")),
                    None => "Scheduling is off.".into(),
                };
                self.reply(chat, &format!("{s}\nAdd one by asking, e.g. \"every weekday at 8am send me the top 5 Hacker News posts\". Stop one with /unschedule <number>.")).await;
            }
            Some("unschedule") => {
                let s = match (&a.ctx.schedules, arg.trim_start_matches('#').parse::<i64>()) {
                    (Some(st), Ok(id)) => match st.remove(id, Some(&session)) {
                        Ok(true) => format!("Stopped schedule #{id}."),
                        Ok(false) => format!("No schedule #{id} in this chat."),
                        Err(e) => format!("error: {e:#}"),
                    },
                    (None, _) => "Scheduling is off.".into(),
                    (_, Err(_)) => "Usage: /unschedule <number> (see /schedules)".into(),
                };
                self.reply(chat, &s).await;
            }
            Some(other) if crate::workflow::Workflow::from_name(other).is_none() => {
                self.reply(chat, &format!("Unknown command /{other}.\n\n{HELP}")).await
            }
            // /research /code /quick /general: a message with that workflow (the agent reads the prefix).
            Some(w) if arg.is_empty() => self.reply(chat, &format!("Usage: /{w} <what you want done>")).await,
            Some(_) | None => {
                let text = match cmd.as_deref() {
                    Some(w) => format!("/{w} {arg}"),
                    None => text.to_string(),
                };
                let (hub, bot, last_task) = (hub.clone(), self.bot.clone(), self.last_task.clone());
                let (stream, own_buttons) = (self.stream, !self.buttons_via_notifier);
                tokio::spawn(async move {
                    run_turn(hub, bot, chat, session, text, stream, own_buttons, last_task).await;
                });
            }
        }
    }

    async fn on_callback<C: ModelClient>(&self, hub: &Arc<Hub<C>>, cb: &Value) {
        let cb_id = cb["id"].as_str().unwrap_or_default().to_string();
        let answer = |text: &'static str| {
            let bot = self.bot.clone();
            let cb_id = cb_id.clone();
            async move {
                let _ = bot.call("answerCallbackQuery", json!({"callback_query_id": cb_id, "text": text})).await;
            }
        };
        if !self.allowed(&cb["from"], "a button tap") {
            answer("Not allowed.").await;
            return;
        }
        let Some((id, allow)) = cb["data"].as_str().and_then(parse_callback) else {
            answer("Unknown button.").await;
            return;
        };
        let ok = hub.agent.approvals.as_ref().is_some_and(|a| a.resolve(id, allow));
        let (toast, mark) = match (ok, allow) {
            (true, true) => ("Approved", "✅ Approved"),
            (true, false) => ("Denied", "❌ Denied"),
            (false, _) => ("Already answered or expired.", "⌛ Already answered or expired"),
        };
        if ok {
            tracing::info!("approval {id} {} from telegram", if allow { "granted" } else { "denied" });
        }
        answer(toast).await;
        let msg = &cb["message"];
        if let (Some(chat), Some(mid)) = (msg["chat"]["id"].as_i64(), msg["message_id"].as_i64()) {
            let original = msg["text"].as_str().unwrap_or("");
            let _ = self.bot.edit_text(chat, mid, &format!("{original}\n\n{mark}")).await;
        }
    }
}

/// One user message → one agent turn, rendered live into the chat.
#[allow(clippy::too_many_arguments)]
async fn run_turn<C: ModelClient>(
    hub: Arc<Hub<C>>,
    bot: Arc<Bot>,
    chat: i64,
    session: String,
    text: String,
    stream: bool,
    own_buttons: bool,
    last_task: Arc<StdMutex<HashMap<i64, i64>>>,
) {
    let _ = bot.call("sendChatAction", json!({"chat_id": chat, "action": "typing"})).await;
    let conv = hub.session(&session).await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ServerMsg>();
    let hub2 = hub.clone();
    let turn = async move {
        let mut conv = conv.lock().await;
        let emit = |ev: ServerMsg| {
            let _ = tx.send(ev);
        };
        hub2.agent.run_turn(&mut conv, "telegram", &text, &emit).await
    };
    let mut live = Live { bot, chat, stream, own_buttons, msg: None, text: String::new(), status: None, shown: String::new() };
    let render = async {
        let mut tick = tokio::time::interval(EDIT_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                ev = rx.recv() => match ev {
                    Some(ev) => live.on_event(ev).await,
                    None => break,
                },
                _ = tick.tick() => live.flush().await,
            }
        }
    };
    let (result, ()) = tokio::join!(turn, render);
    match result {
        Ok(r) => {
            last_task.lock().unwrap().insert(chat, r.task_id);
            live.finish(Some(r.footer())).await;
        }
        Err(e) => {
            tracing::error!("telegram task failed: {e:#}");
            live.text = format!("{}\n\nError: {e:#}", live.text.trim()).trim().to_string();
            live.finish(None).await;
        }
    }
}

struct Live {
    bot: Arc<Bot>,
    chat: i64,
    stream: bool,
    own_buttons: bool,
    /// Message being edited, if any.
    msg: Option<i64>,
    text: String,
    status: Option<String>,
    shown: String,
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

impl Live {
    async fn on_event(&mut self, ev: ServerMsg) {
        match ev {
            ServerMsg::Text { delta } => self.text.push_str(&delta),
            ServerMsg::ToolStart { name, detail } => self.status = Some(format!("⚙ {name}: {}", clip(&detail, 200))),
            ServerMsg::ToolEnd { .. } => self.status = None,
            ServerMsg::ApprovalRequest { id, tool, detail } => {
                self.status = Some(format!("⏳ waiting for your approval: {tool}"));
                self.flush().await;
                if self.own_buttons {
                    let text = format!("wang wants to run {tool}:\n{}", clip(&detail, 1500));
                    if let Err(e) = self.bot.send_text(self.chat, &text, Some(approval_markup(&id))).await {
                        tracing::warn!("telegram approval request failed: {e:#}");
                    }
                }
            }
            ServerMsg::ApprovalResult { .. } => self.status = None,
            _ => {}
        }
    }

    fn view(&self) -> String {
        let mut v = self.text.trim().to_string();
        if let Some(s) = &self.status {
            if !v.is_empty() {
                v += "\n\n";
            }
            v += s;
        }
        v
    }

    async fn put(&mut self, text: &str) {
        let r = match self.msg {
            None => self.bot.send_text(self.chat, text, None).await.map(|id| self.msg = Some(id)),
            Some(id) => self.bot.edit_text(self.chat, id, text).await,
        };
        if let Err(e) = r {
            tracing::warn!("telegram update failed: {e:#}");
        }
    }

    async fn flush(&mut self) {
        if !self.stream {
            return;
        }
        // Finish the current message before it hits the size cap and continue in a new one.
        while self.text.chars().count() > MAX_CHARS {
            let cut: usize = self.text.char_indices().nth(MAX_CHARS).map(|(i, _)| i).unwrap_or(self.text.len());
            let at = self.text[..cut].rfind('\n').filter(|&i| i > cut / 2).map(|i| i + 1).unwrap_or(cut);
            let head: String = self.text.drain(..at).collect();
            self.put(head.trim_end()).await;
            self.msg = None;
            self.shown.clear();
        }
        let v = self.view();
        if v.is_empty() || v == self.shown {
            return;
        }
        self.put(&v).await;
        self.shown = v;
    }

    async fn finish(&mut self, footer: Option<String>) {
        if self.stream {
            self.status = None;
            self.flush().await; // rolls over anything too long
        }
        let mut last = self.text.trim().to_string();
        if let Some(f) = footer {
            last = if last.is_empty() { f } else { format!("{last}\n\n{f}") };
        }
        if last.is_empty() {
            last = "(no reply)".into();
        }
        for (i, part) in split_message(&last, MAX_CHARS + 150).into_iter().enumerate() {
            if i > 0 {
                self.msg = None;
            }
            self.put(&part).await;
        }
    }
}

#[cfg(test)]
mod tests;

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
use crate::voice::{Speech, VOICE_GUIDANCE, approval_intent, spoken_part};

/// Telegram's limit is 4096; keep room for a status line.
const MAX_CHARS: usize = 3900;
const EDIT_EVERY: Duration = Duration::from_millis(1200);
/// Telegram's bot-file size limit.
const MAX_FILE: usize = 20 * 1024 * 1024;

pub struct Bot {
    http: reqwest::Client,
    base: String,
    files: String,
}

impl Bot {
    pub fn new(api_base: &str, token: &str) -> Result<Self> {
        if token.trim().is_empty() {
            bail!("TELEGRAM_BOT_TOKEN is empty");
        }
        // Longer than getUpdates' 50 s long poll.
        let http = reqwest::Client::builder().timeout(Duration::from_secs(75)).connect_timeout(Duration::from_secs(15)).build()?;
        let base = format!("{}/bot{}", api_base.trim_end_matches('/'), token.trim());
        let files = format!("{}/file/bot{}", api_base.trim_end_matches('/'), token.trim());
        Ok(Self { http, base, files })
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

    /// Download a file (voice note) by id. Capped at Telegram's 20 MB bot limit.
    pub async fn download_file(&self, file_id: &str) -> Result<Vec<u8>> {
        let r = self.call("getFile", json!({"file_id": file_id})).await?;
        if let Some(size) = r["file_size"].as_u64()
            && size as usize > MAX_FILE
        {
            bail!("telegram getFile: file too large ({size} bytes)");
        }
        let path = r["file_path"].as_str().context("telegram getFile: no file_path")?;
        let resp = self
            .http
            .get(format!("{}/{}", self.files, path))
            .send()
            .await
            .map_err(|e| anyhow!("telegram download: {}", e.without_url()))?;
        let status = resp.status();
        if !status.is_success() {
            bail!("telegram download: HTTP {status}");
        }
        let bytes = resp.bytes().await.map_err(|e| anyhow!("telegram download: {}", e.without_url()))?;
        if bytes.len() > MAX_FILE {
            bail!("telegram download: file too large ({} bytes)", bytes.len());
        }
        Ok(bytes.to_vec())
    }

    /// Send an OGG/Opus voice note. Returns the message id.
    pub async fn send_voice(&self, chat: i64, ogg: Vec<u8>, caption: Option<String>) -> Result<i64> {
        let part = reqwest::multipart::Part::bytes(ogg).file_name("voice.ogg").mime_str("audio/ogg").map_err(|e| anyhow!("telegram sendVoice: {e}"))?;
        let mut form = reqwest::multipart::Form::new().part("voice", part).text("chat_id", chat.to_string());
        if let Some(c) = caption {
            form = form.text("caption", c);
        }
        let resp = self
            .http
            .post(format!("{}/sendVoice", self.base))
            .multipart(form)
            .send()
            .await
            .map_err(|e| anyhow!("telegram sendVoice: {}", e.without_url()))?;
        let status = resp.status();
        let v: Value = resp.json().await.map_err(|e| anyhow!("telegram sendVoice: {}", e.without_url()))?;
        if v["ok"] != true {
            bail!("telegram sendVoice (HTTP {status}): {}", v["description"].as_str().unwrap_or("unknown error"));
        }
        v["result"]["message_id"].as_i64().context("telegram sendVoice: no message_id")
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
    /// Cloud speech, when `[voice]` is on; `None` = voice notes are refused politely.
    pub speech: Option<Arc<dyn Speech>>,
    /// Refuse voice notes longer than this (seconds).
    pub max_seconds: u32,
    last_task: Arc<StdMutex<HashMap<i64, i64>>>,
    /// chat id → approval id, so a spoken or typed "yes"/"no" can answer it.
    pending: Arc<StdMutex<HashMap<i64, String>>>,
    warned: StdMutex<HashSet<i64>>,
}

impl TelegramChannel {
    pub fn new(bot: Arc<Bot>, cfg: &TelegramCfg, buttons_via_notifier: bool, speech: Option<Arc<dyn Speech>>, max_seconds: u32) -> Self {
        Self {
            bot,
            allowed: cfg.allowed_user_ids.clone(),
            stream: cfg.stream,
            buttons_via_notifier,
            speech,
            max_seconds: max_seconds.max(1),
            last_task: Default::default(),
            pending: Default::default(),
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
        // Voice and audio notes have no `text` field; handle them in a spawned task so a
        // slow download or transcription never stalls the poll loop.
        if let Some(voice) = m.get("voice").or_else(|| m.get("audio")) {
            let (hub, bot, last_task) = (hub.clone(), self.bot.clone(), self.last_task.clone());
            let (stream, own_buttons) = (self.stream, !self.buttons_via_notifier);
            let (speech, pending) = (self.speech.clone(), self.pending.clone());
            let (max_seconds, m, voice) = (self.max_seconds, m.clone(), voice.clone());
            tokio::spawn(async move {
                handle_voice(hub, bot, chat, m, voice, stream, own_buttons, last_task, speech, pending, max_seconds).await;
            });
            return;
        }
        let Some(text) = m["text"].as_str().map(str::trim).filter(|t| !t.is_empty()) else {
            self.reply(chat, "I read text and voice messages.").await;
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
                // A typed "yes"/"no" answers this chat's pending approval.
                if let Some(allow) = approval_intent(&text) {
                    let pending_id = self.pending.lock().unwrap().get(&chat).cloned();
                    if let Some(id) = pending_id {
                        let ok = hub.agent.approvals.as_ref().is_some_and(|a| a.resolve(&id, allow));
                        self.pending.lock().unwrap().remove(&chat);
                        let mark = match (ok, allow) {
                            (true, true) => "✅ Approved",
                            (true, false) => "❌ Denied",
                            (false, _) => "⌛ Already answered or expired.",
                        };
                        self.reply(chat, mark).await;
                        return;
                    }
                }
                let (hub, bot, last_task) = (hub.clone(), self.bot.clone(), self.last_task.clone());
                let (stream, own_buttons) = (self.stream, !self.buttons_via_notifier);
                let (speech, pending) = (self.speech.clone(), self.pending.clone());
                tokio::spawn(async move {
                    run_turn(hub, bot, chat, session, text, stream, own_buttons, last_task, speech, pending, false).await;
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

/// A voice note → transcribe → run a turn with voice guidance. Runs in its own task so a slow
/// download or speech call never stalls the poll loop (button taps and other chats keep flowing).
#[allow(clippy::too_many_arguments)]
async fn handle_voice<C: ModelClient>(
    hub: Arc<Hub<C>>,
    bot: Arc<Bot>,
    chat: i64,
    m: Value,
    voice: Value,
    stream: bool,
    own_buttons: bool,
    last_task: Arc<StdMutex<HashMap<i64, i64>>>,
    speech: Option<Arc<dyn Speech>>,
    pending: Arc<StdMutex<HashMap<i64, String>>>,
    max_seconds: u32,
) {
    let reply = |text: &str| {
        let bot = bot.clone();
        let text = text.to_string();
        async move {
            if let Err(e) = bot.send_long(chat, &text).await {
                tracing::warn!("telegram reply failed: {e:#}");
            }
        }
    };
    let Some(speech) = speech else {
        reply("Voice is off. Set [voice] enabled = true and OPENAI_API_KEY in ~/.config/wang/.env, or send a text message.").await;
        return;
    };
    let dur = voice["duration"].as_u64().unwrap_or(0);
    if dur > max_seconds as u64 {
        reply(&format!("That voice note is too long ({dur}s); keep it under {max_seconds}s.")).await;
        return;
    }
    let Some(file_id) = voice["file_id"].as_str() else {
        reply("Couldn't read that voice note.").await;
        return;
    };
    let bytes = match bot.download_file(file_id).await {
        Ok(b) => b,
        Err(e) => {
            reply(&format!("Couldn't download that voice note: {e:#}")).await;
            return;
        }
    };
    let mime = if m.get("voice").is_some() {
        "audio/ogg"
    } else {
        m.get("audio").and_then(|a| a["mime_type"].as_str()).unwrap_or("audio/mpeg")
    };
    let t = match speech.transcribe(bytes.clone(), mime).await {
        Ok(t) => t,
        Err(e) => {
            reply(&format!("Couldn't hear that: {e:#}")).await;
            return;
        }
    };
    // Meter the transcription as a tool call (a small "voice" task, excluded from latest_task).
    let ledger = &hub.agent.ledger;
    let Ok(vtask) = ledger.start_task("voice", &format!("voice note ({dur}s)")) else { return };
    let _ = ledger.record_tool(vtask, &wang_tokens::ToolRecord {
        name: "voice_stt",
        detail: &format!("{dur}s"),
        raw_bytes: bytes.len(),
        sent_bytes: bytes.len(),
        ms: t.ms,
        ..Default::default()
    });
    let _ = ledger.finish_task(vtask, 0, "ok");
    // Echo the transcript so a mishearing is visible.
    reply(&format!("🎤 \"{}\"", t.text.trim())).await;
    // A spoken "yes"/"no" answers this chat's pending approval.
    if let Some(allow) = approval_intent(&t.text) {
        let pending_id = pending.lock().unwrap().get(&chat).cloned();
        if let Some(id) = pending_id {
            let ok = hub.agent.approvals.as_ref().is_some_and(|a| a.resolve(&id, allow));
            pending.lock().unwrap().remove(&chat);
            let mark = match (ok, allow) {
                (true, true) => "✅ Approved",
                (true, false) => "❌ Denied",
                (false, _) => "⌛ Already answered or expired.",
            };
            reply(mark).await;
            return;
        }
    }
    let session = format!("tg-{chat}");
    let text = format!("{}\n\n[voice] {VOICE_GUIDANCE}", t.text.trim());
    run_turn(hub, bot, chat, session, text, stream, own_buttons, last_task, Some(speech), pending, true).await;
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
    speech: Option<Arc<dyn Speech>>,
    pending: Arc<StdMutex<HashMap<i64, String>>>,
    from_voice: bool,
) {
    let action = if from_voice { "record_voice" } else { "typing" };
    let _ = bot.call("sendChatAction", json!({"chat_id": chat, "action": action})).await;
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
    let mut live = Live { bot: bot.clone(), chat, stream, own_buttons, msg: None, text: String::new(), status: None, shown: String::new(), pending: pending.clone() };
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
            if from_voice && let Some(sp) = &speech {
                let (spoken, _) = spoken_part(&r.reply);
                if !spoken.is_empty() {
                    let started = std::time::Instant::now();
                    match sp.speak(&spoken).await {
                        Ok(ogg) => {
                            let _ = hub.agent.ledger.record_tool(r.task_id, &wang_tokens::ToolRecord {
                                name: "voice_tts",
                                detail: &format!("{} chars", spoken.chars().count()),
                                raw_bytes: ogg.len(),
                                sent_bytes: ogg.len(),
                                ms: started.elapsed().as_millis() as u64,
                                ..Default::default()
                            });
                            if let Err(e) = bot.send_voice(chat, ogg, None).await {
                                tracing::warn!("telegram sendVoice failed: {e:#}");
                            }
                        }
                        Err(e) => {
                            tracing::warn!("voice tts failed: {e:#}");
                            let _ = bot.send_text(chat, "(couldn't speak the answer — the text above is the full reply.)", None).await;
                        }
                    }
                }
            }
        }
        Err(e) => {
            tracing::error!("telegram task failed: {e:#}");
            pending.lock().unwrap().remove(&chat);
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
    /// chat id → approval id, so a spoken or typed "yes"/"no" can answer it.
    pending: Arc<StdMutex<HashMap<i64, String>>>,
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
                self.pending.lock().unwrap().insert(self.chat, id.clone());
                self.status = Some(format!("⏳ waiting for your approval: {tool}"));
                self.flush().await;
                if self.own_buttons {
                    let text = format!("wang wants to run {tool}:\n{}", clip(&detail, 1500));
                    if let Err(e) = self.bot.send_text(self.chat, &text, Some(approval_markup(&id))).await {
                        tracing::warn!("telegram approval request failed: {e:#}");
                    }
                }
            }
            ServerMsg::ApprovalResult { .. } => {
                self.pending.lock().unwrap().remove(&self.chat);
                self.status = None;
            }
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

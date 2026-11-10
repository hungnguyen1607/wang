use super::*;
use crate::agent::tests::agent;
use crate::approvals::Approvals;
use crate::model::testing::{Mock, resp};
use crate::voice::Transcript;
use std::collections::VecDeque;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Minimal fake Bot API: records every call, serves queued updates.
#[derive(Clone, Default)]
struct FakeTg {
    calls: Arc<StdMutex<Vec<(String, Value)>>>,
    updates: Arc<StdMutex<VecDeque<Value>>>,
    next_msg: Arc<StdMutex<i64>>,
}

impl FakeTg {
    async fn start(&self) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = l.accept().await.unwrap();
                let me = me.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    loop {
                        let mut chunk = [0u8; 8192];
                        let Some(he) = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4) else {
                            match s.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            }
                            continue;
                        };
                        let head = String::from_utf8_lossy(&buf[..he]).to_string();
                        let len: usize = head
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                            .unwrap_or(0);
                        while buf.len() < he + len {
                            let n = s.read(&mut chunk).await.unwrap();
                            buf.extend_from_slice(&chunk[..n]);
                        }
                        let path = head.split_whitespace().nth(1).unwrap().to_string();
                        // File downloads go through /file/bot<token>/..., not a JSON method.
                        if path.starts_with("/file/botTEST:token/") {
                            let out = b"OGG".to_vec();
                            let r = format!("HTTP/1.1 200 OK\r\ncontent-type: audio/ogg\r\ncontent-length: {}\r\n\r\n", out.len());
                            if s.write_all(r.as_bytes()).await.is_err() || s.write_all(&out).await.is_err() {
                                return;
                            }
                            buf.drain(..he + len);
                            continue;
                        }
                        let method = path.rsplit('/').next().unwrap().to_string();
                        assert!(path.starts_with("/botTEST:token/"), "{path}");
                        let body: Value = serde_json::from_slice(&buf[he..he + len]).unwrap_or(Value::Null);
                        buf.drain(..he + len);
                        let result = me.handle(&method, &body).await;
                        let out = json!({"ok": true, "result": result}).to_string();
                        let r = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{out}", out.len());
                        if s.write_all(r.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        format!("http://{addr}")
    }

    async fn handle(&self, method: &str, body: &Value) -> Value {
        if method == "getUpdates" {
            let batch: Vec<Value> = self.updates.lock().unwrap().drain(..).collect();
            if batch.is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            return json!(batch);
        }
        self.calls.lock().unwrap().push((method.to_string(), body.clone()));
        match method {
            "getMe" => json!({"username": "wang_test_bot"}),
            "sendMessage" => {
                let mut n = self.next_msg.lock().unwrap();
                *n += 1;
                json!({"message_id": *n, "chat": {"id": body["chat_id"]}})
            }
            "getFile" => json!({"file_path": "voices/F1.ogg", "file_size": 123}),
            "sendVoice" => {
                let mut n = self.next_msg.lock().unwrap();
                *n += 1;
                json!({"message_id": *n})
            }
            _ => json!(true),
        }
    }

    fn push(&self, u: Value) {
        self.updates.lock().unwrap().push_back(u);
    }

    fn texts(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == "sendMessage" || m == "editMessageText")
            .map(|(_, b)| b["text"].as_str().unwrap_or("").to_string())
            .collect()
    }

    fn sent(&self) -> Vec<Value> {
        self.calls.lock().unwrap().iter().filter(|(m, _)| m == "sendMessage").map(|(_, b)| b.clone()).collect()
    }

    async fn wait_for(&self, what: &str, f: impl Fn(&FakeTg) -> bool) {
        for _ in 0..300 {
            if f(self) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}; calls: {:#?}", self.calls.lock().unwrap());
    }
}

fn msg(update_id: i64, from: i64, chat_type: &str, text: &str) -> Value {
    json!({"update_id": update_id, "message": {"message_id": update_id, "from": {"id": from, "username": "u"},
           "chat": {"id": from, "type": chat_type}, "text": text}})
}

fn channel(base: &str, stream: bool, via_notifier: bool) -> TelegramChannel {
    let bot = Arc::new(Bot::new(base, "TEST:token").unwrap());
    let cfg = TelegramCfg { allowed_user_ids: vec![42], stream, ..Default::default() };
    TelegramChannel::new(bot, &cfg, via_notifier, None, 120)
}

#[test]
fn split_and_callbacks() {
    assert_eq!(split_message("hi", 10), vec!["hi"]);
    assert_eq!(split_message("aaaa\nbbbb\ncccc\n", 10), vec!["aaaa\nbbbb\n", "cccc\n"]);
    let parts = split_message(&"é".repeat(25), 10);
    assert_eq!(parts.len(), 3);
    assert!(parts.iter().all(|p| p.chars().count() <= 10));
    assert_eq!(parse_callback("ap:y:abc"), Some(("abc", true)));
    assert_eq!(parse_callback("ap:n:abc"), Some(("abc", false)));
    assert_eq!(parse_callback("ap:x:abc"), None);
    assert_eq!(parse_callback("ap:y:"), None);
    assert_eq!(parse_callback("other"), None);
    let id = "0123456789abcdef0123456789abcdef";
    let m = approval_markup(id);
    assert!(m["inline_keyboard"][0][0]["callback_data"].as_str().unwrap().len() <= 64, "Telegram's callback_data limit");
}

#[tokio::test(flavor = "current_thread")]
async fn chat_streams_reply_and_ignores_strangers() {
    let tg = FakeTg::default();
    let base = tg.start().await;
    let a = agent(Mock::new(vec![
        resp(json!([{"type": "text", "text": "Checking."}, {"type": "tool_use", "id": "t1", "name": "shell", "input": {"command": "echo hi"}}]), "tool_use", 0),
        resp(json!([{"type": "text", "text": " It said hi."}]), "end_turn", 0),
    ]));
    let hub = Arc::new(Hub::new(a));
    let server = tokio::spawn(channel(&base, true, false).serve(hub.clone()));

    tg.push(msg(1, 999, "private", "rm -rf ~"));       // stranger: ignored, no reply
    tg.push(msg(2, 42, "group", "hello"));             // allowed user, but in a group: ignored
    tg.push(msg(3, 42, "private", "say hi"));
    tg.wait_for("final reply", |t| t.texts().last().is_some_and(|x| x.contains("2 calls"))).await;

    let texts = tg.texts();
    let last = texts.last().unwrap();
    assert!(last.starts_with("Checking. It said hi."), "{last}");
    assert!(tg.sent().iter().all(|b| b["chat_id"] == 42), "nothing sent to the stranger or the group");
    assert!(!last.contains("⚙"), "status line removed at the end: {last}");
    assert_eq!(hub.session("tg-42").await.lock().await.messages.len(), 4);

    // commands
    tg.push(msg(4, 42, "private", "/good"));
    tg.wait_for("rating", |t| t.texts().iter().any(|x| x.contains("rated 👍"))).await;
    tg.push(msg(5, 42, "private", "/cost"));
    tg.wait_for("cost", |t| t.texts().iter().any(|x| x.to_lowercase().contains("today"))).await;
    tg.push(msg(6, 42, "private", "/trace"));
    tg.wait_for("trace", |t| t.texts().iter().any(|x| x.contains("shell"))).await;
    tg.push(msg(7, 42, "private", "/new@wang_test_bot"));
    tg.wait_for("reset", |t| t.texts().iter().any(|x| x.contains("new conversation"))).await;
    assert!(hub.session("tg-42").await.lock().await.messages.is_empty());
    server.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn approval_buttons_resolve_and_strangers_cant_tap() {
    let tg = FakeTg::default();
    let base = tg.start().await;
    // ToolCtx::new has the sandbox off, so every shell call asks for approval.
    let appr = Arc::new(Approvals::new(None, Duration::from_secs(5)));
    let a = agent(Mock::new(vec![
        resp(json!([{"type": "tool_use", "id": "t1", "name": "shell", "input": {"command": "echo approved-ran"}}]), "tool_use", 0),
        resp(json!([{"type": "text", "text": "fin"}]), "end_turn", 0),
    ]))
    .with_approvals(Some(appr));
    let hub = Arc::new(Hub::new(a));
    let server = tokio::spawn(channel(&base, false, false).serve(hub.clone()));

    tg.push(msg(1, 42, "private", "run it"));
    tg.wait_for("approval buttons", |t| t.sent().iter().any(|b| b.get("reply_markup").is_some())).await;
    let req = tg.sent().into_iter().find(|b| b.get("reply_markup").is_some()).unwrap();
    assert!(req["text"].as_str().unwrap().contains("echo approved-ran"));
    let data = req["reply_markup"]["inline_keyboard"][0][0]["callback_data"].as_str().unwrap().to_string();
    let tap = |uid: i64, from: i64| {
        json!({"update_id": uid, "callback_query": {"id": format!("cb{uid}"), "from": {"id": from}, "data": data,
               "message": {"message_id": 1, "chat": {"id": 42}, "text": "wang wants to run shell"}}})
    };
    // A stranger's tap does nothing.
    tg.push(tap(2, 999));
    tg.wait_for("stranger answered", |t| t.calls.lock().unwrap().iter().any(|(m, b)| m == "answerCallbackQuery" && b["text"] == "Not allowed.")).await;
    assert!(hub.agent.approvals.as_ref().unwrap().handle_reply("nothing").is_none());
    // The owner's tap approves; the turn finishes.
    tg.push(tap(3, 42));
    tg.wait_for("final reply", |t| t.texts().iter().any(|x| x.starts_with("fin"))).await;
    assert!(tg.texts().iter().any(|x| x.ends_with("✅ Approved")));
    let conv = hub.session("tg-42").await;
    let result = conv.lock().await.messages[2]["content"][0]["content"].as_str().unwrap().to_string();
    assert!(result.contains("approved-ran"), "{result}");
    // A second tap on the same button: already answered.
    tg.push(tap(4, 42));
    tg.wait_for("expired", |t| t.calls.lock().unwrap().iter().any(|(m, b)| m == "answerCallbackQuery" && b["text"] == "Already answered or expired.")).await;
    server.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn notifier_sends_alerts_and_buttons_to_allowed_users() {
    let tg = FakeTg::default();
    let base = tg.start().await;
    let n = Telegram { bot: Arc::new(Bot::new(&base, "TEST:token").unwrap()), users: vec![42, 43] };
    n.send(&Notice { title: "wang: approve task #3?".into(), message: "shell: touch x".into(), approval_id: Some("abc".into()), ..Default::default() })
        .await
        .unwrap();
    let sent = tg.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["chat_id"], 43);
    assert_eq!(sent[0]["text"], "wang: approve task #3?\n\nshell: touch x");
    assert_eq!(sent[0]["reply_markup"]["inline_keyboard"][0][1]["callback_data"], "ap:n:abc");
    assert!(Telegram { bot: n.bot.clone(), users: vec![] }.send(&Notice::default()).await.is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn errors_never_contain_the_token() {
    // Nothing listens on port 9: the connection error must not echo the URL (which holds the token).
    let bot = Bot::new("http://127.0.0.1:9", "SECRET123:abc").unwrap();
    let e = bot.call("getMe", json!({})).await.unwrap_err();
    assert!(!format!("{e:#}").contains("SECRET123"), "{e:#}");
}

#[tokio::test(flavor = "current_thread")]
async fn long_streamed_reply_rolls_over_into_new_messages() {
    let tg = FakeTg::default();
    let base = tg.start().await;
    let bot = Arc::new(Bot::new(&base, "TEST:token").unwrap());
    let mut live = Live { bot, chat: 42, stream: true, own_buttons: false, msg: None, text: String::new(), status: None, shown: String::new(), pending: Default::default() };
    live.on_event(ServerMsg::Text { delta: "line\n".repeat(900) }).await; // 4500 chars
    live.on_event(ServerMsg::ToolStart { name: "shell".into(), detail: "ls".into() }).await;
    live.flush().await;
    let texts = tg.texts();
    assert_eq!(texts.len(), 2, "first message finished, second started");
    assert!(texts.iter().all(|t| t.chars().count() <= 4096));
    assert!(texts[1].ends_with("⚙ shell: ls"));
    live.on_event(ServerMsg::ToolEnd { name: "shell".into(), ok: true, ms: 1, trimmed: false, repeated: false }).await;
    live.finish(Some("— footer".into())).await;
    let texts = tg.texts();
    let last = texts.last().unwrap();
    assert!(last.ends_with("— footer") && !last.contains("⚙"), "{last}");
    let total: usize = [&texts[0], last].iter().map(|t| t.matches("line").count()).sum();
    assert_eq!(total, 900, "no text lost or duplicated across the rollover");
}

struct FakeSpeech {
    transcript: String,
    spoken: Arc<StdMutex<Vec<String>>>,
}

impl Speech for FakeSpeech {
    fn transcribe<'a>(&'a self, _audio: Vec<u8>, _mime: &'a str) -> Pin<Box<dyn Future<Output = anyhow::Result<Transcript>> + Send + 'a>> {
        let text = self.transcript.clone();
        Box::pin(async move { Ok(Transcript { text, ms: 1 }) })
    }

    fn speak<'a>(&'a self, text: &'a str) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<u8>>> + Send + 'a>> {
        self.spoken.lock().unwrap().push(text.to_string());
        Box::pin(async move { Ok(b"OGG".to_vec()) })
    }
}

fn voice_msg(update_id: i64, from: i64, chat_type: &str, duration: u64) -> Value {
    json!({"update_id": update_id, "message": {"message_id": update_id, "from": {"id": from, "username": "u"},
           "chat": {"id": from, "type": chat_type}, "voice": {"file_id": "F1", "duration": duration}}})
}

#[tokio::test(flavor = "current_thread")]
async fn voice_note_runs_a_turn_and_replies_with_voice() {
    let tg = FakeTg::default();
    let base = tg.start().await;
    let speech = Arc::new(FakeSpeech { transcript: "say hi".into(), spoken: Default::default() });
    let a = agent(Mock::new(vec![
        resp(json!([{"type": "text", "text": "Hi there."}]), "end_turn", 0),
    ]));
    let hub = Arc::new(Hub::new(a));
    let bot = Arc::new(Bot::new(&base, "TEST:token").unwrap());
    let cfg = TelegramCfg { allowed_user_ids: vec![42], stream: false, ..Default::default() };
    let server = tokio::spawn(TelegramChannel::new(bot, &cfg, false, Some(speech.clone()), 120).serve(hub.clone()));

    tg.push(voice_msg(1, 42, "private", 3));
    tg.wait_for("voice reply", |t| t.calls.lock().unwrap().iter().any(|(m, _)| m == "sendVoice")).await;

    let texts = tg.texts();
    assert!(texts.iter().any(|x| x.contains("🎤") && x.contains("say hi")), "{texts:?}");
    assert!(texts.iter().any(|x| x.contains("Hi there.")), "{texts:?}");
    assert!(speech.spoken.lock().unwrap().iter().any(|s| s.contains("Hi there.")));
    let conv = hub.session("tg-42").await;
    let msgs = conv.lock().await.messages.iter().any(|m| m.to_string().contains("say hi"));
    assert!(msgs, "the agent received the transcript");
    server.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn voice_ignores_strangers_and_long_clips() {
    let tg = FakeTg::default();
    let base = tg.start().await;
    let speech = Arc::new(FakeSpeech { transcript: "x".into(), spoken: Default::default() });
    let a = agent(Mock::new(vec![]));
    let hub = Arc::new(Hub::new(a));
    let bot = Arc::new(Bot::new(&base, "TEST:token").unwrap());
    let cfg = TelegramCfg { allowed_user_ids: vec![42], stream: false, ..Default::default() };
    let server = tokio::spawn(TelegramChannel::new(bot, &cfg, false, Some(speech), 120).serve(hub.clone()));

    tg.push(voice_msg(1, 999, "private", 3)); // stranger
    tg.push(voice_msg(2, 42, "private", 300)); // too long
    tg.wait_for("refusal", |t| t.texts().iter().any(|x| x.contains("too long"))).await;

    let texts = tg.texts();
    assert!(texts.iter().any(|x| x.contains("too long")), "{texts:?}");
    assert!(!texts.iter().any(|x| x.contains("🎤")), "stranger and long clips never reach speech: {texts:?}");
    server.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn spoken_yes_resolves_pending_approval() {
    let tg = FakeTg::default();
    let base = tg.start().await;
    let speech = Arc::new(FakeSpeech { transcript: "yes".into(), spoken: Default::default() });
    let appr = Arc::new(Approvals::new(None, Duration::from_secs(5)));
    let a = agent(Mock::new(vec![
        resp(json!([{"type": "tool_use", "id": "t1", "name": "shell", "input": {"command": "echo hi"}}]), "tool_use", 0),
        resp(json!([{"type": "text", "text": "fin"}]), "end_turn", 0),
    ])).with_approvals(Some(appr));
    let hub = Arc::new(Hub::new(a));
    let bot = Arc::new(Bot::new(&base, "TEST:token").unwrap());
    let cfg = TelegramCfg { allowed_user_ids: vec![42], stream: false, ..Default::default() };
    let server = tokio::spawn(TelegramChannel::new(bot, &cfg, false, Some(speech), 120).serve(hub.clone()));

    tg.push(msg(1, 42, "private", "run it"));
    tg.wait_for("approval buttons", |t| t.sent().iter().any(|b| b.get("reply_markup").is_some())).await;
    // Spoken "yes" resolves the approval.
    tg.push(voice_msg(2, 42, "private", 2));
    tg.wait_for("approved", |t| t.texts().iter().any(|x| x.contains("✅ Approved"))).await;
    tg.wait_for("final reply", |t| t.texts().iter().any(|x| x.starts_with("fin"))).await;
    server.abort();
}

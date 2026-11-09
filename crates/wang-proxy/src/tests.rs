use std::collections::HashMap;
use std::collections::VecDeque;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;

/// What the fake upstream received.
#[derive(Debug, Clone)]
struct Seen {
    target: String,
    headers: HashMap<String, String>,
    body: Value,
}

/// A fake provider: replays canned responses in order (the last one repeats) and records requests.
struct Upstream {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Upstream {
    async fn start(replies: Vec<(u16, &'static str, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
        let s = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let (s, replies) = (s.clone(), replies.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let (head_end, len) = loop {
                        let mut chunk = [0u8; 65536];
                        let n = sock.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..i]).to_ascii_lowercase();
                            let len = head
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .map_or(0, |v| v.trim().parse().unwrap());
                            break (i + 4, len);
                        }
                    };
                    while buf.len() < head_end + len {
                        let mut chunk = [0u8; 65536];
                        let n = sock.read(&mut chunk).await.unwrap();
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    let mut lines = head.lines();
                    let target = lines.next().unwrap().split(' ').nth(1).unwrap().to_string();
                    let headers = lines
                        .filter_map(|l| l.split_once(':'))
                        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                        .collect();
                    let body = serde_json::from_slice(&buf[head_end..head_end + len]).unwrap_or(Value::Null);
                    s.lock().unwrap().push(Seen { target, headers, body });
                    let (status, ct, body) = {
                        let mut q = replies.lock().unwrap();
                        if q.len() > 1 { q.pop_front().unwrap() } else { q.front().unwrap().clone() }
                    };
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: {ct}\r\ncontent-length: {}\r\nrequest-id: req_123\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        Self { addr, seen }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

async fn start_proxy(cfg: Config) -> (String, Arc<Ledger>) {
    let ledger = Arc::new(Ledger::in_memory().unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let proxy = Arc::new(Proxy::new(cfg, ledger.clone()).unwrap());
    tokio::spawn(serve(listener, proxy));
    (format!("http://{addr}"), ledger)
}

/// The ledger is written after the response body ends; give the pump task a moment.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

const SSE: &str = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-opus-5-5\",\"usage\":{\"input_tokens\":100,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":900000,\"output_tokens\":1}}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":50}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";

fn claude_code_body(outputs: &[String]) -> Value {
    let mut messages = vec![json!({"role": "user", "content": [
        {"type": "text", "text": "<system-reminder>ctx</system-reminder>"},
        {"type": "text", "text": "fix the failing test"}]})];
    for (i, out) in outputs.iter().enumerate() {
        messages.push(json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "", "signature": format!("sig{i}")},
            {"type": "tool_use", "id": format!("toolu_{i}"), "name": "Bash", "input": {"command": "cargo test"}}]}));
        messages.push(json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": format!("toolu_{i}"), "content": out}]}));
    }
    // Tool schema with keys out of alphabetical order: must reach upstream in the same order.
    json!({"model": "claude-opus-5-5", "max_tokens": 32000, "stream": true, "system": "You are Claude Code.",
           "tools": [{"name": "Bash", "input_schema": {"type": "object", "properties": {"z": {}, "a": {}}}}],
           "messages": messages})
}

fn claude_code_request(c: &reqwest::Client, base: &str, body: &Value) -> reqwest::RequestBuilder {
    c.post(format!("{base}/v1/messages?beta=true"))
        .header("x-api-key", "sk-ant-test")
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", "interleaved-thinking-2025-05-14")
        .header("user-agent", "claude-cli/2.1.318 (external, cli)")
        .header("accept-encoding", "gzip, br")
        .json(body)
}

#[tokio::test(flavor = "current_thread")]
async fn claude_code_stream_passes_through_rewritten_and_metered() {
    let up = Upstream::start(vec![(200, "text/event-stream", SSE.into())]).await;
    let (base, ledger) = start_proxy(Config {
        anthropic_upstream: up.url(),
        rules: Rules { cap_chars: 10_000, ..Default::default() },
        ..Default::default()
    })
    .await;
    let c = reqwest::Client::new();
    let log = "test output line\n".repeat(200); // 3.4k chars, repeated below
    let huge = "x".repeat(60_000);

    // Turn 1, then turn 2 of the same conversation with more history.
    let b1 = claude_code_body(&[log.clone()]);
    let b2 = claude_code_body(&[log.clone(), huge.clone(), log.clone()]);
    for b in [&b1, &b2] {
        let r = claude_code_request(&c, &base, b).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers()["request-id"], "req_123");
        assert_eq!(r.text().await.unwrap(), SSE, "stream must reach the client byte for byte");
    }
    settle().await;

    let seen = up.seen();
    assert_eq!(seen.len(), 2);
    let s = &seen[1];
    assert_eq!(s.target, "/v1/messages?beta=true");
    assert_eq!(s.headers["x-api-key"], "sk-ant-test");
    assert_eq!(s.headers["anthropic-beta"], "interleaved-thinking-2025-05-14");
    assert_eq!(s.headers["accept-encoding"], "identity");
    assert_eq!(s.headers["host"], up.addr.to_string());
    let results: Vec<&str> =
        s.body["messages"].as_array().unwrap().iter().filter_map(|m| m["content"][0]["content"].as_str()).collect();
    assert_eq!(results[0], log);
    assert!(results[1].len() < 11_000 && results[1].contains("omitted by wang"));
    assert!(results[2].starts_with("[same output as earlier tool call toolu_0"));
    assert_eq!(s.body["cache_control"]["type"], "ephemeral");
    assert_eq!(s.body["messages"][1]["content"][0]["signature"], "sig0");
    let props: Vec<&String> = s.body["tools"][0]["input_schema"]["properties"].as_object().unwrap().keys().collect();
    assert_eq!(props, ["z", "a"]);
    // Turn 1's rewritten history is a prefix of turn 2's (cache + thinking stay valid).
    let m1 = seen[0].body["messages"].as_array().unwrap();
    assert_eq!(&s.body["messages"].as_array().unwrap()[..m1.len()], &m1[..]);

    // Ledger: one task for the conversation, two metered calls, priced from the table.
    let sum = ledger.summary(1, "today").unwrap();
    assert_eq!(sum.tasks.len(), 1, "{sum}");
    assert_eq!(sum.calls, 2);
    assert_eq!(sum.tasks[0].prompt, "fix the failing test");
    assert_eq!(sum.tasks[0].status, "proxied");
    assert_eq!(sum.usage.cache_read_tokens, 1_800_000);
    assert!((sum.cost_usd - 2.0 * (100.0 * 4.0 + 50.0 * 20.0 + 900_000.0 * 0.20) / 1e6).abs() < 1e-9, "{}", sum.cost_usd);
    let p = ledger.proxy_summary(1, "today").unwrap();
    let row = &p.clients[0];
    assert_eq!(row.client, "claude-code");
    assert_eq!((row.requests, row.trimmed, row.deduped, row.cache_hints), (2, 1, 1, 2));
    assert!(row.bytes_in > row.bytes_out + 50_000);
}

#[tokio::test(flavor = "current_thread")]
async fn openai_route_json_response_and_untouched_passthrough() {
    let reply = json!({"model": "anthropic/claude-sonnet-5.5", "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
                       "usage": {"prompt_tokens": 1000, "completion_tokens": 20, "prompt_tokens_details": {"cached_tokens": 800}, "cost": 0.0031}});
    let up = Upstream::start(vec![(200, "application/json", reply.to_string())]).await;
    let (base, ledger) =
        start_proxy(Config { openai_upstream: format!("{}/api/v1", up.url()), ..Default::default() }).await;
    let c = reqwest::Client::new();
    let body = json!({"model": "openai/gpt-x", "messages": [{"role": "user", "content": "hello"}]});
    let raw = serde_json::to_vec_pretty(&body).unwrap();
    let r = c
        .post(format!("{base}/openai/v1/chat/completions"))
        .header("authorization", "Bearer sk-or-test")
        .header("user-agent", "opencode/1.0 ai-sdk")
        .header("content-type", "application/json")
        .body(raw.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.json::<Value>().await.unwrap(), reply);
    settle().await;

    let s = &up.seen()[0];
    assert_eq!(s.target, "/api/v1/chat/completions");
    assert_eq!(s.headers["authorization"], "Bearer sk-or-test");
    // Nothing to rewrite: the exact bytes go upstream.
    assert_eq!(s.headers["content-length"], raw.len().to_string());

    let sum = ledger.summary(1, "today").unwrap();
    assert_eq!(sum.calls, 1);
    assert!((sum.cost_usd - 0.0031).abs() < 1e-12);
    assert_eq!(sum.usage.input_tokens, 200);
    assert_eq!(ledger.proxy_summary(1, "today").unwrap().clients[0].client, "opencode");
}

#[tokio::test(flavor = "current_thread")]
async fn daily_budget_refuses_before_upstream() {
    let up = Upstream::start(vec![(200, "text/event-stream", SSE.into())]).await;
    let (base, _ledger) =
        start_proxy(Config { anthropic_upstream: up.url(), daily_budget_usd: Some(0.01), ..Default::default() }).await;
    let c = reqwest::Client::new();
    let b = claude_code_body(&[]);
    assert_eq!(claude_code_request(&c, &base, &b).send().await.unwrap().text().await.unwrap(), SSE);
    settle().await; // first call cost $0.18 > $0.01

    let r = claude_code_request(&c, &base, &b).send().await.unwrap();
    assert_eq!(r.status(), 403);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["type"], "permission_error");
    assert!(v["error"]["message"].as_str().unwrap().contains("budget"));
    assert_eq!(up.seen().len(), 1, "refused call must not reach upstream");

    // Non-metered endpoints still work.
    let r = c.post(format!("{base}/v1/messages/count_tokens")).json(&b).send().await.unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test(flavor = "current_thread")]
async fn thinking_mismatch_retries_once_without_thinking() {
    let err = json!({"type": "error", "error": {"type": "invalid_request_error",
        "message": "messages.1.content.0: Invalid `signature` in `thinking` block. The block is bound to a different conversation."}});
    let up = Upstream::start(vec![(400, "application/json", err.to_string()), (200, "text/event-stream", SSE.into())]).await;
    let (base, _) = start_proxy(Config { anthropic_upstream: up.url(), ..Default::default() }).await;
    let c = reqwest::Client::new();
    let r = claude_code_request(&c, &base, &claude_code_body(&["a".into(), "b".into()])).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), SSE);
    let seen = up.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].body["messages"][1]["content"][0]["type"], "thinking");
    assert_eq!(seen[1].body["messages"][1]["content"][0]["type"], "tool_use");

    // Any other 400 is returned as is, once.
    let other = json!({"type": "error", "error": {"type": "invalid_request_error", "message": "max_tokens too large"}});
    let up = Upstream::start(vec![(400, "application/json", other.to_string())]).await;
    let (base, _) = start_proxy(Config { anthropic_upstream: up.url(), ..Default::default() }).await;
    let r = claude_code_request(&c, &base, &claude_code_body(&["a".into()])).send().await.unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(r.json::<Value>().await.unwrap(), other);
    assert_eq!(up.seen().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn upstream_down_and_unknown_paths() {
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap(); // closed when dropped
    let (base, ledger) = start_proxy(Config { anthropic_upstream: format!("http://{dead}"), ..Default::default() }).await;
    let c = reqwest::Client::new();
    let r = claude_code_request(&c, &base, &claude_code_body(&[])).send().await.unwrap();
    assert_eq!(r.status(), 502);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["type"], "error");
    assert!(!v.to_string().contains("sk-ant-test"));
    assert_eq!(ledger.proxy_summary(1, "today").unwrap().clients[0].errors, 1);

    assert_eq!(c.get(format!("{base}/health")).send().await.unwrap().status(), 200);
    assert_eq!(c.get(format!("{base}/etc/passwd")).send().await.unwrap().status(), 404);
}

#[test]
fn routes() {
    let p = Proxy::new(
        Config { anthropic_upstream: "https://a.test".into(), openai_upstream: "https://o.test/api/v1/".into(), ..Default::default() },
        Arc::new(Ledger::in_memory().unwrap()),
    )
    .unwrap();
    let r = |m: Method, path: &str| p.route(&m, path).map(|r| (r.api, r.url, r.rewrite, r.meter));
    assert_eq!(r(Method::POST, "/v1/messages?beta=true"), Some((Api::Anthropic, "https://a.test/v1/messages?beta=true".into(), true, true)));
    assert_eq!(r(Method::POST, "/v1/messages/count_tokens"), Some((Api::Anthropic, "https://a.test/v1/messages/count_tokens".into(), true, false)));
    assert_eq!(r(Method::GET, "/v1/models"), Some((Api::Anthropic, "https://a.test/v1/models".into(), false, false)));
    assert_eq!(r(Method::POST, "/openai/v1/chat/completions"), Some((Api::OpenAi, "https://o.test/api/v1/chat/completions".into(), true, true)));
    assert_eq!(r(Method::POST, "/openai/chat/completions"), Some((Api::OpenAi, "https://o.test/api/v1/chat/completions".into(), true, true)));
    assert_eq!(r(Method::POST, "/v1/chat/completions"), Some((Api::OpenAi, "https://o.test/api/v1/chat/completions".into(), true, true)));
    assert_eq!(r(Method::GET, "/openai/v1/models"), Some((Api::OpenAi, "https://o.test/api/v1/models".into(), false, false)));
    assert_eq!(r(Method::GET, "/other"), None);
}

#[test]
fn client_names() {
    let h = |ua: &str| {
        let mut m = HeaderMap::new();
        m.insert(header::USER_AGENT, HeaderValue::from_str(ua).unwrap());
        client_name(&m)
    };
    assert_eq!(h("claude-cli/2.1.318 (external, cli)"), "claude-code");
    assert_eq!(h("opencode/0.5 ai-sdk/5"), "opencode");
    assert_eq!(h("Aider/0.9"), "aider");
    assert_eq!(client_name(&HeaderMap::new()), "other");
}

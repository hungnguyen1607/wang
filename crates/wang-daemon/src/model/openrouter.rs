//! OpenRouter client (OpenAI-compatible chat completions, streamed over SSE).
//!
//! - Translates wang's Anthropic-style history to chat-completions messages and back.
//! - Claude models: explicit cache breakpoint on the system prompt + top-level automatic caching.
//! - Sticky provider routing per conversation (`x-session-id`) so the cache stays warm.
//! - Cost comes from `usage.cost` (always included by OpenRouter).
//! - Reasoning is preserved across tool calls via `reasoning_details` on the assistant message.

use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use wang_tokens::Usage;

use super::{CallCtx, MAX_ATTEMPTS, ModelClient, ModelResponse, backoff, http_client, is_retryable};
use crate::config::ModelCfg;

pub struct OpenRouter {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    cfg: ModelCfg,
    model: String,
}

impl OpenRouter {
    pub fn new(cfg: &ModelCfg) -> Result<Self> {
        let api_key = std::env::var("OPENROUTER_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())
            .context("OPENROUTER_API_KEY is not set (put it in ~/.config/wang/.env)")?;
        Self::with_key(cfg, api_key)
    }

    pub fn with_key(cfg: &ModelCfg, api_key: String) -> Result<Self> {
        let base_url = cfg
            .base_url
            .clone()
            .unwrap_or_else(|| "https://openrouter.ai/api/v1".into())
            .trim_end_matches('/')
            .to_string();
        Ok(Self { http: http_client()?, api_key, base_url, cfg: cfg.clone(), model: cfg.model_id() })
    }

    pub fn build_body(model: &str, cfg: &ModelCfg, system: &str, tools: &[Value], messages: &[Value]) -> Value {
        let claude = model.starts_with("anthropic/");
        let system_msg = if claude {
            json!({"role": "system", "content": [{"type": "text", "text": system, "cache_control": {"type": "ephemeral"}}]})
        } else {
            json!({"role": "system", "content": system})
        };
        let mut msgs = vec![system_msg];
        msgs.extend(to_chat_messages(messages));
        let mut body = json!({
            "model": model,
            "messages": msgs,
            "max_tokens": cfg.max_tokens,
            "stream": true,
        });
        if !tools.is_empty() {
            body["tools"] = json!(to_chat_tools(tools));
        }
        if claude {
            body["cache_control"] = json!({"type": "ephemeral"});
        }
        if let Some(effort) = &cfg.reasoning_effort {
            body["reasoning"] = json!({"effort": effort});
        }
        body
    }
}

/// Anthropic-style tool definitions → chat-completions `tools`.
pub fn to_chat_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({"type": "function", "function": {
                "name": t["name"], "description": t["description"], "parameters": t["input_schema"]
            }})
        })
        .collect()
}

fn content_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// Anthropic-style history → chat-completions messages.
pub fn to_chat_messages(messages: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for m in messages {
        let blocks: Vec<Value> = match &m["content"] {
            Value::Array(a) => a.clone(),
            Value::String(s) => vec![json!({"type": "text", "text": s})],
            _ => vec![],
        };
        match m["role"].as_str() {
            Some("user") => {
                // Tool results must directly follow the assistant's tool_calls, so they go first.
                for b in blocks.iter().filter(|b| b["type"] == "tool_result") {
                    let mut text = content_text(&b["content"]);
                    if b["is_error"] == true {
                        text = format!("[tool error]\n{text}");
                    }
                    out.push(json!({"role": "tool", "tool_call_id": b["tool_use_id"], "content": text}));
                }
                let texts: Vec<&str> =
                    blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
                if !texts.is_empty() {
                    out.push(json!({"role": "user", "content": texts.join("\n\n")}));
                }
            }
            Some("assistant") => {
                let text: String =
                    blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("");
                let calls: Vec<Value> = blocks
                    .iter()
                    .filter(|b| b["type"] == "tool_use")
                    .map(|b| {
                        json!({"id": b["id"], "type": "function", "function": {
                            "name": b["name"], "arguments": serde_json::to_string(&b["input"]).unwrap_or_else(|_| "{}".into())
                        }})
                    })
                    .collect();
                let mut msg = json!({"role": "assistant"});
                msg["content"] = if text.is_empty() && !calls.is_empty() { Value::Null } else { json!(text) };
                if !calls.is_empty() {
                    msg["tool_calls"] = json!(calls);
                }
                if let Some(rd) = m.get("reasoning_details") {
                    msg["reasoning_details"] = rd.clone();
                }
                out.push(msg);
            }
            _ => {}
        }
    }
    out
}

#[derive(Default, Debug)]
struct PartialCall {
    id: String,
    name: String,
    args: String,
}

/// Incremental SSE parser that rebuilds one chat completion from its stream.
#[derive(Default)]
pub struct StreamAcc {
    buf: Vec<u8>,
    text: String,
    calls: Vec<PartialCall>,
    reasoning: Vec<Value>,
    finish: Option<String>,
    usage: Option<Value>,
    model: Option<String>,
    pub done: bool,
    pub got_first_token: bool,
}

impl StreamAcc {
    /// Feed raw bytes; complete lines are parsed. Returns text deltas to stream out.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<String>> {
        self.buf.extend_from_slice(bytes);
        let mut deltas = Vec::new();
        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim();
            if line.is_empty() || line.starts_with(':') {
                continue; // blank separator or keep-alive comment (": OPENROUTER PROCESSING")
            }
            let Some(payload) = line.strip_prefix("data:") else { continue };
            let payload = payload.trim();
            if payload == "[DONE]" {
                self.done = true;
                continue;
            }
            let chunk: Value = serde_json::from_str(payload).with_context(|| format!("bad SSE chunk: {payload}"))?;
            if let Some(d) = self.apply(&chunk)? {
                deltas.push(d);
            }
        }
        Ok(deltas)
    }

    fn apply(&mut self, chunk: &Value) -> Result<Option<String>> {
        if let Some(err) = chunk.get("error") {
            let msg = err.get("message").and_then(Value::as_str).unwrap_or("unknown error");
            bail!("provider error mid-stream: {msg}");
        }
        if let Some(m) = chunk.get("model").and_then(Value::as_str) {
            self.model = Some(m.to_string());
        }
        if let Some(u) = chunk.get("usage").filter(|u| !u.is_null()) {
            self.usage = Some(u.clone());
        }
        let Some(choice) = chunk.get("choices").and_then(|c| c.get(0)) else { return Ok(None) };
        if let Some(f) = choice.get("finish_reason").and_then(Value::as_str) {
            if f == "error" {
                bail!("provider error mid-stream (finish_reason=error)");
            }
            self.finish = Some(f.to_string());
        }
        let delta = &choice["delta"];
        let mut out = None;
        if let Some(t) = delta.get("content").and_then(Value::as_str) {
            if !t.is_empty() {
                self.got_first_token = true;
                self.text.push_str(t);
                out = Some(t.to_string());
            }
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            self.got_first_token = true;
            for (i, tc) in calls.iter().enumerate() {
                let idx = tc.get("index").and_then(Value::as_u64).map(|x| x as usize).unwrap_or(i);
                while self.calls.len() <= idx {
                    self.calls.push(PartialCall::default());
                }
                let pc = &mut self.calls[idx];
                if let Some(id) = tc.get("id").and_then(Value::as_str) {
                    pc.id = id.to_string();
                }
                if let Some(f) = tc.get("function") {
                    if let Some(n) = f.get("name").and_then(Value::as_str) {
                        pc.name.push_str(n);
                    }
                    if let Some(a) = f.get("arguments").and_then(Value::as_str) {
                        pc.args.push_str(a);
                    }
                }
            }
        }
        if let Some(rd) = delta.get("reasoning_details").and_then(Value::as_array) {
            self.got_first_token = true;
            for item in rd {
                self.push_reasoning(item);
            }
        }
        Ok(out)
    }

    /// Reasoning details arrive in order; fragments of the same block share `index` + `type`
    /// and are concatenated. Whole blocks are kept as-is.
    fn push_reasoning(&mut self, item: &Value) {
        if let Some(last) = self.reasoning.last_mut() {
            let same = item.get("index").is_some()
                && last.get("index") == item.get("index")
                && last.get("type") == item.get("type");
            if same {
                for (k, v) in item.as_object().into_iter().flatten() {
                    match (k.as_str(), v, last.get_mut(k)) {
                        ("text" | "summary" | "data", Value::String(s), Some(Value::String(prev))) => prev.push_str(s),
                        (_, v, _) if !v.is_null() => {
                            last[k] = v.clone();
                        }
                        _ => {}
                    }
                }
                return;
            }
        }
        self.reasoning.push(item.clone());
    }

    pub fn finish(self, requested_model: &str) -> Result<ModelResponse> {
        if self.finish.is_none() && !self.done {
            bail!("stream ended before the model finished");
        }
        let mut content = Vec::new();
        if !self.text.is_empty() {
            content.push(json!({"type": "text", "text": self.text}));
        }
        let has_calls = !self.calls.is_empty();
        for (i, c) in self.calls.into_iter().enumerate() {
            let input: Value = if c.args.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&c.args).unwrap_or_else(|_| json!({"_invalid_arguments": c.args}))
            };
            let id = if c.id.is_empty() { format!("call_{i}") } else { c.id };
            content.push(json!({"type": "tool_use", "id": id, "name": c.name, "input": input}));
        }
        let stop_reason = match self.finish.as_deref() {
            // A call cut off by the length limit has incomplete arguments: never run it.
            Some("length") => "max_tokens",
            _ if has_calls => "tool_use",
            Some("content_filter") => "refusal",
            _ => "end_turn",
        }
        .to_string();

        let u = self.usage.unwrap_or(Value::Null);
        let g = |v: &Value, path: &[&str]| -> u64 {
            let mut cur = v;
            for p in path {
                cur = &cur[*p];
            }
            cur.as_u64().unwrap_or(0)
        };
        let prompt = g(&u, &["prompt_tokens"]);
        let cached = g(&u, &["prompt_tokens_details", "cached_tokens"]);
        let written = g(&u, &["prompt_tokens_details", "cache_write_tokens"]);
        let usage = Usage {
            input_tokens: prompt.saturating_sub(cached + written),
            output_tokens: g(&u, &["completion_tokens"]),
            cache_write_tokens: written,
            cache_read_tokens: cached,
            reasoning_tokens: g(&u, &["completion_tokens_details", "reasoning_tokens"]),
        };
        let mut message_extra = Map::new();
        if !self.reasoning.is_empty() {
            message_extra.insert("reasoning_details".into(), Value::Array(self.reasoning));
        }
        Ok(ModelResponse {
            content,
            message_extra,
            stop_reason,
            usage,
            model: self.model.unwrap_or_else(|| requested_model.to_string()),
            cost_usd: u.get("cost").and_then(Value::as_f64),
            ttft_ms: None,
            request_bytes: 0,
        })
    }
}

impl ModelClient for OpenRouter {
    fn provider(&self) -> &'static str {
        "openrouter"
    }
    fn model(&self) -> &str {
        &self.model
    }

    async fn create(&self, system: &str, tools: &[Value], messages: &[Value], ctx: &CallCtx<'_>) -> Result<ModelResponse> {
        let body = serde_json::to_string(&Self::build_body(&self.model, &self.cfg, system, tools, messages))?;
        let url = format!("{}/chat/completions", self.base_url);

        let mut attempt = 0;
        let (mut resp, started) = loop {
            attempt += 1;
            let started = Instant::now();
            let sent = self
                .http
                .post(&url)
                .bearer_auth(&self.api_key)
                .header("content-type", "application/json")
                .header("x-title", "wang")
                .header("x-session-id", ctx.session)
                .body(body.clone())
                .send()
                .await;
            let resp = match sent {
                Ok(r) => r,
                Err(e) if attempt < MAX_ATTEMPTS && (e.is_connect() || e.is_timeout()) => {
                    tracing::warn!("model request failed ({e}), retrying");
                    tokio::time::sleep(backoff(attempt, None)).await;
                    continue;
                }
                Err(e) => return Err(e).context("sending model request"),
            };
            let status = resp.status();
            if status.is_success() {
                break (resp, started);
            }
            let wait = backoff(attempt, resp.headers().get("retry-after"));
            let text = resp.text().await.unwrap_or_default();
            if is_retryable(status) && attempt < MAX_ATTEMPTS {
                tracing::warn!("OpenRouter {status}, retrying (attempt {attempt})");
                tokio::time::sleep(wait).await;
                continue;
            }
            let hint = match status.as_u16() {
                401 => " (check OPENROUTER_API_KEY)",
                402 => " (out of OpenRouter credits)",
                _ => "",
            };
            bail!("OpenRouter returned {status}{hint}: {}", text.chars().take(800).collect::<String>());
        };

        let mut acc = StreamAcc::default();
        let mut ttft = None;
        while let Some(chunk) = resp.chunk().await.context("reading model stream")? {
            for d in acc.feed(&chunk)? {
                (ctx.on_text)(&d);
            }
            if ttft.is_none() && acc.got_first_token {
                ttft = Some(started.elapsed().as_millis() as u64);
            }
        }
        acc.feed(b"\n")?; // flush a final line without trailing newline
        let mut r = acc.finish(&self.model)?;
        r.ttft_ms = ttft;
        r.request_bytes = body.len();
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_translation() {
        let hist = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "list files"}]}),
            json!({"role": "assistant", "reasoning_details": [{"type": "reasoning.text", "text": "hm"}],
                   "content": [{"type": "text", "text": "Checking."},
                               {"type": "tool_use", "id": "c1", "name": "shell", "input": {"command": "ls"}}]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "c1", "content": "a\nb", "is_error": true},
                {"type": "text", "text": "and then?"}]}),
        ];
        let m = to_chat_messages(&hist);
        assert_eq!(m.len(), 4);
        assert_eq!(m[0], json!({"role": "user", "content": "list files"}));
        assert_eq!(m[1]["tool_calls"][0]["function"]["name"], "shell");
        assert_eq!(m[1]["tool_calls"][0]["function"]["arguments"], "{\"command\":\"ls\"}");
        assert_eq!(m[1]["reasoning_details"][0]["text"], "hm");
        assert_eq!(m[2]["role"], "tool");
        assert_eq!(m[2]["tool_call_id"], "c1");
        assert!(m[2]["content"].as_str().unwrap().starts_with("[tool error]"));
        assert_eq!(m[3], json!({"role": "user", "content": "and then?"}));
    }

    #[test]
    fn body_caching_for_claude_only() {
        let cfg = ModelCfg::default();
        let b = OpenRouter::build_body("anthropic/claude-haiku-5.5", &cfg, "sys", &crate::tools::definitions(), &[]);
        assert_eq!(b["cache_control"]["type"], "ephemeral");
        assert_eq!(b["messages"][0]["content"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(b["tools"][1]["function"]["name"], "shell");
        assert_eq!(b["stream"], true);
        let b = OpenRouter::build_body("deepseek/deepseek-v4-flash", &cfg, "sys", &[], &[]);
        assert!(b.get("cache_control").is_none());
        assert_eq!(b["messages"][0]["content"], "sys");
    }

    fn sse(lines: &[Value]) -> String {
        let mut s = String::from(": OPENROUTER PROCESSING\n\n");
        for l in lines {
            s += &format!("data: {l}\n\n");
        }
        s + "data: [DONE]\n\n"
    }

    #[test]
    fn stream_text_tools_reasoning_usage() {
        let stream = sse(&[
            json!({"model": "anthropic/claude-haiku-5.5", "choices": [{"delta": {"reasoning_details": [{"type": "reasoning.text", "text": "Let me ", "index": 0}]}}]}),
            json!({"choices": [{"delta": {"reasoning_details": [{"type": "reasoning.text", "text": "think", "signature": "sig", "index": 0}]}}]}),
            json!({"choices": [{"delta": {"content": "Héllo "}}]}),
            json!({"choices": [{"delta": {"content": "there"}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_a", "function": {"name": "shell", "arguments": "{\"comm"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "and\":\"ls\"}"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 1, "id": "call_b", "function": {"name": "read_file", "arguments": "{\"path\":\"x\"}"}}]}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
            json!({"choices": [{"delta": {"content": ""}, "finish_reason": "tool_calls"}],
                   "usage": {"prompt_tokens": 1000, "completion_tokens": 80,
                             "prompt_tokens_details": {"cached_tokens": 700, "cache_write_tokens": 200},
                             "completion_tokens_details": {"reasoning_tokens": 30}, "cost": 0.00123}}),
        ]);
        // Feed in awkward 7-byte pieces to exercise line buffering and split UTF-8.
        let mut acc = StreamAcc::default();
        let mut streamed = String::new();
        for piece in stream.as_bytes().chunks(7) {
            for d in acc.feed(piece).unwrap() {
                streamed += &d;
            }
        }
        assert_eq!(streamed, "Héllo there");
        let r = acc.finish("req").unwrap();
        assert_eq!(r.stop_reason, "tool_use");
        assert_eq!(r.model, "anthropic/claude-haiku-5.5");
        assert_eq!(r.content[0]["text"], "Héllo there");
        assert_eq!(r.content[1]["input"], json!({"command": "ls"}));
        assert_eq!(r.content[2]["name"], "read_file");
        assert_eq!(r.usage.input_tokens, 100);
        assert_eq!(r.usage.cache_read_tokens, 700);
        assert_eq!(r.usage.cache_write_tokens, 200);
        assert_eq!(r.usage.reasoning_tokens, 30);
        assert_eq!(r.cost_usd, Some(0.00123));
        let rd = &r.message_extra["reasoning_details"];
        assert_eq!(rd.as_array().unwrap().len(), 1);
        assert_eq!(rd[0]["text"], "Let me think");
        assert_eq!(rd[0]["signature"], "sig");
    }

    #[test]
    fn stream_error_chunk() {
        let stream = sse(&[
            json!({"choices": [{"delta": {"content": "partial"}}]}),
            json!({"error": {"code": "server_error", "message": "Provider disconnected"}, "choices": [{"delta": {"content": ""}, "finish_reason": "error"}]}),
        ]);
        let mut acc = StreamAcc::default();
        let err = acc.feed(stream.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("Provider disconnected"));
    }

    #[test]
    fn truncated_stream_is_an_error() {
        let mut acc = StreamAcc::default();
        acc.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n").unwrap();
        assert!(acc.finish("m").is_err());
    }

    #[test]
    fn length_and_bad_args() {
        let stream = sse(&[
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c", "function": {"name": "shell", "arguments": "{\"command\": \"ec"}}]}, "finish_reason": "length"}]}),
        ]);
        let mut acc = StreamAcc::default();
        acc.feed(stream.as_bytes()).unwrap();
        let r = acc.finish("m").unwrap();
        assert_eq!(r.stop_reason, "max_tokens");
        assert!(r.content[0]["input"]["_invalid_arguments"].is_string());
    }
}

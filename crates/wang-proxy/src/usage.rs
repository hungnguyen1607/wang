//! Reads token usage out of a response while it streams through, without holding the body.

use std::time::Instant;

use serde_json::Value;
use wang_tokens::Usage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    Anthropic,
    OpenAi,
}

impl Api {
    pub fn name(self) -> &'static str {
        match self {
            Api::Anthropic => "anthropic",
            Api::OpenAi => "openai",
        }
    }
}

#[derive(Debug)]
pub struct Tap {
    api: Api,
    line: Vec<u8>,
    pub model: Option<String>,
    pub usage: Usage,
    pub saw_usage: bool,
    /// Provider-reported cost (OpenRouter `usage.cost`).
    pub cost: Option<f64>,
    pub stop: Option<String>,
    /// When the first event with model output arrived.
    pub first_output: Option<Instant>,
    /// An `error` event inside a 200 stream (e.g. Anthropic `overloaded_error`).
    pub error: Option<String>,
}

fn n(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

impl Tap {
    pub fn new(api: Api) -> Self {
        Self {
            api,
            line: Vec::new(),
            model: None,
            usage: Usage::default(),
            saw_usage: false,
            cost: None,
            stop: None,
            first_output: None,
            error: None,
        }
    }

    /// Feed raw SSE bytes as they arrive (any chunking).
    pub fn feed_sse(&mut self, mut bytes: &[u8]) {
        while let Some(i) = bytes.iter().position(|&b| b == b'\n') {
            self.line.extend_from_slice(&bytes[..i]);
            let line = std::mem::take(&mut self.line);
            self.on_line(&line);
            bytes = &bytes[i + 1..];
        }
        self.line.extend_from_slice(bytes);
    }

    fn on_line(&mut self, line: &[u8]) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(data) = line.strip_prefix(b"data:") else { return };
        let data = data.strip_prefix(b" ").unwrap_or(data);
        if data == b"[DONE]" {
            return;
        }
        if let Ok(v) = serde_json::from_slice::<Value>(data) {
            self.on_event(&v);
        }
    }

    /// A whole (non-streamed) JSON response.
    pub fn feed_json(&mut self, body: &[u8]) {
        let Ok(v) = serde_json::from_slice::<Value>(body) else { return };
        match self.api {
            Api::Anthropic => {
                self.set_model(&v["model"]);
                self.anthropic_usage(&v["usage"]);
                self.set_stop(&v["stop_reason"]);
            }
            Api::OpenAi => self.on_event(&v),
        }
    }

    fn set_model(&mut self, m: &Value) {
        if let Some(m) = m.as_str() {
            self.model = Some(m.to_string());
        }
    }

    fn set_stop(&mut self, s: &Value) {
        if let Some(s) = s.as_str() {
            self.stop = Some(s.to_string());
        }
    }

    /// Anthropic usage objects are cumulative: later events overwrite earlier fields they carry.
    fn anthropic_usage(&mut self, u: &Value) {
        let Some(o) = u.as_object() else { return };
        self.saw_usage = true;
        let set = |k: &str, f: &mut u64| {
            if let Some(v) = o.get(k).filter(|v| !v.is_null()) {
                *f = n(v);
            }
        };
        set("input_tokens", &mut self.usage.input_tokens);
        set("cache_creation_input_tokens", &mut self.usage.cache_write_tokens);
        set("cache_read_input_tokens", &mut self.usage.cache_read_tokens);
        set("output_tokens", &mut self.usage.output_tokens);
    }

    fn on_event(&mut self, v: &Value) {
        match self.api {
            Api::Anthropic => match v["type"].as_str().unwrap_or("") {
                "message_start" => {
                    self.set_model(&v["message"]["model"]);
                    self.anthropic_usage(&v["message"]["usage"]);
                }
                "content_block_start" | "content_block_delta" => {
                    self.first_output.get_or_insert_with(Instant::now);
                }
                "message_delta" => {
                    self.set_stop(&v["delta"]["stop_reason"]);
                    self.anthropic_usage(&v["usage"]);
                }
                "error" => {
                    self.error = Some(v["error"]["message"].as_str().unwrap_or("stream error").to_string());
                }
                _ => {}
            },
            Api::OpenAi => {
                self.set_model(&v["model"]);
                if let Some(c) = v["choices"].get(0) {
                    if !c["delta"].is_null() && c["delta"].as_object().is_some_and(|d| !d.is_empty()) {
                        self.first_output.get_or_insert_with(Instant::now);
                    }
                    self.set_stop(&c["finish_reason"]);
                }
                if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
                    self.error = Some(e["message"].as_str().unwrap_or("stream error").to_string());
                }
                let u = &v["usage"];
                if u.is_object() {
                    // prompt_tokens includes cached and cache-written tokens; wang stores the uncached rest.
                    self.saw_usage = true;
                    let prompt = n(&u["prompt_tokens"]);
                    let cached = n(&u["prompt_tokens_details"]["cached_tokens"]);
                    let written = n(&u["prompt_tokens_details"]["cache_write_tokens"]);
                    self.usage = Usage {
                        input_tokens: prompt.saturating_sub(cached + written),
                        output_tokens: n(&u["completion_tokens"]),
                        cache_write_tokens: written,
                        cache_read_tokens: cached,
                        reasoning_tokens: n(&u["completion_tokens_details"]["reasoning_tokens"]),
                    };
                    self.cost = u["cost"].as_f64();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANTHROPIC_SSE: &str = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-opus-5-5\",\"usage\":{\"input_tokens\":12,\"cache_creation_input_tokens\":300,\"cache_read_input_tokens\":9000,\"output_tokens\":1}}}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":42}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";

    #[test]
    fn anthropic_stream_any_chunking() {
        for size in [1, 7, 64, 10_000] {
            let mut t = Tap::new(Api::Anthropic);
            for c in ANTHROPIC_SSE.as_bytes().chunks(size) {
                t.feed_sse(c);
            }
            assert_eq!(t.model.as_deref(), Some("claude-opus-5-5"));
            assert_eq!(t.usage, Usage { input_tokens: 12, cache_write_tokens: 300, cache_read_tokens: 9000, output_tokens: 42, reasoning_tokens: 0 });
            assert_eq!(t.stop.as_deref(), Some("end_turn"));
            assert!(t.first_output.is_some() && t.error.is_none());
        }
    }

    #[test]
    fn anthropic_error_event_and_json() {
        let mut t = Tap::new(Api::Anthropic);
        t.feed_sse(b"event: error\r\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\r\n\r\n");
        assert_eq!(t.error.as_deref(), Some("Overloaded"));

        let mut t = Tap::new(Api::Anthropic);
        t.feed_json(br#"{"model":"claude-haiku-5-5","stop_reason":"tool_use","usage":{"input_tokens":5,"output_tokens":7}}"#);
        assert_eq!((t.usage.input_tokens, t.usage.output_tokens), (5, 7));
        assert_eq!(t.stop.as_deref(), Some("tool_use"));
    }

    #[test]
    fn openai_stream_usage_and_cost() {
        let sse = concat!(
            ": OPENROUTER PROCESSING\n\n",
            "data: {\"model\":\"anthropic/claude-sonnet-5.5\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hi\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1000,\"completion_tokens\":20,",
            "\"prompt_tokens_details\":{\"cached_tokens\":700,\"cache_write_tokens\":200},\"cost\":0.0042}}\n\n",
            "data: [DONE]\n\n"
        );
        let mut t = Tap::new(Api::OpenAi);
        t.feed_sse(sse.as_bytes());
        assert_eq!(t.model.as_deref(), Some("anthropic/claude-sonnet-5.5"));
        assert_eq!(t.usage.input_tokens, 100);
        assert_eq!(t.usage.cache_read_tokens, 700);
        assert_eq!(t.cost, Some(0.0042));
        assert_eq!(t.stop.as_deref(), Some("stop"));
    }
}

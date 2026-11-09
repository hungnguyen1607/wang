//! Direct Claude API client (raw HTTP; there is no official Rust SDK).
//!
//! Cache-friendly layout: tools (fixed order) -> system (static, explicit breakpoint) ->
//! messages (automatic top-level breakpoint that moves with the conversation).
//! Non-streaming for now: text is delivered to `on_text` when the response arrives.

use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use wang_tokens::Usage;

use super::{CallCtx, MAX_ATTEMPTS, ModelClient, ModelResponse, backoff, http_client, is_retryable};
use crate::config::ModelCfg;

pub struct Anthropic {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    cfg: ModelCfg,
    model: String,
}

const API_VERSION: &str = "2026-10-21";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";

#[derive(Deserialize)]
struct RawUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct RawResponse {
    content: Vec<Value>,
    stop_reason: Option<String>,
    usage: RawUsage,
    model: String,
}

fn supports_fallbacks(model: &str) -> bool {
    ["claude-opus-5-5", "claude-opus-5", "claude-fable-5-1", "claude-sonnet-5-5"].contains(&model)
}

impl Anthropic {
    pub fn new(cfg: &ModelCfg) -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())
            .context("ANTHROPIC_API_KEY is not set (put it in ~/.config/wang/.env)")?;
        let base_url = cfg
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.anthropic.com".into())
            .trim_end_matches('/')
            .to_string();
        Ok(Self { http: http_client()?, api_key, base_url, cfg: cfg.clone(), model: cfg.model_id() })
    }

    pub fn build_body(model: &str, cfg: &ModelCfg, system: &str, tools: &[Value], messages: &[Value]) -> Value {
        let mut body = json!({
            "model": model,
            "max_tokens": cfg.max_tokens,
            "system": [{"type": "text", "text": system, "cache_control": {"type": "ephemeral"}}],
            "messages": messages,
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": cfg.effort},
            "cache_control": {"type": "ephemeral"},
        });
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        if cfg.fallbacks && supports_fallbacks(model) {
            body["fallbacks"] = json!("default");
        }
        body
    }
}

impl ModelClient for Anthropic {
    fn provider(&self) -> &'static str {
        "anthropic"
    }
    fn model(&self) -> &str {
        &self.model
    }

    async fn create(&self, system: &str, tools: &[Value], messages: &[Value], ctx: &CallCtx<'_>) -> Result<ModelResponse> {
        let body = Self::build_body(&self.model, &self.cfg, system, tools, messages);
        let use_fallback = body.get("fallbacks").is_some();
        let body = serde_json::to_string(&body)?;
        let url = format!("{}/v1/messages", self.base_url);

        let mut attempt = 0;
        loop {
            attempt += 1;
            let started = Instant::now();
            let mut req = self
                .http
                .post(&url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", API_VERSION)
                .header("content-type", "application/json")
                .body(body.clone());
            if use_fallback {
                req = req.header("anthropic-beta", FALLBACK_BETA);
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) if attempt < MAX_ATTEMPTS && (e.is_connect() || e.is_timeout()) => {
                    tracing::warn!("model request failed ({e}), retrying");
                    tokio::time::sleep(backoff(attempt, None)).await;
                    continue;
                }
                Err(e) => return Err(e).context("sending model request"),
            };
            let status = resp.status();
            if !status.is_success() {
                let wait = backoff(attempt, resp.headers().get("retry-after"));
                let text = resp.text().await.unwrap_or_default();
                if is_retryable(status) && attempt < MAX_ATTEMPTS {
                    tracing::warn!("Claude API {status}, retrying (attempt {attempt})");
                    tokio::time::sleep(wait).await;
                    continue;
                }
                bail!("Claude API returned {status}: {}", text.chars().take(800).collect::<String>());
            }
            let raw: RawResponse = resp.json().await.context("decoding model response")?;
            let ttft = started.elapsed().as_millis() as u64;
            let text: String = raw.content.iter().filter_map(|b| b.get("text").and_then(Value::as_str)).collect();
            if !text.is_empty() {
                (ctx.on_text)(&text);
            }
            let u = raw.usage;
            return Ok(ModelResponse {
                content: raw.content,
                stop_reason: raw.stop_reason.unwrap_or_else(|| "unknown".into()),
                usage: Usage {
                    input_tokens: u.input_tokens.unwrap_or(0),
                    output_tokens: u.output_tokens.unwrap_or(0),
                    cache_write_tokens: u.cache_creation_input_tokens.unwrap_or(0),
                    cache_read_tokens: u.cache_read_input_tokens.unwrap_or(0),
                    reasoning_tokens: 0,
                },
                model: raw.model,
                cost_usd: None,
                ttft_ms: Some(ttft),
                request_bytes: body.len(),
                ..Default::default()
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_shape() {
        let cfg = ModelCfg { provider: "anthropic".into(), ..Default::default() };
        let tools = vec![json!({"name": "t", "description": "d", "input_schema": {"type": "object"}})];
        let msgs = vec![json!({"role": "user", "content": [{"type": "text", "text": "hi"}]})];
        let b = Anthropic::build_body(&cfg.model_id(), &cfg, "sys", &tools, &msgs);
        assert_eq!(b["model"], "claude-opus-5-5");
        assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(b["cache_control"]["type"], "ephemeral");
        assert_eq!(b["thinking"]["type"], "adaptive");
        assert_eq!(b["output_config"]["effort"], "medium");
        assert_eq!(b["fallbacks"], "default");
        assert!(b.get("temperature").is_none());
        assert!(b.get("tool_choice").is_none());
    }

    #[test]
    fn no_fallbacks_on_unsupported_model() {
        let cfg = ModelCfg::default();
        let b = Anthropic::build_body("claude-haiku-5-5", &cfg, "s", &[], &[]);
        assert!(b.get("fallbacks").is_none());
    }
}

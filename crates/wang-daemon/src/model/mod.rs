//! Model clients. The agent loop only sees [`ModelClient`].
//!
//! The canonical conversation format is Anthropic-style content blocks (`text`, `tool_use`,
//! `tool_result`, `thinking`) stored as raw JSON, because it is the richest; each client
//! translates on the way out and back. Blocks are passed back unchanged (append-only history).

pub mod anthropic;
pub mod cassette;
pub mod local;
pub mod openrouter;

use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Map, Value};
use wang_tokens::Usage;

pub use anthropic::Anthropic;
pub use openrouter::OpenRouter;

use crate::config::ModelCfg;

/// Per-call context.
pub struct CallCtx<'a> {
    /// Conversation id; used for provider sticky routing so the prompt cache stays warm.
    pub session: &'a str,
    /// Called with each piece of reply text as it streams in.
    pub on_text: &'a (dyn Fn(&str) + Send + Sync),
    /// 1-based step within the task; 0 for housekeeping calls (history summaries).
    pub step: u32,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ModelResponse {
    /// Content blocks exactly as they must be sent back.
    pub content: Vec<Value>,
    /// Extra fields to store on the assistant message (e.g. OpenRouter `reasoning_details`).
    pub message_extra: Map<String, Value>,
    /// end_turn | tool_use | max_tokens | refusal | ...
    pub stop_reason: String,
    pub usage: Usage,
    /// Model that actually served the request.
    pub model: String,
    /// Cost reported by the provider, if any.
    pub cost_usd: Option<f64>,
    pub ttft_ms: Option<u64>,
    pub request_bytes: usize,
}

pub trait ModelClient: Send + Sync + 'static {
    fn provider(&self) -> &'static str;
    fn model(&self) -> &str;
    fn create(
        &self,
        system: &str,
        tools: &[Value],
        messages: &[Value],
        ctx: &CallCtx<'_>,
    ) -> impl Future<Output = Result<ModelResponse>> + Send;
}

/// One concrete provider client.
pub enum Provider {
    Anthropic(Anthropic),
    OpenRouter(OpenRouter),
}

impl Provider {
    pub fn from_config(cfg: &ModelCfg) -> Result<Self> {
        Ok(match cfg.provider.as_str() {
            "anthropic" => Provider::Anthropic(Anthropic::new(cfg)?),
            _ => Provider::OpenRouter(OpenRouter::new(cfg)?),
        })
    }
}

impl ModelClient for Provider {
    fn provider(&self) -> &'static str {
        match self {
            Provider::Anthropic(c) => c.provider(),
            Provider::OpenRouter(c) => c.provider(),
        }
    }
    fn model(&self) -> &str {
        match self {
            Provider::Anthropic(c) => c.model(),
            Provider::OpenRouter(c) => c.model(),
        }
    }
    async fn create(&self, system: &str, tools: &[Value], messages: &[Value], ctx: &CallCtx<'_>) -> Result<ModelResponse> {
        match self {
            Provider::Anthropic(c) => c.create(system, tools, messages, ctx).await,
            Provider::OpenRouter(c) => c.create(system, tools, messages, ctx).await,
        }
    }
}

/// Runtime-selected model: one provider, or routed between a strong and a cheap one.
pub enum AnyModel {
    Single(Provider),
    /// Step 1 (planning) goes to `strong`; later tool-loop steps and summaries to `cheap`.
    Routed { strong: Provider, cheap: Provider },
}

impl AnyModel {
    pub fn from_config(cfg: &ModelCfg) -> Result<Self> {
        Ok(AnyModel::Single(Provider::from_config(cfg)?))
    }

    /// `cheap_model` set (and different) → route between the configured model and it.
    pub fn from_config_routed(cfg: &ModelCfg, cheap_model: Option<&str>) -> Result<Self> {
        let strong = Provider::from_config(cfg)?;
        Ok(match cheap_model.filter(|m| !m.is_empty() && *m != cfg.model_id()) {
            Some(m) => {
                let cheap_cfg = ModelCfg { model: Some(m.to_string()), ..cfg.clone() };
                AnyModel::Routed { strong, cheap: Provider::from_config(&cheap_cfg)? }
            }
            None => AnyModel::Single(strong),
        })
    }
}

/// Which side of a router handles a call: `true` = strong.
pub fn route_strong(step: u32) -> bool {
    step == 1
}

impl ModelClient for AnyModel {
    fn provider(&self) -> &'static str {
        match self {
            AnyModel::Single(p) | AnyModel::Routed { strong: p, .. } => p.provider(),
        }
    }
    fn model(&self) -> &str {
        match self {
            AnyModel::Single(p) | AnyModel::Routed { strong: p, .. } => p.model(),
        }
    }
    async fn create(&self, system: &str, tools: &[Value], messages: &[Value], ctx: &CallCtx<'_>) -> Result<ModelResponse> {
        match self {
            AnyModel::Single(p) => p.create(system, tools, messages, ctx).await,
            AnyModel::Routed { strong, cheap } => {
                let target = if route_strong(ctx.step) { strong } else { cheap };
                target.create(system, tools, messages, ctx).await
            }
        }
    }
}

pub(crate) fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        // Generous: long generations stream for minutes. Idle streams are caught by read_timeout.
        .timeout(Duration::from_secs(900))
        .read_timeout(Duration::from_secs(180))
        .build()?)
}

pub(crate) fn is_retryable(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504 | 529)
}

pub(crate) fn backoff(attempt: u32, retry_after: Option<&reqwest::header::HeaderValue>) -> Duration {
    let ra = retry_after.and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    match ra {
        Some(s) => Duration::from_secs(s.min(60)),
        None => Duration::from_millis(500 * 2u64.pow(attempt)),
    }
}

pub(crate) const MAX_ATTEMPTS: u32 = 4;

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::sync::Mutex;

    /// Scripted model: returns canned responses (streaming their text) and records requests.
    pub struct Mock {
        script: Mutex<Vec<ModelResponse>>,
        pub seen: Mutex<Vec<Vec<Value>>>,
    }

    impl Mock {
        pub fn new(mut script: Vec<ModelResponse>) -> Self {
            script.reverse();
            Self { script: Mutex::new(script), seen: Mutex::new(vec![]) }
        }
    }

    impl ModelClient for Mock {
        fn provider(&self) -> &'static str {
            "mock"
        }
        fn model(&self) -> &str {
            "mock-model"
        }
        async fn create(&self, _system: &str, tools: &[Value], messages: &[Value], ctx: &CallCtx<'_>) -> Result<ModelResponse> {
            assert!(tools.is_empty() || tools.len() == 5, "{}", tools.len());
            self.seen.lock().unwrap().push(messages.to_vec());
            let r = self.script.lock().unwrap().pop().expect("mock script exhausted");
            for b in &r.content {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    (ctx.on_text)(t);
                }
            }
            Ok(r)
        }
    }

    pub fn resp(content: Value, stop: &str, cached: u64) -> ModelResponse {
        ModelResponse {
            content: content.as_array().unwrap().clone(),
            stop_reason: stop.into(),
            usage: Usage { input_tokens: 100, output_tokens: 50, cache_read_tokens: cached, ..Default::default() },
            model: "mock-model".into(),
            cost_usd: Some(0.001),
            ttft_ms: Some(5),
            ..Default::default()
        }
    }
}

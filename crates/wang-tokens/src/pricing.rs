use serde::{Deserialize, Serialize};

/// Token usage for one model call, normalized across providers.
///
/// `input_tokens` is the *uncached* remainder only; total prompt size is
/// `input_tokens + cache_write_tokens + cache_read_tokens`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_read_tokens: u64,
    /// Reasoning/thinking tokens; already included in `output_tokens` (informational).
    #[serde(default)]
    pub reasoning_tokens: u64,
}

impl Usage {
    pub fn prompt_tokens(&self) -> u64 {
        self.input_tokens + self.cache_write_tokens + self.cache_read_tokens
    }

    /// Fraction of the prompt served from cache (0.0..=1.0).
    pub fn cache_hit_ratio(&self) -> f64 {
        let p = self.prompt_tokens();
        if p == 0 { 0.0 } else { self.cache_read_tokens as f64 / p as f64 }
    }

    pub fn add(&mut self, o: &Usage) {
        self.input_tokens += o.input_tokens;
        self.output_tokens += o.output_tokens;
        self.cache_write_tokens += o.cache_write_tokens;
        self.cache_read_tokens += o.cache_read_tokens;
        self.reasoning_tokens += o.reasoning_tokens;
    }
}

/// USD per million tokens. `cache_write` is the 5-minute-TTL write price (1.25x input).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
}

/// Anthropic first-party prices (cached 2026-10). Unknown models return `None`.
/// OpenRouter ids (`anthropic/claude-haiku-5.5`) are normalized (`claude-haiku-5-5`).
/// Used only when the provider doesn't report the cost itself.
pub fn price_for(model: &str) -> Option<Price> {
    let normalized;
    let model = match model.strip_prefix("anthropic/") {
        Some(rest) => {
            normalized = rest.split(':').next().unwrap_or(rest).replace('.', "-");
            normalized.as_str()
        }
        None => model,
    };
    let p = |input: f64, output: f64, cache_read: f64| Price {
        input,
        output,
        cache_write: input * 1.25,
        cache_read,
    };
    // Longest/most specific prefixes first.
    let table: &[(&str, Price)] = &[
        ("claude-fable-5-1", p(10.0, 50.0, 0.25)),
        ("claude-mythos-5-1", p(10.0, 50.0, 0.25)),
        ("claude-fable-5", p(10.0, 50.0, 1.0)),
        ("claude-opus-5-5", p(4.0, 20.0, 0.20)),
        ("claude-opus-5", p(5.0, 25.0, 0.50)),
        ("claude-opus-4", p(5.0, 25.0, 0.50)),
        ("claude-sonnet-5-5", p(2.0, 10.0, 0.20)),
        ("claude-sonnet-5", p(2.0, 10.0, 0.20)),
        ("claude-sonnet-4", p(3.0, 15.0, 0.30)),
        ("claude-haiku-5-5", p(0.10, 0.50, 0.01)),
        ("claude-haiku-4-5", p(1.0, 5.0, 0.10)),
    ];
    table.iter().find(|(prefix, _)| model.starts_with(prefix)).map(|(_, p)| *p)
}

/// Cost of one call in USD. Unknown models cost 0.0 (and are flagged by the ledger).
pub fn cost_usd(model: &str, u: &Usage) -> f64 {
    let Some(p) = price_for(model) else { return 0.0 };
    (u.input_tokens as f64 * p.input
        + u.output_tokens as f64 * p.output
        + u.cache_write_tokens as f64 * p.cache_write
        + u.cache_read_tokens as f64 * p.cache_read)
        / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_55_cost() {
        let u = Usage { input_tokens: 1_000_000, output_tokens: 1_000_000, cache_write_tokens: 1_000_000, cache_read_tokens: 1_000_000, reasoning_tokens: 0 };
        let c = cost_usd("claude-opus-5-5", &u);
        assert!((c - (4.0 + 20.0 + 5.0 + 0.20)).abs() < 1e-9, "{c}");
    }

    #[test]
    fn prefix_specificity() {
        assert_eq!(price_for("claude-opus-5-5").unwrap().input, 4.0);
        assert_eq!(price_for("claude-opus-5").unwrap().input, 5.0);
        assert_eq!(price_for("claude-fable-5-1").unwrap().cache_read, 0.25);
        assert!(price_for("gpt-whatever").is_none());
        assert_eq!(price_for("anthropic/claude-haiku-5.5").unwrap().input, 0.10);
        assert_eq!(price_for("anthropic/claude-opus-5.5").unwrap().input, 4.0);
    }

    #[test]
    fn hit_ratio() {
        let u = Usage { input_tokens: 100, cache_read_tokens: 900, ..Default::default() };
        assert!((u.cache_hit_ratio() - 0.9).abs() < 1e-9);
    }
}

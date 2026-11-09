//! Request rewrites: the same token savings wang's own agent gets, applied to someone else's
//! agent loop (Claude Code, opencode, ...).
//!
//! Every rewrite is a pure function of the message it touches and the messages before it. So a
//! message is rewritten the same way on every request of a conversation, and the history the
//! model provider sees stays append-only. That matters twice:
//! - the prompt cache is a prefix match, so a message that changed between requests would
//!   invalidate everything after it;
//! - thinking blocks are bound to the exact prefix that produced them (preserved thinking), and
//!   that prefix is the one the *proxy* sent. Rewriting it identically each time keeps it valid.
//!
//! Rewrites:
//! - **trim**: a huge tool result keeps its head and tail (`wang_tokens::cap_middle`).
//! - **dedup**: a tool result identical to an earlier one in the same conversation is replaced
//!   by a pointer to that earlier call.
//! - **cache hint**: a request with no `cache_control` anywhere gets top-level automatic caching.
//!
//! Assistant messages (and their thinking blocks) are never touched.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};

use serde_json::{Value, json};
use wang_tokens::cap_middle;

#[derive(Debug, Clone, Copy)]
pub struct Rules {
    /// Max chars per tool result (0 = don't trim).
    pub cap_chars: usize,
    pub dedup: bool,
    pub cache_hints: bool,
}

impl Default for Rules {
    fn default() -> Self {
        // Claude Code caps its own tool output at ~30k chars, so 40k only catches outliers
        // (MCP tools, huge reads). wang's own agent uses a tighter cap.
        Self { cap_chars: 40_000, dedup: true, cache_hints: true }
    }
}

/// Outputs shorter than this are never deduped: the pointer wouldn't be much shorter.
const DEDUP_MIN_CHARS: usize = 400;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Edits {
    pub trimmed: u32,
    pub deduped: u32,
    pub cache_hint: bool,
}

impl Edits {
    pub fn changed(&self) -> bool {
        self.trimmed > 0 || self.deduped > 0 || self.cache_hint
    }
}

fn hash(v: &Value) -> u64 {
    let mut h = DefaultHasher::new();
    v.to_string().hash(&mut h);
    h.finish()
}

fn char_len(v: &Value) -> usize {
    match v {
        Value::String(s) => s.chars().count(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).map(|t| t.chars().count()).sum(),
        _ => 0,
    }
}

fn dedup_note(earlier_id: &str) -> String {
    format!("[same output as earlier tool call {earlier_id}; not repeated by wang to save tokens]")
}

/// Trim a tool result's text in place: a plain string, or each text part of an array.
/// Images and other parts are left alone.
fn trim(content: &mut Value, cap: usize) -> bool {
    if cap == 0 {
        return false;
    }
    match content {
        Value::String(s) => {
            let c = cap_middle(s, cap);
            let hit = c.truncated();
            if hit {
                *s = c.text;
            }
            hit
        }
        Value::Array(parts) => {
            let mut hit = false;
            for p in parts.iter_mut() {
                if p["type"] == "text"
                    && let Some(t) = p.get_mut("text")
                    && let Some(s) = t.as_str()
                {
                    let c = cap_middle(s, cap);
                    if c.truncated() {
                        *t = Value::String(c.text);
                        hit = true;
                    }
                }
            }
            hit
        }
        _ => false,
    }
}

/// Dedup + trim one tool result. `seen` maps an output's hash to the first call that produced it.
fn rewrite_result(content: &mut Value, call_id: &str, rules: &Rules, seen: &mut HashMap<u64, String>, e: &mut Edits) {
    if rules.dedup && char_len(content) >= DEDUP_MIN_CHARS {
        let h = hash(content);
        match seen.get(&h) {
            Some(earlier) if earlier != call_id => {
                *content = Value::String(dedup_note(earlier));
                e.deduped += 1;
                return;
            }
            Some(_) => {}
            None => {
                seen.insert(h, call_id.to_string());
            }
        }
    }
    if trim(content, rules.cap_chars) {
        e.trimmed += 1;
    }
}

fn has_cache_control(v: &Value) -> bool {
    match v {
        Value::Object(m) => m.contains_key("cache_control") || m.values().any(has_cache_control),
        Value::Array(a) => a.iter().any(has_cache_control),
        _ => false,
    }
}

/// Anthropic Messages API (`POST /v1/messages`, also `count_tokens`).
pub fn anthropic(body: &mut Value, rules: &Rules) -> Edits {
    let mut e = Edits::default();
    let mut seen = HashMap::new();
    if let Some(msgs) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for m in msgs.iter_mut().filter(|m| m["role"] == "user") {
            let Some(blocks) = m.get_mut("content").and_then(Value::as_array_mut) else { continue };
            for b in blocks.iter_mut().filter(|b| b["type"] == "tool_result") {
                let id = b["tool_use_id"].as_str().unwrap_or("?").to_string();
                if let Some(c) = b.get_mut("content") {
                    rewrite_result(c, &id, rules, &mut seen, &mut e);
                }
            }
        }
    }
    if rules.cache_hints && !has_cache_control(body) {
        body["cache_control"] = json!({"type": "ephemeral"});
        e.cache_hint = true;
    }
    e
}

/// OpenAI-compatible chat completions (OpenRouter and friends).
pub fn openai(body: &mut Value, rules: &Rules) -> Edits {
    let mut e = Edits::default();
    let mut seen = HashMap::new();
    if let Some(msgs) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for m in msgs.iter_mut().filter(|m| m["role"] == "tool") {
            let id = m["tool_call_id"].as_str().unwrap_or("?").to_string();
            if let Some(c) = m.get_mut("content") {
                rewrite_result(c, &id, rules, &mut seen, &mut e);
            }
        }
    }
    // Only Claude models on OpenRouter take `cache_control`; others cache automatically or not at all.
    let claude = body["model"].as_str().is_some_and(|m| m.starts_with("anthropic/"));
    if rules.cache_hints && claude && !has_cache_control(body) {
        body["cache_control"] = json!({"type": "ephemeral"});
        e.cache_hint = true;
    }
    e
}

/// Recovery when the provider says thinking blocks no longer match their conversation (e.g. the
/// proxy's rules changed mid-conversation): drop every thinking block and retry once.
pub fn strip_thinking(body: &mut Value) -> usize {
    let mut n = 0;
    if let Some(msgs) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for m in msgs.iter_mut().filter(|m| m["role"] == "assistant") {
            if let Some(blocks) = m.get_mut("content").and_then(Value::as_array_mut) {
                let before = blocks.len();
                blocks.retain(|b| b["type"] != "thinking" && b["type"] != "redacted_thinking");
                n += before - blocks.len();
            }
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big(c: char, n: usize) -> String {
        std::iter::repeat_n(c, n).collect()
    }

    fn convo(outputs: &[&str]) -> Value {
        let mut messages = vec![json!({"role": "user", "content": "go"})];
        for (i, out) in outputs.iter().enumerate() {
            messages.push(json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "", "signature": format!("sig{i}")},
                {"type": "tool_use", "id": format!("t{i}"), "name": "Read", "input": {}}]}));
            messages.push(json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": format!("t{i}"), "content": out}]}));
        }
        json!({"model": "claude-opus-5-5", "system": "sys", "messages": messages})
    }

    fn results(b: &Value) -> Vec<String> {
        b["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["content"][0]["content"].as_str().map(String::from))
            .collect()
    }

    #[test]
    fn trims_dedups_and_hints() {
        let file = big('a', 1_000);
        let huge = big('x', 50_000);
        let mut b = convo(&[&file, &huge, &file]);
        let rules = Rules { cap_chars: 10_000, ..Default::default() };
        let e = anthropic(&mut b, &rules);
        assert_eq!(e, Edits { trimmed: 1, deduped: 1, cache_hint: true });
        let r = results(&b);
        assert_eq!(r[0], file);
        assert!(r[1].len() < 11_000 && r[1].contains("chars omitted by wang"));
        assert_eq!(r[2], dedup_note("t0"));
        assert_eq!(b["cache_control"]["type"], "ephemeral");
        // Assistant turns, thinking included, are untouched.
        assert_eq!(b["messages"][1]["content"][0]["signature"], "sig0");
    }

    #[test]
    fn rewrites_are_stable_as_the_conversation_grows() {
        // Each request's rewritten history must be a prefix of the next one's (append-only).
        let file = big('a', 1_000);
        let huge = big('x', 50_000);
        let outs = [file.as_str(), huge.as_str(), file.as_str(), "short", huge.as_str()];
        let rules = Rules { cap_chars: 10_000, ..Default::default() };
        let mut prev: Option<Vec<Value>> = None;
        for n in 1..=outs.len() {
            let mut b = convo(&outs[..n]);
            anthropic(&mut b, &rules);
            let msgs = b["messages"].as_array().unwrap().clone();
            if let Some(p) = &prev {
                assert_eq!(&msgs[..p.len()], &p[..], "history changed at step {n}");
            }
            prev = Some(msgs);
        }
    }

    #[test]
    fn existing_cache_control_and_small_outputs_left_alone() {
        let mut b = convo(&["ok", "ok"]);
        b["system"] = json!([{"type": "text", "text": "sys", "cache_control": {"type": "ephemeral"}}]);
        let before = b.clone();
        let e = anthropic(&mut b, &Rules::default());
        assert!(!e.changed());
        assert_eq!(b, before);
    }

    #[test]
    fn array_content_trims_text_parts_only() {
        let mut c = json!([{"type": "text", "text": big('y', 500)}, {"type": "image", "source": {"data": big('z', 5000)}}]);
        assert!(trim(&mut c, 100));
        assert!(c[0]["text"].as_str().unwrap().contains("omitted"));
        assert_eq!(c[1]["source"]["data"].as_str().unwrap().len(), 5000);
    }

    #[test]
    fn openai_tool_messages() {
        let out = big('q', 2_000);
        let mut b = json!({"model": "anthropic/claude-sonnet-5.5", "messages": [
            {"role": "system", "content": "s"},
            {"role": "user", "content": "go"},
            {"role": "assistant", "tool_calls": [{"id": "a"}, {"id": "b"}]},
            {"role": "tool", "tool_call_id": "a", "content": out},
            {"role": "tool", "tool_call_id": "b", "content": out},
        ]});
        let e = openai(&mut b, &Rules::default());
        assert_eq!(e, Edits { trimmed: 0, deduped: 1, cache_hint: true });
        assert_eq!(b["messages"][4]["content"], dedup_note("a"));

        // Non-Claude models get no cache_control.
        let mut b = json!({"model": "openai/gpt-x", "messages": []});
        assert!(!openai(&mut b, &Rules::default()).changed());
    }

    #[test]
    fn strips_thinking_only() {
        let mut b = convo(&["a", "b"]);
        assert_eq!(strip_thinking(&mut b), 2);
        assert_eq!(b["messages"][1]["content"][0]["type"], "tool_use");
    }
}

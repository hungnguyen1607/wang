//! Voice (M13): hands-free use — Telegram voice notes in, a short spoken answer out.
//!
//! - Speech is **cloud, with the user's own key** (OpenAI-compatible endpoints; Groq by config).
//!   No local models, ffmpeg or cmake. Both STT and TTS are plain HTTPS calls from the daemon,
//!   so — like `web_fetch` — no jail is needed and no code runs locally.
//! - Telegram voice notes arrive as OGG/Opus, which OpenAI-style speech-to-text accepts as-is;
//!   text-to-speech can return OGG/Opus, which is exactly what Telegram voice notes use.
//! - The key is part of every request, so errors are stripped of URLs before logging.
//! - Trade-off (documented for the user): the audio goes to the speech provider, billed per
//!   minute (STT) or per character (TTS).

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::json;

/// Guidance appended to the user's message for voice turns. It rides with the message (like
/// workflow guidance) so the cached prefix stays identical.
pub const VOICE_GUIDANCE: &str = "The user is listening on earbuds. Start with a short spoken answer (1–3 plain sentences, no lists, links, markdown or code). If details matter, put them after a line containing only ---.";

/// Runtime speech settings (resolved from `[voice]`).
#[derive(Debug, Clone)]
pub struct VoiceCfg {
    pub enabled: bool,
    pub stt_base: String,
    pub stt_model: String,
    pub tts_base: String,
    pub tts_model: String,
    pub tts_voice: String,
    /// Environment variable that holds the API key.
    pub api_key_env: String,
    /// Refuse voice notes longer than this (seconds).
    pub max_seconds: u32,
    /// Also speak scheduled results delivered to a Telegram chat.
    pub speak_schedules: bool,
}

impl Default for VoiceCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            stt_base: "https://api.openai.com/v1".into(),
            stt_model: "gpt-4o-mini-transcribe".into(),
            tts_base: "https://api.openai.com/v1".into(),
            tts_model: "gpt-4o-mini-tts".into(),
            tts_voice: "alloy".into(),
            api_key_env: "OPENAI_API_KEY".into(),
            max_seconds: 120,
            speak_schedules: false,
        }
    }
}

impl VoiceCfg {
    pub fn api_key(&self) -> Option<String> {
        std::env::var(&self.api_key_env).ok().map(|k| k.trim().to_string()).filter(|k| !k.is_empty())
    }

    /// On with a key, else the reason it's off.
    pub fn status(&self) -> (&'static str, &'static str) {
        if !self.enabled {
            ("off", "set [voice] enabled = true")
        } else if self.api_key().is_none() {
            ("off", "set OPENAI_API_KEY in ~/.config/wang/.env")
        } else {
            ("on", "")
        }
    }
}

pub struct Transcript {
    pub text: String,
    pub ms: u64,
}

/// Speech is a seam: `CloudSpeech` in the daemon, `FakeSpeech` in tests.
pub trait Speech: Send + Sync {
    fn transcribe<'a>(&'a self, audio: Vec<u8>, mime: &'a str) -> Pin<Box<dyn Future<Output = Result<Transcript>> + Send + 'a>>;
    fn speak<'a>(&'a self, text: &'a str) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'a>>;
}

/// OpenAI-compatible speech-to-text and text-to-speech (also Groq's STT, by config).
pub struct CloudSpeech {
    http: reqwest::Client,
    cfg: VoiceCfg,
    key: String,
}

impl CloudSpeech {
    pub fn new(cfg: VoiceCfg) -> Result<Self> {
        let key = cfg.api_key().ok_or_else(|| anyhow!("no {} in the environment", cfg.api_key_env))?;
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).connect_timeout(Duration::from_secs(10)).build()?;
        Ok(Self { http, cfg, key })
    }
}

impl Speech for CloudSpeech {
    fn transcribe<'a>(&'a self, audio: Vec<u8>, mime: &'a str) -> Pin<Box<dyn Future<Output = Result<Transcript>> + Send + 'a>> {
        Box::pin(async move {
            let started = std::time::Instant::now();
            let part = reqwest::multipart::Part::bytes(audio)
                .file_name("voice.ogg")
                .mime_str(if mime.is_empty() { "audio/ogg" } else { mime })
                .map_err(|e| anyhow!("voice stt: {e}"))?;
            let form = reqwest::multipart::Form::new()
                .part("file", part)
                .text("model", self.cfg.stt_model.clone())
                .text("language", "en");
            let resp = self
                .http
                .post(format!("{}/audio/transcriptions", self.cfg.stt_base.trim_end_matches('/')))
                .bearer_auth(&self.key)
                .multipart(form)
                .send()
                .await
                .map_err(|e| anyhow!("voice stt: {}", e.without_url()))?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| anyhow!("voice stt: {}", e.without_url()))?;
            if !status.is_success() {
                bail!("voice stt: HTTP {status}: {}", clip_body(&body));
            }
            let text = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v["text"].as_str().map(str::to_string))
                .filter(|t| !t.trim().is_empty())
                .context("voice stt: no transcript returned")?;
            Ok(Transcript { text, ms: started.elapsed().as_millis() as u64 })
        })
    }

    fn speak<'a>(&'a self, text: &'a str) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'a>> {
        Box::pin(async move {
            let body = json!({
                "model": self.cfg.tts_model,
                "voice": self.cfg.tts_voice,
                "input": text,
                "response_format": "opus",
            });
            let resp = self
                .http
                .post(format!("{}/audio/speech", self.cfg.tts_base.trim_end_matches('/')))
                .bearer_auth(&self.key)
                .json(&body)
                .send()
                .await
                .map_err(|e| anyhow!("voice tts: {}", e.without_url()))?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                bail!("voice tts: HTTP {status}: {}", clip_body(&body));
            }
            resp.bytes().await.map(|b| b.to_vec()).map_err(|e| anyhow!("voice tts: {}", e.without_url()))
        })
    }
}

fn clip_body(s: &str) -> String {
    let mut out: String = s.chars().take(200).collect();
    if s.chars().count() > 200 {
        out.push('…');
    }
    out
}

/// Split a reply into the short part to speak and the details to send as text.
/// A line containing only `---` marks the boundary; without it, the first ~3 sentences are
/// spoken and the rest becomes details. Returns `(spoken, details)`.
pub fn spoken_part(reply: &str) -> (String, Option<String>) {
    let mut spoken = String::new();
    let mut details = String::new();
    let mut in_details = false;
    for line in reply.lines() {
        // A `---` line is the separator, but only once something has been said — a leading
        // `---` is a Markdown horizontal rule, not a split.
        if !in_details && line.trim() == "---" && !spoken.trim().is_empty() {
            in_details = true;
            continue;
        }
        if in_details {
            details.push_str(line);
            details.push('\n');
        } else {
            spoken.push_str(line);
            spoken.push('\n');
        }
    }
    if in_details {
        return (clean_speech(&spoken), Some(details.trim().to_string()));
    }
    let (head, rest) = first_sentences(reply, 3);
    let spoken = clean_speech(&head);
    let details = rest.trim();
    (spoken, (!details.is_empty()).then(|| details.to_string()))
}

/// The first `n` sentences of `s` (and the remainder, unchanged).
fn first_sentences(s: &str, n: usize) -> (String, String) {
    let mut count = 0;
    let mut cut = s.len();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let boundary = matches!(c, b'.' | b'!' | b'?');
        if boundary {
            // Sentence end: followed by whitespace/end, and not a decimal like "3.14".
            let prev_digit = i > 0 && bytes[i - 1].is_ascii_digit();
            let next = bytes.get(i + 1).copied();
            let next_digit = next.is_some_and(|b| b.is_ascii_digit());
            if !(prev_digit && next_digit) && next.is_none_or(|b| b.is_ascii_whitespace()) {
                count += 1;
                if count == n {
                    cut = i + 1;
                    break;
                }
            }
        }
        i += 1;
    }
    (s[..cut].to_string(), s[cut..].to_string())
}

/// Strip markdown, links, code and list markers so a reply reads well when heard.
pub fn clean_speech(s: &str) -> String {
    // Drop fenced code blocks entirely.
    let mut out = String::new();
    let mut in_fence = false;
    for line in s.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    // Inline code spans: drop the backticks.
    let s = out.replace('`', "");
    // Markdown links: [text](url) -> text.
    let s = remove_links(&s);
    // URLs and emphasis/list markers, per word.
    let mut words = Vec::new();
    for word in s.split_whitespace() {
        let w = word.trim_matches(|c: char| matches!(c, '*' | '_' | '~'));
        let w = w.trim_start_matches(['#', '>', '-', '•', '+']);
        if w.is_empty() || looks_like_url(w) {
            continue;
        }
        words.push(w);
    }
    words.join(" ")
}

fn looks_like_url(w: &str) -> bool {
    w.starts_with("http://") || w.starts_with("https://") || w.starts_with("www.")
}

fn remove_links(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('[') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(close) = rest.find(']') else {
            out.push_str(rest);
            return out;
        };
        if rest[close + 1..].starts_with('(') && let Some(end) = rest[close + 1..].find(')') {
            let end = close + 1 + end;
            out.push_str(&rest[1..close]); // the link text
            rest = &rest[end + 1..];
            continue;
        }
        out.push_str(&rest[..close + 1]);
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

/// Whole-message "yes"/"no" (used to answer a pending approval by voice or text).
pub fn approval_intent(text: &str) -> Option<bool> {
    let t = text.trim().trim_matches(|c: char| matches!(c, '.' | '!' | '?' | ',')).to_ascii_lowercase();
    if t.is_empty() {
        return None;
    }
    const YES: &[&str] = &["yes", "yeah", "yep", "y", "approve", "approved", "go ahead", "do it", "sure", "ok", "okay", "proceed", "accept"];
    const NO: &[&str] = &["no", "nope", "nah", "n", "deny", "denied", "stop", "cancel", "don't", "dont", "reject"];
    if YES.contains(&t.as_str()) {
        return Some(true);
    }
    if NO.contains(&t.as_str()) {
        return Some(false);
    }
    None
}

#[cfg(test)]
mod tests;

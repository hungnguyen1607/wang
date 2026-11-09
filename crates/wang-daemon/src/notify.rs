//! Push notifications through ntfy (https://ntfy.sh or self-hosted).
//! Used for loop alerts now; approvals (M4) will reuse it.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::config::NtfyCfg;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Notice {
    pub title: String,
    pub message: String,
    /// 1 (min) .. 5 (max); 3 is default.
    pub priority: u8,
    pub tags: Vec<String>,
    /// Adds Approve / Deny buttons that answer this approval request.
    pub approval_id: Option<String>,
}

pub trait Notify: Send + Sync {
    fn send<'a>(&'a self, n: &'a Notice) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
}

/// Sends to every notifier; succeeds if at least one did.
pub struct Fanout(pub Vec<std::sync::Arc<dyn Notify>>);

impl Notify for Fanout {
    fn send<'a>(&'a self, n: &'a Notice) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let results = futures::future::join_all(self.0.iter().map(|x| x.send(n))).await;
            let errors: Vec<String> = results.into_iter().filter_map(|r| r.err().map(|e| format!("{e:#}"))).collect();
            if !errors.is_empty() && errors.len() == self.0.len() {
                bail!("all notifiers failed: {}", errors.join("; "));
            }
            for e in &errors {
                tracing::warn!("a notifier failed: {e}");
            }
            Ok(())
        })
    }
}

pub struct Ntfy {
    http: reqwest::Client,
    server: String,
    topic: String,
    reply_topic: String,
    token: Option<String>,
}

impl Ntfy {
    /// `None` when no topic is configured (alerts off).
    pub fn from_config(cfg: &NtfyCfg) -> Result<Option<Self>> {
        let topic = cfg.topic.trim();
        if topic.is_empty() {
            return Ok(None);
        }
        let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build()?;
        Ok(Some(Self {
            http,
            server: cfg.server.trim_end_matches('/').to_string(),
            topic: topic.to_string(),
            reply_topic: if cfg.reply_topic.trim().is_empty() { format!("{topic}-reply") } else { cfg.reply_topic.trim().to_string() },
            token: std::env::var("NTFY_TOKEN").ok().filter(|t| !t.is_empty()),
        }))
    }

    pub fn describe(&self) -> String {
        format!("{}/{}", self.server, self.topic)
    }

    /// Listen on the reply topic forever, passing each message body to `on_message`.
    /// Reconnects with backoff. Only messages published after we connect are seen.
    pub async fn listen(&self, on_message: impl Fn(&str)) {
        let url = format!("{}/{}/json", self.server, self.reply_topic);
        let client = reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).build().expect("http client");
        let mut backoff = 1u64;
        loop {
            let mut req = client.get(&url);
            if let Some(t) = &self.token {
                req = req.bearer_auth(t);
            }
            match req.send().await {
                Ok(mut resp) if resp.status().is_success() => {
                    tracing::info!("listening for approvals on {}", self.reply_topic);
                    backoff = 1;
                    let mut buf = Vec::new();
                    while let Ok(Some(chunk)) = resp.chunk().await {
                        buf.extend_from_slice(&chunk);
                        while let Some(i) = buf.iter().position(|b| *b == b'\n') {
                            let line: Vec<u8> = buf.drain(..=i).collect();
                            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&line) {
                                if v["event"] == "message" {
                                    if let Some(m) = v["message"].as_str() {
                                        on_message(m);
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(resp) => tracing::warn!("ntfy reply topic: HTTP {}", resp.status()),
                Err(e) => tracing::warn!("ntfy reply topic: {e}"),
            }
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(60);
        }
    }

    fn body(&self, n: &Notice) -> serde_json::Value {
        let mut body = json!({
            "topic": self.topic,
            "title": n.title,
            "message": n.message,
            "priority": n.priority,
            "tags": n.tags,
        });
        if let Some(id) = &n.approval_id {
            let url = format!("{}/{}", self.server, self.reply_topic);
            let mut headers = serde_json::Map::new();
            if let Some(t) = &self.token {
                headers.insert("Authorization".into(), json!(format!("Bearer {t}")));
            }
            body["actions"] = json!([
                {"action": "http", "label": "Approve", "url": url, "method": "POST", "body": format!("approve {id}"), "headers": headers, "clear": true},
                {"action": "http", "label": "Deny", "url": url, "method": "POST", "body": format!("deny {id}"), "headers": headers, "clear": true},
            ]);
        }
        body
    }
}

impl Notify for Ntfy {
    fn send<'a>(&'a self, n: &'a Notice) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            // JSON publishing (POST to the server root) handles UTF-8 titles cleanly.
            let mut req = self.http.post(&self.server).json(&self.body(n));
            if let Some(t) = &self.token {
                req = req.bearer_auth(t);
            }
            let resp = req.send().await.context("sending ntfy notification")?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                bail!("ntfy returned {status}: {}", body.chars().take(300).collect::<String>());
            }
            Ok(())
        })
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct Recorder(pub Mutex<Vec<Notice>>);

    impl Notify for Recorder {
        fn send<'a>(&'a self, n: &'a Notice) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
            self.0.lock().unwrap().push(n.clone());
            Box::pin(async { Ok(()) })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_buttons_post_to_reply_topic() {
        let cfg = NtfyCfg { server: "https://ntfy.example".into(), topic: "t1".into(), reply_topic: String::new() };
        let n = Ntfy::from_config(&cfg).unwrap().unwrap();
        let b = n.body(&Notice { title: "x".into(), approval_id: Some("abc".into()), ..Default::default() });
        assert_eq!(b["actions"][0]["url"], "https://ntfy.example/t1-reply");
        assert_eq!(b["actions"][0]["body"], "approve abc");
        assert_eq!(b["actions"][1]["body"], "deny abc");
        let plain = n.body(&Notice { title: "x".into(), ..Default::default() });
        assert!(plain.get("actions").is_none());
    }
}

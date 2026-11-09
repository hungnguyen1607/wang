//! Approvals: privileged tool calls wait for your yes/no.
//!
//! Answers can come from:
//! - your phone: the ntfy notification has Approve / Deny buttons that post
//!   `approve <id>` / `deny <id>` to a reply topic the daemon listens on;
//! - the terminal: `wang chat` asks inline, or `wang approve <id>` / `wang deny <id>`.
//!
//! Ids are 128-bit random, so only someone who saw the request can answer it.
//! No answer before the timeout counts as "deny".

use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

use crate::notify::{Notice, Notify};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approved,
    Denied,
    TimedOut,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Approved => "approved",
            Decision::Denied => "denied",
            Decision::TimedOut => "timed_out",
        }
    }
}

pub struct Approvals {
    pending: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    notifier: Option<Arc<dyn Notify>>,
    pub timeout: Duration,
}

pub fn new_id() -> String {
    let mut b = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut b);
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl Approvals {
    pub fn new(notifier: Option<Arc<dyn Notify>>, timeout: Duration) -> Self {
        Self { pending: Mutex::new(HashMap::new()), notifier, timeout }
    }

    /// Register a request; returns (id, receiver). Call `wait` with the receiver.
    pub fn open(&self) -> (String, oneshot::Receiver<bool>) {
        let id = new_id();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.clone(), tx);
        (id, rx)
    }

    /// Push the request to the phone (if ntfy is configured).
    pub async fn notify(&self, id: &str, task_id: i64, what: &str) {
        let Some(n) = &self.notifier else { return };
        let notice = Notice {
            title: format!("wang: approve task #{task_id}?"),
            message: what.to_string(),
            priority: 4,
            tags: vec!["question".into()],
            approval_id: Some(id.to_string()),
        };
        if let Err(e) = tokio::time::timeout(Duration::from_secs(10), n.send(&notice)).await.unwrap_or_else(|_| Err(anyhow::anyhow!("timed out"))) {
            tracing::warn!("approval notification failed: {e:#}");
        }
    }

    pub async fn wait(&self, id: &str, rx: oneshot::Receiver<bool>) -> (Decision, Duration) {
        let started = Instant::now();
        let d = match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(true)) => Decision::Approved,
            Ok(Ok(false)) => Decision::Denied,
            Ok(Err(_)) => Decision::Denied,
            Err(_) => Decision::TimedOut,
        };
        self.pending.lock().unwrap().remove(id);
        (d, started.elapsed())
    }

    /// Answer a pending request. Returns false if the id is unknown (or already answered).
    pub fn resolve(&self, id: &str, allow: bool) -> bool {
        match self.pending.lock().unwrap().remove(id.trim()) {
            Some(tx) => tx.send(allow).is_ok(),
            None => false,
        }
    }

    /// Parse "approve <id>" / "deny <id>" (from the ntfy reply topic).
    pub fn handle_reply(&self, text: &str) -> Option<(String, bool)> {
        let mut it = text.split_whitespace();
        let allow = match it.next()?.to_ascii_lowercase().as_str() {
            "approve" | "yes" => true,
            "deny" | "no" => false,
            _ => return None,
        };
        let id = it.next()?.to_string();
        self.resolve(&id, allow).then_some((id, allow))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn approve_deny_timeout() {
        let a = Arc::new(Approvals::new(None, Duration::from_millis(200)));

        let (id, rx) = a.open();
        assert_eq!(id.len(), 32);
        let a2 = a.clone();
        let id2 = id.clone();
        tokio::spawn(async move { assert!(a2.resolve(&id2, true)) });
        assert_eq!(a.wait(&id, rx).await.0, Decision::Approved);
        assert!(!a.resolve(&id, true), "can't answer twice");

        let (id, rx) = a.open();
        assert_eq!(a.handle_reply(&format!("deny {id}")), Some((id.clone(), false)));
        assert_eq!(a.wait(&id, rx).await.0, Decision::Denied);

        let (id, rx) = a.open();
        assert_eq!(a.wait(&id, rx).await.0, Decision::TimedOut);
        assert_eq!(a.handle_reply("approve not-a-real-id"), None);
        assert_eq!(a.handle_reply("hello"), None);
    }
}

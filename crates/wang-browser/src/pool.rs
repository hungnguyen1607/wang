//! A pool of jailed browsers, one per agent session: capped in number and in RAM each,
//! idle ones closed, and refused when the machine is short on memory.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use tokio::sync::Mutex;
use wang_jail::Jail;

use crate::{Browser, BrowserCfg};

pub type Lease = Arc<Mutex<Browser>>;

struct Slot {
    browser: Lease,
    last_used: Instant,
}

#[derive(Debug, Clone)]
pub struct PoolStatus {
    pub session: String,
    pub memory_bytes: Option<u64>,
    pub idle: Duration,
}

pub struct Pool {
    jail: Arc<Jail>,
    pub cfg: BrowserCfg,
    slots: Mutex<HashMap<String, Slot>>,
    launches: std::sync::atomic::AtomicU64,
}

/// System MemAvailable, in bytes.
pub fn mem_available() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb: u64 = s.lines().find(|l| l.starts_with("MemAvailable:"))?.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

impl Pool {
    pub fn new(jail: Arc<Jail>, cfg: BrowserCfg) -> Arc<Self> {
        Arc::new(Self { jail, cfg, slots: Mutex::new(HashMap::new()), launches: Default::default() })
    }

    /// The session's browser, launching one if needed (and if there's room).
    pub async fn get(&self, session: &str) -> Result<Lease> {
        let mut slots = self.slots.lock().await;
        if let Some(slot) = slots.get_mut(session) {
            slot.last_used = Instant::now();
            let lease = slot.browser.clone();
            let alive = match lease.try_lock() {
                Ok(mut b) => b.alive(),
                Err(_) => true, // in use, so alive enough
            };
            if alive {
                return Ok(lease);
            }
            tracing::warn!(session, "browser died; starting a new one");
            if let Some(dead) = slots.remove(session) {
                close_later(dead.browser);
            }
        }
        if slots.len() >= self.cfg.max_browsers {
            // Close the least recently used browser that nobody is using right now.
            let idle = slots
                .iter()
                .filter(|(_, s)| Arc::strong_count(&s.browser) == 1)
                .min_by_key(|(_, s)| s.last_used)
                .map(|(k, _)| k.clone());
            match idle {
                Some(k) => {
                    tracing::info!(evicted = %k, "browser pool full; closing the least recently used browser");
                    if let Some(s) = slots.remove(&k) {
                        close_later(s.browser);
                    }
                }
                None => bail!("all {} browsers are busy; try again shortly", self.cfg.max_browsers),
            }
        }
        if let Some(avail) = mem_available()
            && avail < self.cfg.min_free_bytes
        {
            bail!(
                "not starting a browser: only {} MiB of memory free (need {} MiB free)",
                avail >> 20,
                self.cfg.min_free_bytes >> 20
            );
        }
        let b = Browser::launch(&self.jail, &self.cfg).await?;
        self.launches.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::info!(session, ms = b.launch_ms as u64, version = %b.version, "browser started");
        let lease = Arc::new(Mutex::new(b));
        slots.insert(session.to_string(), Slot { browser: lease.clone(), last_used: Instant::now() });
        Ok(lease)
    }

    /// Close one session's browser (e.g. on `/new`).
    pub async fn release(&self, session: &str) {
        if let Some(s) = self.slots.lock().await.remove(session) {
            close_later(s.browser);
        }
    }

    /// Close browsers idle longer than `idle_timeout`. Returns how many.
    pub async fn reap(&self) -> usize {
        let mut slots = self.slots.lock().await;
        let stale: Vec<String> = slots
            .iter()
            .filter(|(_, s)| s.last_used.elapsed() > self.cfg.idle_timeout && Arc::strong_count(&s.browser) == 1)
            .map(|(k, _)| k.clone())
            .collect();
        for k in &stale {
            if let Some(s) = slots.remove(k) {
                close_later(s.browser);
            }
        }
        stale.len()
    }

    /// Background task: reap idle browsers every 30 s.
    pub fn spawn_reaper(self: &Arc<Self>) {
        let pool = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let Some(p) = pool.upgrade() else { return };
                let n = p.reap().await;
                if n > 0 {
                    tracing::info!(closed = n, "closed idle browsers");
                }
            }
        });
    }

    pub async fn status(&self) -> Vec<PoolStatus> {
        let slots = self.slots.lock().await;
        let mut v: Vec<PoolStatus> = slots
            .iter()
            .map(|(k, s)| PoolStatus {
                session: k.clone(),
                memory_bytes: s.browser.try_lock().ok().and_then(|b| b.memory_bytes()),
                idle: s.last_used.elapsed(),
            })
            .collect();
        v.sort_by(|a, b| a.session.cmp(&b.session));
        v
    }

    pub fn launches(&self) -> u64 {
        self.launches.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub async fn shutdown(&self) {
        let all: Vec<Slot> = self.slots.lock().await.drain().map(|(_, s)| s).collect();
        for s in all {
            if let Ok(m) = Arc::try_unwrap(s.browser) {
                m.into_inner().close().await;
            }
        }
    }
}

/// Close a browser without holding up the caller (once its last user lets go).
fn close_later(lease: Lease) {
    tokio::spawn(async move {
        // Wait until whoever holds it is done.
        let mut lease = lease;
        loop {
            match Arc::try_unwrap(lease) {
                Ok(m) => {
                    m.into_inner().close().await;
                    return;
                }
                Err(back) => {
                    lease = back;
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    });
}

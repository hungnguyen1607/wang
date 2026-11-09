//! One headless Chrome inside its own jail, with one page, driven over the DevTools pipe.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use wang_jail::run::{Running, Stopped};
use wang_jail::{Jail, Limits, Policy};

use crate::cdp::{Cdp, Event};
use crate::{BrowserCfg, find_chrome};

const VIEW_JS: &str = include_str!("view.js");

/// Scroll a ref into view and return its center, or null if it's gone.
const BOX_JS: &str = r#"(n) => {
  const el = (window.__wangRefs || [])[n - 1];
  if (!el || !el.isConnected) return null;
  el.scrollIntoView({block: "center", inline: "center"});
  const r = el.getBoundingClientRect();
  if (r.width === 0 && r.height === 0) return {hidden: true};
  return {x: r.left + r.width / 2, y: r.top + r.height / 2};
}"#;

const FOCUS_JS: &str = r#"(n) => {
  const el = (window.__wangRefs || [])[n - 1];
  if (!el || !el.isConnected) return "gone";
  el.scrollIntoView({block: "center"});
  el.focus();
  if (typeof el.select === "function" && el.tagName !== "SELECT") el.select();
  else if (el.isContentEditable) document.execCommand("selectAll");
  return document.activeElement === el || el.contains(document.activeElement) ? "ok" : "not focusable";
}"#;

const SELECT_JS: &str = r#"(n, want) => {
  const el = (window.__wangRefs || [])[n - 1];
  if (!el || !el.isConnected) return {error: "gone"};
  if (el.tagName !== "SELECT") return {error: "not a select"};
  const w = want.trim().toLowerCase();
  const opts = [...el.options];
  const o = opts.find(o => o.text.trim().toLowerCase() === w || o.value.toLowerCase() === w)
         || opts.find(o => o.text.trim().toLowerCase().includes(w));
  if (!o) return {error: "no option matching '" + want + "'; options: " + opts.map(o => o.text.trim()).join(" | ")};
  el.value = o.value;
  el.dispatchEvent(new Event("input", {bubbles: true}));
  el.dispatchEvent(new Event("change", {bubbles: true}));
  return {chosen: o.text.trim()};
}"#;

/// The compact view of the current page.
#[derive(Debug, Clone, Default)]
pub struct View {
    pub text: String,
    pub refs: usize,
    /// Size of the page's HTML and of its plain text, to show what the view saves.
    pub html_bytes: usize,
    pub text_bytes: usize,
}

/// Chrome flags: headless, no Chrome sandbox (the jail is the sandbox, and nested user
/// namespaces are disabled in it), nothing phoning home, DevTools over the pipe only.
fn chrome_args(bin: &str, cfg: &BrowserCfg) -> Vec<String> {
    let mut a: Vec<String> = [
        bin,
        "--headless",
        "--no-sandbox",
        "--remote-debugging-pipe",
        "--user-data-dir=/tmp/wang-profile",
        "--disable-dev-shm-usage",
        "--disable-gpu",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-extensions",
        "--disable-component-update",
        "--disable-background-networking",
        "--disable-sync",
        "--disable-default-apps",
        "--disable-breakpad",
        "--metrics-recording-only",
        "--password-store=basic",
        "--use-mock-keychain",
        "--mute-audio",
        "--hide-scrollbars",
        "--disable-features=Translate,MediaRouter,OptimizationHints,AutofillServerCommunication,InterestFeedContentSuggestions,CalculateNativeWinOcclusion",
        // Fewer renderer processes: the jail, not site isolation, is the boundary here.
        "--renderer-process-limit=2",
        "--disable-site-isolation-trials",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if !cfg.domains.is_empty() {
        // The jail's egress forwarder; `<-loopback>` sends localhost through it too, where the
        // allow-list decides (the jail's own loopback has nothing on it).
        a.push(format!("--proxy-server=http://127.0.0.1:{}", wang_jail::helper::PROXY_PORT));
        a.push("--proxy-bypass-list=<-loopback>".into());
    }
    a.extend(cfg.extra_args.iter().cloned());
    a.push("about:blank".into());
    a
}

pub struct Browser {
    running: Running,
    cdp: Arc<Cdp>,
    session: String,
    frame: String,
    crashed: Arc<AtomicBool>,
    cfg: BrowserCfg,
    /// Time from spawn to a ready page.
    pub launch_ms: f64,
    pub version: String,
}

impl Browser {
    pub async fn launch(jail: &Jail, cfg: &BrowserCfg) -> Result<Self> {
        let started = Instant::now();
        let bin = match &cfg.binary {
            Some(b) => b.clone(),
            None => find_chrome()?,
        };
        let bin = std::fs::canonicalize(&bin).with_context(|| format!("browser binary {}", bin.display()))?;
        let dir = bin.parent().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
        let mut read = vec![];
        if !dir.starts_with("/usr") {
            read.push(dir);
        }
        let policy = Policy {
            read,
            domains: cfg.domains.clone(),
            limits: Limits {
                memory_bytes: cfg.memory_bytes,
                cpus: cfg.cpus,
                // Chrome counts threads against pids.max: dozens per process.
                pids: 2048,
                timeout: Duration::from_secs(24 * 3600),
                tmp_bytes: 512 << 20,
                max_output_bytes: 4 << 20,
            },
            ..Default::default()
        };
        let argv = chrome_args(&bin.to_string_lossy(), cfg);
        let mut running = jail.spawn_piped(&policy, &argv).await.context("starting the browser jail")?;
        let (r, w) = (running.stdout.take().context("browser pipe")?, running.stdin.take().context("browser pipe")?);
        let cdp = Cdp::new(r, w);

        let version = match cdp.call_timeout(None, "Browser.getVersion", json!({}), Duration::from_secs(20)).await {
            Ok(v) => v["product"].as_str().unwrap_or("?").to_string(),
            Err(e) => {
                let stopped = running.stop().await;
                bail!("browser didn't start: {e}{}", stderr_tail(&stopped));
            }
        };
        let crashed = Arc::new(AtomicBool::new(false));
        let mut b = Self { running, cdp, session: String::new(), frame: String::new(), crashed, cfg: cfg.clone(), launch_ms: 0.0, version };
        b.cdp.call(None, "Browser.setDownloadBehavior", json!({"behavior": "deny"})).await.ok();
        b.new_page().await?;
        b.launch_ms = started.elapsed().as_secs_f64() * 1e3;
        Ok(b)
    }

    /// Open a fresh tab (also used to recover from a crashed renderer).
    async fn new_page(&mut self) -> Result<()> {
        let t = self.cdp.call(None, "Target.createTarget", json!({"url": "about:blank"})).await?;
        let target = t["targetId"].as_str().context("no targetId")?.to_string();
        let s = self.cdp.call(None, "Target.attachToTarget", json!({"targetId": target, "flatten": true})).await?;
        self.session = s["sessionId"].as_str().context("no sessionId")?.to_string();
        self.frame = target.clone();
        self.crashed.store(false, Ordering::Relaxed);
        let sid = Some(self.session.as_str());
        self.cdp.call(sid, "Page.enable", json!({})).await?;
        self.cdp.call(sid, "Page.setLifecycleEventsEnabled", json!({"enabled": true})).await?;
        self.cdp
            .call(sid, "Emulation.setDeviceMetricsOverride", json!({"width": 1280, "height": 900, "deviceScaleFactor": 1, "mobile": false}))
            .await?;
        // Headless Chrome announces itself in its user agent; some sites refuse it.
        let ua = self.cdp.call(None, "Browser.getVersion", json!({})).await?["userAgent"].as_str().unwrap_or("").replace("HeadlessChrome", "Chrome");
        if !ua.is_empty() {
            self.cdp.call(sid, "Emulation.setUserAgentOverride", json!({"userAgent": ua})).await.ok();
        }

        // Dialogs (alert/confirm/beforeunload) would block the page: dismiss them. Note crashes.
        let mut ev = self.cdp.subscribe();
        let (cdp, session, crashed) = (self.cdp.clone(), self.session.clone(), self.crashed.clone());
        tokio::spawn(async move {
            loop {
                match ev.recv().await {
                    Ok(e) if e.session.as_deref() == Some(&session) => match e.method.as_str() {
                        "Page.javascriptDialogOpening" => {
                            let _ = cdp.call(Some(&session), "Page.handleJavaScriptDialog", json!({"accept": false})).await;
                        }
                        "Inspector.targetCrashed" => crashed.store(true, Ordering::Relaxed),
                        "Inspector.detached" => return,
                        _ => {}
                    },
                    Ok(e) if e.method == "wang.closed" => return,
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => return,
                }
            }
        });
        Ok(())
    }

    fn sid(&self) -> Option<&str> {
        Some(self.session.as_str())
    }

    /// Ready for a command: fails if the browser is gone, reopens the tab if it crashed.
    async fn ready(&mut self) -> Result<()> {
        if self.cdp.is_closed() {
            return Err(self.gone());
        }
        if self.crashed.load(Ordering::Relaxed) {
            self.new_page().await?;
            bail!("the page crashed{} and was reset to a blank tab; open the URL again", self.oom_hint());
        }
        Ok(())
    }

    fn gone(&self) -> anyhow::Error {
        anyhow!("the browser is gone ({}){}", self.cdp.closed_reason().unwrap_or_else(|| "no reason recorded".into()), self.oom_hint())
    }

    fn oom_hint(&self) -> String {
        match self.running.memory_bytes() {
            Some(m) if m + (64 << 20) >= self.cfg.memory_bytes => format!(" (it hit the {} MiB browser memory cap)", self.cfg.memory_bytes >> 20),
            _ => String::new(),
        }
    }

    pub fn alive(&mut self) -> bool {
        !self.cdp.is_closed() && self.running.try_exit_code().is_none()
    }

    pub fn memory_bytes(&self) -> Option<u64> {
        self.running.memory_bytes()
    }

    pub fn blocked_hosts(&self) -> Vec<String> {
        self.running.blocked_hosts()
    }

    /// Evaluate a JavaScript expression in the page and return its JSON value.
    pub async fn eval(&self, expr: &str) -> Result<Value> {
        let r = self.cdp.call(self.sid(), "Runtime.evaluate", json!({"expression": expr, "returnByValue": true})).await?;
        if let Some(ex) = r.get("exceptionDetails") {
            let msg = ex["exception"]["description"].as_str().or(ex["text"].as_str()).unwrap_or("script error");
            bail!("page script error: {}", msg.lines().next().unwrap_or(msg));
        }
        Ok(r["result"]["value"].clone())
    }

    /// Wait for the main frame's load event, then briefly for the network to go quiet.
    async fn wait_load(&self, ev: &mut broadcast::Receiver<Event>, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut loaded = false;
        loop {
            let limit = if loaded { deadline.min(tokio::time::Instant::now() + Duration::from_millis(1500)) } else { deadline };
            let e = match tokio::time::timeout_at(limit, ev.recv()).await {
                Err(_) if loaded => return Ok(()),
                Err(_) => bail!("page load timed out after {:.0}s", timeout.as_secs_f64()),
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(_)) => bail!("browser is gone"),
                Ok(Ok(e)) => e,
            };
            if e.method == "wang.closed" {
                return Err(self.gone());
            }
            if e.session.as_deref() != self.sid() {
                continue;
            }
            match e.method.as_str() {
                "Page.loadEventFired" => loaded = true,
                "Page.lifecycleEvent" if e.params["frameId"] == self.frame.as_str() && e.params["name"] == "networkAlmostIdle" && loaded => {
                    return Ok(());
                }
                "Inspector.targetCrashed" => bail!("the page crashed{}", self.oom_hint()),
                _ => {}
            }
        }
    }

    /// After a click or key press: if it starts a navigation, wait for the new page.
    async fn settle(&self, ev: &mut broadcast::Receiver<Event>) -> Result<()> {
        let quick = tokio::time::Instant::now() + Duration::from_millis(500);
        loop {
            match tokio::time::timeout_at(quick, ev.recv()).await {
                Err(_) => return Ok(()), // no navigation: give scripts their moment and go on
                Ok(Ok(e)) if e.session.as_deref() == self.sid() => {
                    let main = e.params["frameId"] == self.frame.as_str() || e.params["frame"]["id"] == self.frame.as_str();
                    if main && matches!(e.method.as_str(), "Page.frameStartedNavigating" | "Page.frameStartedLoading" | "Page.frameRequestedNavigation") {
                        return self.wait_load(ev, self.cfg.nav_timeout).await;
                    }
                }
                Ok(Ok(e)) if e.method == "wang.closed" => return Err(self.gone()),
                Ok(Err(broadcast::error::RecvError::Closed)) => bail!("browser is gone"),
                Ok(_) => {}
            }
        }
    }

    pub async fn open(&mut self, url: &str) -> Result<()> {
        self.ready().await?;
        let parsed = reqwest_like_scheme(url)?;
        let blocked_before = self.blocked_hosts().len();
        let mut ev = self.cdp.subscribe();
        let r = self.cdp.call_timeout(self.sid(), "Page.navigate", json!({"url": parsed}), self.cfg.nav_timeout).await?;
        if let Some(err) = r["errorText"].as_str().filter(|s| !s.is_empty()) {
            let blocked = self.blocked_hosts();
            if !blocked.is_empty() {
                bail!("{err}: the sandbox blocked {} (not in the browser's domain allow-list)", blocked.join(", "));
            }
            bail!("{err} loading {url}");
        }
        if r.get("loaderId").is_none() {
            return Ok(()); // same-document navigation (#fragment)
        }
        self.wait_load(&mut ev, self.cfg.nav_timeout).await?;
        // Plain-http pages refused by the egress proxy load as its 403 page, not as an error.
        if let Some(hp) = host_port(&parsed) {
            let blocked = self.blocked_hosts();
            if blocked.len() > blocked_before && blocked.iter().any(|b| b == &hp || b.starts_with(&format!("{hp} "))) {
                bail!("the sandbox blocked {hp} (not in the browser's domain allow-list)");
            }
        }
        Ok(())
    }

    pub async fn view(&mut self, max_chars: usize) -> Result<View> {
        self.ready().await?;
        let v = self.eval(&format!("({VIEW_JS})({max_chars})")).await?;
        Ok(View {
            text: v["text"].as_str().unwrap_or("").to_string(),
            refs: v["refs"].as_u64().unwrap_or(0) as usize,
            html_bytes: v["html_bytes"].as_u64().unwrap_or(0) as usize,
            text_bytes: v["text_bytes"].as_u64().unwrap_or(0) as usize,
        })
    }

    pub async fn click(&mut self, n: usize) -> Result<()> {
        self.ready().await?;
        let b = self.eval(&format!("({BOX_JS})({n})")).await?;
        if b.is_null() {
            bail!("element [{n}] is not on the page any more; call view to get fresh numbers");
        }
        if b["hidden"] == true {
            bail!("element [{n}] has no size (hidden); try another element");
        }
        let (x, y) = (b["x"].as_f64().unwrap_or(0.0), b["y"].as_f64().unwrap_or(0.0));
        let mut ev = self.cdp.subscribe();
        for (kind, buttons) in [("mouseMoved", 0), ("mousePressed", 1), ("mouseReleased", 0)] {
            let mut p = json!({"type": kind, "x": x, "y": y, "buttons": buttons});
            if kind != "mouseMoved" {
                p["button"] = json!("left");
                p["clickCount"] = json!(1);
            }
            self.cdp.call(self.sid(), "Input.dispatchMouseEvent", p).await?;
        }
        self.settle(&mut ev).await
    }

    pub async fn type_text(&mut self, n: usize, text: &str, submit: bool) -> Result<()> {
        self.ready().await?;
        match self.eval(&format!("({FOCUS_JS})({n})")).await?.as_str() {
            Some("ok") => {}
            Some("gone") => bail!("element [{n}] is not on the page any more; call view to get fresh numbers"),
            other => bail!("element [{n}] can't be typed into ({})", other.unwrap_or("?")),
        }
        self.cdp.call(self.sid(), "Input.insertText", json!({"text": text})).await?;
        if !submit {
            return Ok(());
        }
        let mut ev = self.cdp.subscribe();
        for kind in ["keyDown", "keyUp"] {
            let mut p = json!({"type": kind, "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "nativeVirtualKeyCode": 13});
            if kind == "keyDown" {
                p["text"] = json!("\r");
            }
            self.cdp.call(self.sid(), "Input.dispatchKeyEvent", p).await?;
        }
        self.settle(&mut ev).await
    }

    pub async fn select(&mut self, n: usize, option: &str) -> Result<String> {
        self.ready().await?;
        let r = self.eval(&format!("({SELECT_JS})({n}, {})", json!(option))).await?;
        if let Some(e) = r["error"].as_str() {
            bail!("select [{n}]: {e}");
        }
        Ok(r["chosen"].as_str().unwrap_or("").to_string())
    }

    pub async fn back(&mut self) -> Result<()> {
        self.ready().await?;
        let h = self.cdp.call(self.sid(), "Page.getNavigationHistory", json!({})).await?;
        let i = h["currentIndex"].as_u64().unwrap_or(0) as usize;
        if i == 0 {
            bail!("no previous page");
        }
        let id = h["entries"][i - 1]["id"].clone();
        let mut ev = self.cdp.subscribe();
        self.cdp.call(self.sid(), "Page.navigateToHistoryEntry", json!({"entryId": id})).await?;
        self.wait_load(&mut ev, self.cfg.nav_timeout).await.or_else(|e| {
            // Pages restored from the back/forward cache fire no load event.
            if e.to_string().contains("timed out") { Ok(()) } else { Err(e) }
        })
    }

    pub async fn url(&self) -> Result<String> {
        Ok(self.eval("location.href").await?.as_str().unwrap_or("").to_string())
    }

    /// Kill the browser and its jail.
    pub async fn close(self) -> Stopped {
        let _ = self.cdp.call_timeout(None, "Browser.close", json!({}), Duration::from_secs(2)).await;
        self.running.stop().await
    }
}

fn stderr_tail(s: &Stopped) -> String {
    let text = String::from_utf8_lossy(&s.stderr);
    let useful: Vec<&str> = text.lines().filter(|l| !l.contains("WARNING") && !l.trim().is_empty()).collect();
    let tail = useful[useful.len().saturating_sub(5)..].join("\n");
    let mut out = String::new();
    if s.usage.oom_kills > 0 {
        out.push_str(" (killed: out of memory under the browser memory cap)");
    }
    if !tail.is_empty() {
        out.push_str(&format!("\n{tail}"));
    }
    out
}

/// `host:port` of an http(s) URL, as the egress proxy records it.
fn host_port(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let auth = rest.split(['/', '?', '#']).next()?;
    let auth = auth.rsplit('@').next()?.to_ascii_lowercase();
    let default = if scheme.eq_ignore_ascii_case("https") { 443 } else { 80 };
    if auth.starts_with('[') {
        return Some(if auth.contains("]:") { auth } else { format!("{auth}:{default}") });
    }
    Some(if auth.contains(':') { auth } else { format!("{auth}:{default}") })
}

/// Only web pages: no file://, chrome://, javascript: or data: navigation.
fn reqwest_like_scheme(url: &str) -> Result<String> {
    let u = url.trim();
    let lower = u.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower == "about:blank" {
        return Ok(u.to_string());
    }
    if !lower.contains("://") && !lower.contains(':') && lower.contains('.') {
        return Ok(format!("https://{u}"));
    }
    Err(anyhow!("only http:// and https:// URLs can be opened (got '{u}')"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemes() {
        assert_eq!(reqwest_like_scheme("example.com/x").unwrap(), "https://example.com/x");
        assert_eq!(reqwest_like_scheme("http://a.b").unwrap(), "http://a.b");
        for bad in ["file:///etc/passwd", "chrome://settings", "javascript:alert(1)", "data:text/html,hi", "localhost:8080"] {
            assert!(reqwest_like_scheme(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn host_ports() {
        assert_eq!(host_port("http://Example.com/x?y").unwrap(), "example.com:80");
        assert_eq!(host_port("https://a.b:8443#f").unwrap(), "a.b:8443");
        assert_eq!(host_port("http://u:p@127.0.0.1:9/").unwrap(), "127.0.0.1:9");
    }

    #[test]
    fn args_pipe_no_port() {
        let a = chrome_args("/c/chrome", &BrowserCfg { domains: vec!["*".into()], ..Default::default() });
        assert!(a.contains(&"--remote-debugging-pipe".to_string()));
        assert!(!a.iter().any(|x| x.starts_with("--remote-debugging-port")));
        assert!(a.contains(&"--proxy-server=http://127.0.0.1:3128".to_string()));
        let a = chrome_args("/c/chrome", &BrowserCfg { domains: vec![], ..Default::default() });
        assert!(!a.iter().any(|x| x.starts_with("--proxy-server")));
    }
}

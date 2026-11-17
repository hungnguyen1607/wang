//! `wang login`: a visible Chrome window where you sign in to your sites once. When you're done,
//! its cookies are saved to wang's own login file, and every headless browser after that starts
//! signed in. The window uses a throwaway profile; only the login file is kept.
//!
//! The window runs inside wang's jail, with only your display socket added.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use wang_jail::{Bind, Jail, Limits, Policy};

use crate::browser::chrome_args;
use crate::cdp::Cdp;
use crate::logins::{LoginSource, hosts_of, save_jar};
use crate::BrowserCfg;

/// A Chrome that can open a window: `WANG_CHROME_HEADED`, Playwright's full Chromium (same
/// version as its headless shell, so logins carry over), then system installs (not snaps).
pub fn find_headed_chrome() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("WANG_CHROME_HEADED") {
        return Ok(PathBuf::from(p));
    }
    let cache = dirs::cache_dir().unwrap_or_default().join("ms-playwright");
    let mut found: Vec<PathBuf> = std::fs::read_dir(&cache)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("chromium-")))
        .map(|p| p.join("chrome-linux64/chrome"))
        .filter(|p| p.is_file())
        .collect();
    found.sort();
    if let Some(p) = found.pop() {
        return Ok(p);
    }
    for p in ["/usr/bin/google-chrome", "/usr/bin/google-chrome-stable", "/usr/bin/chromium-browser", "/usr/bin/chromium"] {
        let p = PathBuf::from(p);
        if p.is_file() && !std::fs::read_link(&p).is_ok_and(|t| t.to_string_lossy().contains("snap")) {
            return Ok(p);
        }
    }
    bail!("no Chrome that can open a window: run `npx playwright install chromium` (or set WANG_CHROME_HEADED)")
}

/// Turn "youtube.com" into "https://youtube.com/".
pub fn start_url(s: &str) -> String {
    let s = s.trim();
    if s.starts_with("http://") || s.starts_with("https://") { s.to_string() } else { format!("https://{s}/") }
}

async fn snapshot(cdp: &Cdp) -> Option<Vec<Value>> {
    cdp.call_timeout(None, "Storage.getCookies", json!({}), Duration::from_secs(5)).await.ok().and_then(|r| r["cookies"].as_array().cloned())
}

pub struct LoginResult {
    /// Cookies now in the login file.
    pub saved: usize,
    /// Sites signed in during this window.
    pub sites: Vec<String>,
}

/// Open the window on `urls` (with the existing logins loaded), wait for `done` or for the
/// window to close, then save the cookies.
///
/// The window runs in wang's jail like the headless browser (Ubuntu blocks Chrome's own sandbox
/// outside it): same egress proxy and allow-list, a throwaway profile on the jail's tmpfs, and
/// only the Wayland display socket added so it can draw a window.
pub async fn login_window(jail: &Jail, cfg: &BrowserCfg, urls: &[String], done: impl std::future::Future<Output = ()>) -> Result<LoginResult> {
    let logins = cfg.logins.as_ref().context("no [browser] logins configured")?;
    let LoginSource::Jar(jar) = &logins.source else {
        bail!("`wang login` saves to wang's own login file; set [browser] logins = \"wang\"")
    };
    let chrome = std::fs::canonicalize(find_headed_chrome()?)?;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).context("XDG_RUNTIME_DIR isn't set (no desktop session?)")?;
    let display = std::env::var("WAYLAND_DISPLAY").ok().filter(|d| !d.is_empty()).context(
        "no Wayland display: run `wang login` from a terminal in your desktop session",
    )?;
    let socket = if display.starts_with('/') { PathBuf::from(&display) } else { runtime.join(&display) };
    if !socket.exists() {
        bail!("Wayland socket {} not found", socket.display());
    }
    let policy = Policy {
        read: vec![chrome.parent().map(PathBuf::from).unwrap_or_default()],
        domains: cfg.domains.clone(),
        binds: vec![Bind { src: socket, dst: PathBuf::from("/run/wang-display/wayland-0"), writable: true }],
        env: vec![
            ("WAYLAND_DISPLAY".into(), "/run/wang-display/wayland-0".into()),
            ("XDG_RUNTIME_DIR".into(), "/tmp".into()),
        ],
        limits: Limits {
            memory_bytes: cfg.memory_bytes.max(2 << 30),
            cpus: cfg.cpus,
            pids: 4096,
            timeout: Duration::from_secs(2 * 3600),
            tmp_bytes: 1 << 30,
            max_output_bytes: 1 << 20,
        },
        ..Default::default()
    };
    let mut argv: Vec<String> = chrome_args(&chrome.to_string_lossy(), cfg)
        .into_iter()
        .filter(|a| a != "--headless" && a != "--hide-scrollbars" && a != "--mute-audio" && a != "about:blank")
        .collect();
    argv.extend(["--ozone-platform=wayland".to_string(), "--window-size=1200,900".into(), "about:blank".into()]);
    let mut running = jail.spawn_piped(&policy, &argv).await.context("starting the login window in the jail")?;
    let (r, w) = (running.stdout.take().context("chrome pipe")?, running.stdin.take().context("chrome pipe")?);
    let cdp = Cdp::new(r, w);
    let result = async {
        cdp.call_timeout(None, "Browser.getVersion", json!({}), Duration::from_secs(30)).await.context("Chrome didn't start")?;
        let existing = logins.load()?;
        let known = hosts_of(&existing);
        if !existing.is_empty() {
            cdp.call(None, "Storage.setCookies", json!({"cookies": existing})).await?;
        }
        for u in urls {
            cdp.call(None, "Target.createTarget", json!({"url": start_url(u)})).await?;
        }
        // Keep a recent copy of the cookies, so closing the window instead of pressing Enter still saves.
        let mut last: Option<Vec<Value>> = None;
        tokio::pin!(done);
        loop {
            tokio::select! {
                _ = &mut done => {
                    if let Some(c) = snapshot(&cdp).await {
                        last = Some(c);
                    }
                    break;
                }
                _ = tokio::time::sleep(Duration::from_secs(3)) => {
                    if cdp.is_closed() {
                        break;
                    }
                    if let Some(c) = snapshot(&cdp).await {
                        last = Some(c);
                    }
                }
            }
        }
        let cookies = last.context("no cookies read from the window (it closed before it finished starting?)")?;
        // The window is where you signed in: it's authoritative for every site it saw.
        let mut replace = hosts_of(&cookies);
        replace.extend(known.iter().cloned());
        let saved = save_jar(jar, &cookies, &logins.sites, &replace)?;
        let sites: Vec<String> = hosts_of(&cookies).into_iter().filter(|h| !known.contains(h)).collect();
        Ok(LoginResult { saved, sites })
    }
    .await;
    let _ = cdp.call_timeout(None, "Browser.close", json!({}), Duration::from_secs(3)).await;
    let stopped = running.stop().await;
    if result.is_err() {
        let err = String::from_utf8_lossy(&stopped.stderr);
        let tail: Vec<&str> = err.lines().filter(|l| !l.trim().is_empty()).collect();
        if !tail.is_empty() {
            return result.with_context(|| tail[tail.len().saturating_sub(4)..].join("\n"));
        }
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_urls() {
        assert_eq!(start_url("youtube.com"), "https://youtube.com/");
        assert_eq!(start_url(" https://www.instagram.com/accounts/login/ "), "https://www.instagram.com/accounts/login/");
    }
}

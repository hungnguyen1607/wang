//! Desktop control: open apps, read their windows, click and type, for the `desktop` tool.
//!
//! **Accessibility first.** A Python helper (`helper.py`, embedded here; python3-gi ships with
//! Ubuntu/GNOME) walks a window's AT-SPI tree and returns a compact view with numbered
//! elements, the same idea as the browser's page view. Clicking and typing target one element,
//! never "whatever has focus".
//!
//! **Pixels second.** Screenshots, clicks at x,y and key presses use the xdg-desktop-portal
//! RemoteDesktop session. GNOME asks once (`wang setup screen`, at the laptop); after that the
//! saved permission is used and the dialog never appears while you're away.
//!
//! Guardrails ([`guard`]) decide which actions need your approval first.

pub mod apps;
pub mod guard;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

pub const HELPER: &str = include_str!("../helper.py");

struct Helper {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
}

/// The desktop helper, started on first use and restarted if it dies.
pub struct Desktop {
    dir: PathBuf,
    helper: Mutex<Option<Helper>>,
    next: AtomicU64,
    /// Which profile wang last opened per app (lowercased app name), for approval messages.
    pub profiles: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

/// A screenshot: JPEG bytes and the screen size it was taken at.
pub struct Shot {
    pub jpeg: Vec<u8>,
    pub screen: Option<(u64, u64)>,
}

impl Desktop {
    /// `dir`: where the helper and its saved portal permission live (hidden from the sandbox).
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, helper: Mutex::new(None), next: AtomicU64::new(1), profiles: Default::default() }
    }

    pub fn default_dir() -> PathBuf {
        dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("wang/desktop")
    }

    /// Is there a desktop to drive? (a session bus and a graphical session)
    pub fn available() -> Result<()> {
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() && std::env::var_os("XDG_RUNTIME_DIR").is_none() {
            bail!("no desktop session (no session bus)");
        }
        let ok = std::process::Command::new("python3")
            .args(["-I", "-c", "import gi; gi.require_version('Atspi','2.0'); from gi.repository import Atspi"])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            bail!("python3 with the AT-SPI bindings is needed: sudo apt install python3-gi gir1.2-atspi-2.0");
        }
        Ok(())
    }

    /// Has the one-time screen-control permission been given?
    pub fn screen_ready(&self) -> bool {
        self.dir.join("portal-restore-token").exists()
    }

    async fn spawn(&self) -> Result<Helper> {
        std::fs::create_dir_all(&self.dir)?;
        let script = self.dir.join("helper.py");
        if std::fs::read_to_string(&script).ok().as_deref() != Some(HELPER) {
            std::fs::write(&script, HELPER)?;
        }
        let mut child = tokio::process::Command::new("python3")
            .args(["-I", "-W", "ignore"])
            .arg(&script)
            .env("WANG_DESKTOP_STATE", &self.dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context("starting the desktop helper (python3)")?;
        let stdin = child.stdin.take().context("helper stdin")?;
        let lines = BufReader::new(child.stdout.take().context("helper stdout")?).lines();
        Ok(Helper { child, stdin, lines })
    }

    /// One request. Ops: windows, view, info, click, type, select, scroll, apps, open, key,
    /// screenshot, click_at, portal, window_info.
    pub async fn call(&self, op: &str, mut args: Value, timeout: Duration) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        args["id"] = json!(id);
        args["op"] = json!(op);
        let mut guard = self.helper.lock().await;
        for attempt in 0..2 {
            if guard.as_mut().is_none_or(|h| h.child.try_wait().ok().flatten().is_some()) {
                *guard = Some(self.spawn().await?);
            }
            let h = guard.as_mut().unwrap();
            let line = format!("{args}\n");
            if h.stdin.write_all(line.as_bytes()).await.is_err() {
                *guard = None;
                continue;
            }
            let _ = h.stdin.flush().await;
            match tokio::time::timeout(timeout, h.lines.next_line()).await {
                Ok(Ok(Some(l))) => {
                    let v: Value = serde_json::from_str(&l).context("bad reply from the desktop helper")?;
                    if v["ok"] == true {
                        return Ok(v["result"].clone());
                    }
                    bail!("{}", v["error"].as_str().unwrap_or("desktop helper error"));
                }
                Ok(_) if attempt == 0 => {
                    *guard = None; // it died: one restart
                }
                Ok(_) => bail!("the desktop helper stopped"),
                Err(_) => {
                    // Stuck (e.g. an app not answering): drop it, the next call starts fresh.
                    *guard = None;
                    bail!("the desktop didn't answer within {}s", timeout.as_secs());
                }
            }
        }
        bail!("the desktop helper keeps failing")
    }

    pub async fn screenshot(&self, max_width: u32) -> Result<Shot> {
        let r = self.call("screenshot", json!({"max_width": max_width}), Duration::from_secs(25)).await?;
        let path = PathBuf::from(r["path"].as_str().context("no screenshot path")?);
        let jpeg = std::fs::read(&path)?;
        let _ = std::fs::remove_file(&path);
        let screen = r["screen"].as_array().and_then(|a| Some((a.first()?.as_u64()?, a.get(1)?.as_u64()?)));
        Ok(Shot { jpeg, screen })
    }

    /// `wang setup screen`: ask GNOME for the screen-control permission (shows its dialog).
    pub async fn grant_screen(&self) -> Result<Value> {
        self.call("portal", json!({}), Duration::from_secs(130)).await
    }

    /// Open `app` on the profile/folder the user named. Returns (app, profile, helper result).
    pub async fn open_profile(&self, app: &str, profile: &str) -> Result<(String, apps::Profile, Value)> {
        let (app_name, p) = apps::resolve(app, profile)?;
        let label = format!("{app_name} ({})", p.name);
        let r = self.call("launch", json!({"argv": p.argv, "label": label}), Duration::from_secs(25)).await?;
        // Remember it under the app's process-ish names, so approvals can say which profile.
        let key = app_name.to_lowercase();
        let mut m = self.profiles.lock().unwrap();
        for k in [key.clone(), key.replace(' ', ""), key.split(' ').next_back().unwrap_or(&key).to_string()] {
            m.insert(k, p.name.clone());
        }
        if let Some(w) = r["window"]["app"].as_str() {
            m.insert(w.to_lowercase(), p.name.clone());
        }
        Ok((app_name.to_string(), p, r))
    }

    /// The profile wang opened for this app (by its window's app name), if any.
    pub fn profile_of(&self, app: &str) -> Option<String> {
        let a = app.to_lowercase();
        let m = self.profiles.lock().unwrap();
        m.get(&a).cloned().or_else(|| m.iter().find(|(k, _)| a.contains(k.as_str()) || k.contains(&a)).map(|(_, v)| v.clone()))
    }

    pub async fn shutdown(&self) {
        *self.helper.lock().await = None;
    }
}

pub fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

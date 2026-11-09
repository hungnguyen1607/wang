//! wang-browser: headless Chrome for the agent, inside wang's sandbox.
//!
//! ```text
//! agent ── browser tool ──► Pool (one browser per session, N max, RAM cap each, idle reaping)
//!                              └─ Browser: jail (cgroup RAM/CPU/pids, namespaces, seccomp)
//!                                   └─ chrome --remote-debugging-pipe  ◄── own CDP client (fd 3/4)
//!                                        └─ all traffic ─► jail egress proxy ─► domain allow-list
//! ```
//!
//! The agent never sees HTML: [`Browser::view`] renders the page as compact text with numbered
//! links, buttons and fields, and actions refer to those numbers.

pub mod browser;
pub mod cdp;
pub mod pool;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};

pub use browser::{Browser, View};
pub use pool::{Lease, Pool, PoolStatus};

#[derive(Debug, Clone)]
pub struct BrowserCfg {
    /// Chrome / Chromium / chrome-headless-shell; found automatically when unset.
    pub binary: Option<PathBuf>,
    /// Network allow-list for pages (same syntax as the jail). Private addresses need an exact entry.
    pub domains: Vec<String>,
    /// RAM cap per browser (cgroup, no swap).
    pub memory_bytes: u64,
    pub cpus: f64,
    pub max_browsers: usize,
    pub idle_timeout: Duration,
    /// Don't launch a browser when the system has less than this free.
    pub min_free_bytes: u64,
    pub nav_timeout: Duration,
    /// Characters per view part (~4 per token).
    pub view_chars: usize,
    pub extra_args: Vec<String>,
}

impl Default for BrowserCfg {
    fn default() -> Self {
        Self {
            binary: None,
            domains: vec!["*".into()],
            memory_bytes: 1 << 30,
            cpus: 2.0,
            max_browsers: 2,
            idle_timeout: Duration::from_secs(300),
            min_free_bytes: 768 << 20,
            nav_timeout: Duration::from_secs(30),
            view_chars: 12_000,
            extra_args: vec![],
        }
    }
}

/// Find a Chrome that can run in the jail: `WANG_CHROME`, Playwright's headless shell or
/// Chromium, then system installs. Snap packages are skipped (they can't run inside bubblewrap).
pub fn find_chrome() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("WANG_CHROME") {
        return Ok(PathBuf::from(p));
    }
    let home = dirs::home_dir().unwrap_or_default();
    let pw = home.join(".cache/ms-playwright");
    let mut found: Vec<(u32, u8, PathBuf)> = vec![];
    if let Ok(rd) = std::fs::read_dir(&pw) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let (rank, rel) = if let Some(v) = name.strip_prefix("chromium_headless_shell-") {
                (v, "chrome-headless-shell-linux64/chrome-headless-shell")
            } else if let Some(v) = name.strip_prefix("chromium-") {
                (v, "chrome-linux64/chrome")
            } else {
                continue;
            };
            let p = e.path().join(rel);
            if p.is_file() {
                // Prefer the headless shell (lighter), then the newest build.
                let pref = if rel.starts_with("chrome-headless-shell") { 1 } else { 0 };
                found.push((rank.parse().unwrap_or(0), pref, p));
            }
        }
    }
    found.sort_by(|a, b| (b.1, b.0).cmp(&(a.1, a.0)));
    if let Some((_, _, p)) = found.into_iter().next() {
        return Ok(p);
    }
    for p in ["/usr/bin/chromium", "/usr/bin/chromium-browser", "/usr/bin/google-chrome-stable", "/usr/bin/google-chrome", "/opt/google/chrome/chrome"] {
        let path = PathBuf::from(p);
        if let Ok(real) = std::fs::canonicalize(&path)
            && real.is_file()
            && !real.starts_with("/snap")
        {
            return Ok(real);
        }
    }
    bail!("no usable Chrome found: install one with `npx playwright install chromium-headless-shell`, or set [browser] binary (snap Chromium can't run inside the sandbox)")
}

/// Split a view into parts of about `size` chars on line boundaries; returns (part `n`, total).
/// The first line pair (title, URL) is repeated on every part.
pub fn view_part(text: &str, n: usize, size: usize) -> (String, usize) {
    let mut head_end = 0;
    for _ in 0..2 {
        head_end += text[head_end..].find('\n').map(|i| i + 1).unwrap_or(text.len() - head_end);
    }
    let (head, body) = text.split_at(head_end);
    let mut parts: Vec<String> = vec![];
    let mut cur = String::new();
    for line in body.lines() {
        if !cur.is_empty() && cur.len() + line.len() + 1 > size {
            parts.push(std::mem::take(&mut cur));
        }
        // A single giant line is cut on a char boundary.
        let mut l = line;
        while l.len() > size {
            let mut cut = size;
            while !l.is_char_boundary(cut) {
                cut -= 1;
            }
            parts.push(l[..cut].to_string());
            l = &l[cut..];
        }
        cur.push_str(l);
        cur.push('\n');
    }
    if !cur.trim().is_empty() || parts.is_empty() {
        parts.push(cur);
    }
    let total = parts.len();
    let i = n.clamp(1, total) - 1;
    (format!("{head}{}", parts[i].trim_end()), total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_keep_header_and_lines() {
        let body: String = (0..100).map(|i| format!("line {i:03} {}\n", "x".repeat(40))).collect();
        let text = format!("Title\nhttps://e.test/\n\n{body}");
        let (p1, total) = view_part(&text, 1, 1000);
        assert!(total > 4);
        assert!(p1.starts_with("Title\nhttps://e.test/\n"));
        assert!(p1.contains("line 000") && !p1.contains("line 099"));
        let (last, _) = view_part(&text, total, 1000);
        assert!(last.starts_with("Title\n") && last.contains("line 099"));
        // Every line appears exactly once across parts.
        let all: String = (1..=total).map(|n| view_part(&text, n, 1000).0).collect();
        for i in 0..100 {
            assert_eq!(all.matches(&format!("line {i:03} ")).count(), 1, "line {i}");
        }
        assert_eq!(view_part("T\nU\n\nshort", 5, 1000), ("T\nU\n\nshort".to_string(), 1));
    }

    #[test]
    fn finds_some_chrome_or_explains() {
        match find_chrome() {
            Ok(p) => assert!(!p.starts_with("/snap")),
            Err(e) => assert!(e.to_string().contains("playwright")),
        }
    }
}

#[cfg(test)]
mod e2e;

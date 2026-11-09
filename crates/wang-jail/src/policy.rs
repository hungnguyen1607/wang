//! What a jailed command may touch: files, network domains and resource limits.
//! A policy comes from a manifest (TOML), config, CLI flags, or code.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Secrets hidden from every jail, even when a parent directory is visible.
/// Directories are covered by an empty tmpfs, files by /dev/null.
pub const ALWAYS_HIDDEN: &[&str] = &[
    "~/.ssh",
    "~/.gnupg",
    "~/.aws",
    "~/.azure",
    "~/.config/gcloud",
    "~/.kube",
    "~/.docker",
    "~/.password-store",
    "~/.netrc",
    "~/.git-credentials",
    "~/.npmrc",
    "~/.pypirc",
    "~/.cargo/credentials",
    "~/.cargo/credentials.toml",
    "~/.config/gh",
    "~/.config/wang",
    "~/.local/share/wang",
    "~/.local/share/keyrings",
    "~/.mozilla",
    "~/.config/google-chrome",
    "~/.config/chromium",
    "~/.config/BraveSoftware",
    "~/.config/1Password",
    "~/.thunderbird",
    "~/.bash_history",
    "~/.zsh_history",
    "~/.python_history",
    "~/.psql_history",
    "~/.mysql_history",
    "~/.lesshst",
    "~/.viminfo",
];

#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    /// Visible read-only.
    pub read: Vec<PathBuf>,
    /// Visible read-write (writes reach the real files).
    pub write: Vec<PathBuf>,
    /// Visible copy-on-write: writes land in a throwaway layer and vanish afterwards.
    pub overlay: Vec<PathBuf>,
    /// Hidden on top of `ALWAYS_HIDDEN`.
    pub hide: Vec<PathBuf>,
    /// Network allow-list (`example.com`, `*.example.com`, `example.com:8443`, `*`). Empty = no network.
    pub domains: Vec<String>,
    pub limits: Limits,
    /// Working directory inside the jail (must be visible; falls back to /tmp).
    pub cwd: Option<PathBuf>,
    /// Extra environment variables.
    pub env: Vec<(String, String)>,
    /// Host paths mounted at a different path inside the jail (e.g. one VM's run directory at `/vm`).
    pub binds: Vec<Bind>,
    /// Device nodes passed through (e.g. `/dev/kvm`).
    pub devices: Vec<PathBuf>,
    /// Let the kernel merge identical memory pages of this jail's processes (KSM,
    /// `PR_SET_MEMORY_MERGE`). For VMs: many copies of the same guest share pages.
    pub ksm: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Bind {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub writable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub memory_bytes: u64,
    pub cpus: f64,
    pub pids: u32,
    pub timeout: Duration,
    pub tmp_bytes: u64,
    /// Max bytes of stdout and of stderr kept (the rest is drained and dropped).
    pub max_output_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_bytes: 512 << 20,
            cpus: 1.0,
            pids: 128,
            timeout: Duration::from_secs(60),
            tmp_bytes: 256 << 20,
            max_output_bytes: 4 << 20,
        }
    }
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            read: vec![],
            write: vec![],
            overlay: vec![],
            hide: vec![],
            domains: vec![],
            limits: Limits::default(),
            cwd: None,
            env: vec![],
            binds: vec![],
            devices: vec![],
            ksm: false,
        }
    }
}

pub fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/nonexistent"))
}

pub fn expand(p: &str) -> PathBuf {
    if p == "~" {
        return home();
    }
    match p.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(p),
    }
}

/// "512M", "1G", "64k", "1048576" → bytes.
pub fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('k' | 'K') => (&s[..s.len() - 1], 1u64 << 10),
        Some('m' | 'M') => (&s[..s.len() - 1], 1 << 20),
        Some('g' | 'G') => (&s[..s.len() - 1], 1 << 30),
        _ => (s, 1),
    };
    let n: f64 = num.trim().parse().with_context(|| format!("bad size '{s}'"))?;
    if n < 0.0 {
        bail!("negative size '{s}'");
    }
    Ok((n * mult as f64) as u64)
}

/// "30s", "2m", "500ms", "45" (seconds) → Duration.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let (num, unit) = if let Some(n) = s.strip_suffix("ms") {
        (n, 0.001)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1.0)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60.0)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600.0)
    } else {
        (s, 1.0)
    };
    let n: f64 = num.trim().parse().with_context(|| format!("bad duration '{s}'"))?;
    Ok(Duration::from_secs_f64(n * unit))
}

/// Does `host:port` match the allow-list?
pub fn domain_allowed(patterns: &[String], host: &str, port: u16) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    patterns.iter().any(|p| {
        let p = p.trim().to_ascii_lowercase();
        let (pat, pport) = match p.rsplit_once(':') {
            Some((h, pt)) if !h.contains(']') && pt.parse::<u16>().is_ok() => (h.to_string(), pt.parse::<u16>().ok()),
            _ => (p.clone(), None),
        };
        let port_ok = match pport {
            Some(pp) => pp == port,
            None => port == 80 || port == 443,
        };
        if !port_ok {
            return false;
        }
        if pat == "*" {
            return true;
        }
        match pat.strip_prefix("*.") {
            Some(suffix) => host.len() > suffix.len() && host.ends_with(&format!(".{suffix}")),
            None => host == pat,
        }
    })
}

/// Loopback, private, link-local, CGNAT/Tailscale (100.64/10), unique-local, unspecified,
/// multicast, broadcast: addresses on the user's machine or network rather than the internet.
pub fn is_private_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
                || (o[0] == 198 && (o[1] & 0xfe) == 18)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_ip(IpAddr::V4(v4));
            }
            let s = v6.segments();
            v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() || (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Private destinations are only allowed when the host is listed exactly (not via a wildcard).
pub fn private_allowed(patterns: &[String], host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    patterns.iter().any(|p| {
        let p = p.trim().to_ascii_lowercase();
        let p = p.rsplit_once(':').filter(|(_, pt)| pt.parse::<u16>().is_ok()).map(|(h, _)| h.to_string()).unwrap_or(p);
        p == host
    })
}

/// The hidden set actually applied: built-ins + extra, expanded.
pub fn hidden_paths(extra: &[PathBuf]) -> Vec<PathBuf> {
    ALWAYS_HIDDEN.iter().map(|p| expand(p)).chain(extra.iter().cloned()).collect()
}

/// True if `path` is (inside) a hidden path. Used by in-process tools that share the policy.
pub fn is_hidden(path: &Path, extra: &[PathBuf]) -> bool {
    hidden_paths(extra).iter().any(|h| path.starts_with(h))
}

// ---- manifests -----------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Manifest {
    pub name: Option<String>,
    pub files: ManifestFiles,
    pub network: ManifestNetwork,
    pub limits: ManifestLimits,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ManifestFiles {
    pub read: Vec<String>,
    pub write: Vec<String>,
    pub overlay: Vec<String>,
    pub hide: Vec<String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ManifestNetwork {
    pub domains: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ManifestLimits {
    pub memory: Option<String>,
    pub cpus: Option<f64>,
    pub pids: Option<u32>,
    pub timeout: Option<String>,
    pub tmp: Option<String>,
}

impl Manifest {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(toml::from_str(s)?)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let s = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&s).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn into_policy(self) -> Result<Policy> {
        let paths = |v: Vec<String>| v.iter().map(|p| expand(p)).collect::<Vec<_>>();
        let mut limits = Limits::default();
        if let Some(m) = &self.limits.memory {
            limits.memory_bytes = parse_size(m)?;
        }
        if let Some(c) = self.limits.cpus {
            limits.cpus = c;
        }
        if let Some(p) = self.limits.pids {
            limits.pids = p;
        }
        if let Some(t) = &self.limits.timeout {
            limits.timeout = parse_duration(t)?;
        }
        if let Some(t) = &self.limits.tmp {
            limits.tmp_bytes = parse_size(t)?;
        }
        Ok(Policy {
            read: paths(self.files.read),
            write: paths(self.files.write),
            overlay: paths(self.files.overlay),
            hide: paths(self.files.hide),
            domains: self.network.domains,
            limits,
            cwd: self.files.cwd.map(|c| expand(&c)),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_durations() {
        assert_eq!(parse_size("512M").unwrap(), 512 << 20);
        assert_eq!(parse_size("1.5G").unwrap(), 3 << 29);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert!(parse_size("lots").is_err());
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("5").unwrap(), Duration::from_secs(5));
    }

    #[test]
    fn domains() {
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let allow = p(&["api.github.com", "*.githubusercontent.com", "example.com:8443"]);
        assert!(domain_allowed(&allow, "api.github.com", 443));
        assert!(domain_allowed(&allow, "API.GitHub.com.", 443));
        assert!(!domain_allowed(&allow, "api.github.com", 22));
        assert!(!domain_allowed(&allow, "evil-api.github.com", 443));
        assert!(!domain_allowed(&allow, "github.com", 443));
        assert!(domain_allowed(&allow, "raw.githubusercontent.com", 443));
        assert!(!domain_allowed(&allow, "githubusercontent.com", 443));
        assert!(!domain_allowed(&allow, "evilgithubusercontent.com", 443));
        assert!(domain_allowed(&allow, "example.com", 8443));
        assert!(!domain_allowed(&allow, "example.com", 443));
        assert!(domain_allowed(&p(&["*"]), "anything.org", 80));
        assert!(!domain_allowed(&p(&["*"]), "anything.org", 25));
        assert!(!domain_allowed(&[], "example.com", 443));
    }

    #[test]
    fn manifest() {
        let m = Manifest::parse(
            r#"
            name = "notes"
            [files]
            read = ["~/notes"]
            write = ["~/notes/out"]
            [network]
            domains = ["api.github.com"]
            [limits]
            memory = "256M"
            pids = 32
            timeout = "10s"
            "#,
        )
        .unwrap();
        let p = m.into_policy().unwrap();
        assert_eq!(p.read, vec![home().join("notes")]);
        assert_eq!(p.limits.memory_bytes, 256 << 20);
        assert_eq!(p.limits.pids, 32);
        assert_eq!(p.limits.timeout, Duration::from_secs(10));
        assert!(Manifest::parse("[files]\nreed = []").is_err(), "typos are rejected");
    }

    #[test]
    fn private_ips() {
        for ip in ["127.0.0.1", "10.1.2.3", "192.168.1.1", "172.16.0.1", "169.254.1.1", "100.100.1.1", "0.0.0.0", "::1", "fd00::1", "fe80::1", "::ffff:127.0.0.1"] {
            assert!(is_private_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "93.184.216.34", "100.128.0.1", "2606:4700::1111"] {
            assert!(!is_private_ip(ip.parse().unwrap()), "{ip}");
        }
        let p = vec!["*".to_string(), "printer.lan:631".to_string()];
        assert!(!private_allowed(&p, "localhost"));
        assert!(private_allowed(&p, "printer.lan"));
    }

    #[test]
    fn hidden() {
        assert!(is_hidden(&home().join(".ssh/id_ed25519"), &[]));
        assert!(is_hidden(&home().join(".config/wang/.env"), &[]));
        assert!(!is_hidden(&home().join("notes/a.md"), &[]));
        assert!(is_hidden(&home().join("private/x"), &[home().join("private")]));
    }
}

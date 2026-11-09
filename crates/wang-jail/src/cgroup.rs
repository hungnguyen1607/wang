//! cgroups v2: one cgroup per jailed command for RAM / CPU / process-count caps, OOM and
//! fork-limit detection, usage stats, and killing everything at once.
//!
//! Lives under the user's systemd manager (`user@UID.service/wang-jail.slice/`), which has
//! the cpu, memory and pids controllers delegated on standard systemd setups.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::policy::Limits;

static COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct CgroupRoot {
    pub path: PathBuf,
}

impl CgroupRoot {
    /// `WANG_CGROUP_ROOT`, else `<user@UID.service>/wang-jail.slice` derived from our own cgroup.
    pub fn discover() -> Result<Self> {
        let path = match std::env::var_os("WANG_CGROUP_ROOT") {
            Some(p) => PathBuf::from(p),
            None => {
                let own = std::fs::read_to_string("/proc/self/cgroup").context("reading /proc/self/cgroup")?;
                let rel = own
                    .lines()
                    .find_map(|l| l.strip_prefix("0::"))
                    .context("cgroups v2 (unified hierarchy) is required")?;
                let mut base = PathBuf::from("/sys/fs/cgroup");
                let mut found = false;
                for part in rel.trim_start_matches('/').split('/') {
                    base.push(part);
                    if part.starts_with("user@") && part.ends_with(".service") {
                        found = true;
                        break;
                    }
                }
                if !found {
                    bail!("not running under a systemd user manager (cgroup {rel}); set WANG_CGROUP_ROOT to a delegated cgroup");
                }
                base.join("wang-jail.slice")
            }
        };
        if !path.exists() {
            std::fs::create_dir(&path).with_context(|| format!("creating {}", path.display()))?;
        }
        let ctl = std::fs::read_to_string(path.join("cgroup.controllers")).unwrap_or_default();
        for c in ["memory", "pids", "cpu"] {
            if !ctl.split_whitespace().any(|x| x == c) {
                bail!("cgroup controller '{c}' is not delegated to {} (have: {ctl})", path.display());
            }
        }
        let sub = std::fs::read_to_string(path.join("cgroup.subtree_control")).unwrap_or_default();
        if !["memory", "pids", "cpu"].iter().all(|c| sub.split_whitespace().any(|x| x == *c)) {
            std::fs::write(path.join("cgroup.subtree_control"), "+memory +pids +cpu")
                .with_context(|| format!("enabling controllers in {}", path.display()))?;
        }
        Ok(Self { path })
    }

    pub fn create(&self, limits: &Limits) -> Result<Cgroup> {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = self.path.join(format!("j{}-{n}", std::process::id()));
        std::fs::create_dir(&path).with_context(|| format!("creating {}", path.display()))?;
        let cg = Cgroup { path };
        cg.set("memory.max", &limits.memory_bytes.to_string())?;
        // No swap: otherwise a memory bomb just pushes the laptop into swap.
        cg.set("memory.swap.max", "0")?;
        cg.set("pids.max", &limits.pids.to_string())?;
        let period = 100_000u64;
        let quota = ((limits.cpus.max(0.01)) * period as f64) as u64;
        cg.set("cpu.max", &format!("{quota} {period}"))?;
        Ok(cg)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub peak_memory_bytes: Option<u64>,
    pub cpu_usec: Option<u64>,
    pub oom_kills: u64,
    /// Times a fork/clone was refused because pids.max was reached.
    pub pids_limit_hits: u64,
}

#[derive(Debug)]
pub struct Cgroup {
    pub path: PathBuf,
}

fn kv(path: &Path, key: &str) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix(key).and_then(|v| v.trim().parse().ok()))
}

impl Cgroup {
    fn set(&self, file: &str, value: &str) -> Result<()> {
        std::fs::write(self.path.join(file), value).with_context(|| format!("writing {value} to {}/{file}", self.path.display()))
    }

    pub fn procs_file(&self) -> PathBuf {
        self.path.join("cgroup.procs")
    }

    pub fn usage(&self) -> Usage {
        Usage {
            peak_memory_bytes: std::fs::read_to_string(self.path.join("memory.peak")).ok().and_then(|s| s.trim().parse().ok()),
            cpu_usec: kv(&self.path.join("cpu.stat"), "usage_usec "),
            oom_kills: kv(&self.path.join("memory.events"), "oom_kill ").unwrap_or(0),
            pids_limit_hits: kv(&self.path.join("pids.events"), "max ").unwrap_or(0),
        }
    }

    /// Memory in use right now (`memory.current`).
    pub fn memory_current(&self) -> Option<u64> {
        std::fs::read_to_string(self.path.join("memory.current")).ok()?.trim().parse().ok()
    }

    pub fn kill(&self) {
        let _ = std::fs::write(self.path.join("cgroup.kill"), "1");
    }

    /// Kill anything left and remove the cgroup (processes exit asynchronously, so retry).
    /// Blocking: call from a blocking thread.
    pub fn destroy_blocking(self) {
        self.kill();
        for _ in 0..50 {
            if std::fs::remove_dir(&self.path).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        eprintln!("wang-jail: could not remove {}", self.path.display());
    }

    /// Read final usage, then destroy, off the runtime thread.
    pub async fn finish(self) -> Usage {
        let path = self.path.clone();
        match tokio::task::spawn_blocking(move || {
            let u = self.usage();
            self.destroy_blocking();
            u
        })
        .await
        {
            Ok(u) => u,
            Err(_) => {
                eprintln!("wang-jail: cleanup of {} failed", path.display());
                Usage::default()
            }
        }
    }

    pub async fn destroy(self) {
        let _ = tokio::task::spawn_blocking(move || self.destroy_blocking()).await;
    }
}

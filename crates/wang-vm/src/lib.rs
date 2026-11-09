//! wang-vm: throwaway Firecracker micro-VMs for big jobs. Boot, snapshot, fork into N copies,
//! roll back, and try N approaches in parallel to keep the best.
//!
//! ```text
//! wang ── vsock (Unix socket) ──► wang-vm-agent (PID 1 in the guest): exec / read / write
//!   └─ jail (cgroup RAM cap, namespaces, seccomp, /dev/kvm only)
//!        └─ firecracker ── KVM ──► guest: read-only squashfs image + RAM overlay, no NIC
//!                                   └─ 127.0.0.1:3128 ── vsock ──► host egress proxy (allow-list)
//! ```
//!
//! The image is built from any Docker image (`wang vm setup --image python:3.12-slim`). Every
//! write in the guest lands in guest RAM, so a memory snapshot captures the whole machine, and
//! clones restored from it share all unchanged pages through the page cache.

pub mod agent;
pub mod api;
pub mod bench;
pub mod sessions;
pub mod setup;
pub mod vm;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::json;
use wang_jail::Jail;

pub use agent::Exec;
pub use sessions::Sessions;
pub use vm::{Memory, Vm};

#[derive(Debug, Clone, PartialEq)]
pub struct VmCfg {
    pub vcpus: u32,
    pub mem_mib: u64,
    /// Guest network allow-list (through the host egress proxy). Empty = no network.
    pub domains: Vec<String>,
    /// Let KSM merge identical pages across VMs.
    pub ksm: bool,
}

impl Default for VmCfg {
    fn default() -> Self {
        Self { vcpus: 2, mem_mib: 512, domains: vec![], ksm: true }
    }
}

/// Where the VM pieces live (default `~/.cache/wang/vm`).
#[derive(Debug, Clone)]
pub struct Assets {
    pub root: PathBuf,
    pub firecracker: PathBuf,
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
}

impl Assets {
    pub fn default_root() -> PathBuf {
        dirs::cache_dir().unwrap_or_else(|| PathBuf::from(".")).join("wang/vm")
    }

    pub fn at(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            firecracker: root.join("firecracker"),
            kernel: root.join("vmlinux"),
            rootfs: root.join("rootfs.sqfs"),
        }
    }

    /// The assets, if `wang vm setup` has been run.
    pub fn find(root: Option<&Path>) -> Result<Self> {
        let a = Self::at(&root.map(PathBuf::from).unwrap_or_else(Self::default_root));
        for (what, p) in [("firecracker", &a.firecracker), ("kernel", &a.kernel), ("image", &a.rootfs)] {
            if !p.exists() {
                bail!("VM {what} missing at {} (run `wang vm setup`)", p.display());
            }
        }
        if !Path::new("/dev/kvm").exists() {
            bail!("/dev/kvm not found: this machine can't run VMs (KVM disabled or not supported)");
        }
        Ok(a)
    }

    pub fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }

    pub fn snap_dir(&self) -> PathBuf {
        self.root.join("snaps")
    }

    pub fn base_dir(&self) -> PathBuf {
        self.root.join("base")
    }

    /// The idle, just-booted snapshot that new VMs start from, if it matches `cfg`.
    pub fn base(&self, cfg: &VmCfg) -> Option<Snapshot> {
        let s = Snapshot::load(&self.base_dir()).ok()?;
        (s.cfg.vcpus == cfg.vcpus && s.cfg.mem_mib == cfg.mem_mib).then_some(s)
    }

    /// Remove run directories and snapshots left by processes that are gone.
    pub fn clean_stale(&self) {
        for dir in [self.run_dir(), self.snap_dir()] {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let pid: Option<u32> = name.split('-').next().and_then(|p| p.parse().ok());
                if let Some(pid) = pid
                    && !Path::new(&format!("/proc/{pid}")).exists()
                {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
    }
}

/// A saved VM: `vmstate` (devices, vCPUs) + `mem` (guest RAM).
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub dir: PathBuf,
    pub cfg: VmCfg,
    pub create_ms: f64,
}

impl Snapshot {
    pub fn save_meta(&self) -> Result<()> {
        let meta = json!({"vcpus": self.cfg.vcpus, "mem_mib": self.cfg.mem_mib, "create_ms": self.create_ms});
        std::fs::write(self.dir.join("meta.json"), meta.to_string())?;
        Ok(())
    }

    pub fn load(dir: &Path) -> Result<Self> {
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json"))?)?;
        if !dir.join("vmstate").exists() || !dir.join("mem").exists() {
            bail!("incomplete snapshot at {}", dir.display());
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            cfg: VmCfg {
                vcpus: v["vcpus"].as_u64().unwrap_or(2) as u32,
                mem_mib: v["mem_mib"].as_u64().unwrap_or(512),
                ..Default::default()
            },
            create_ms: v["create_ms"].as_f64().unwrap_or(0.0),
        })
    }

    /// Delete the snapshot's files.
    pub fn remove(self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Start a VM: restore the base snapshot when there is one (fast), else cold boot.
pub async fn start(jail: &Jail, assets: &Assets, cfg: &VmCfg) -> Result<Vm> {
    match assets.base(cfg) {
        Some(base) => Vm::restore(jail, assets, &base, cfg).await,
        None => Vm::boot(jail, assets, cfg).await,
    }
}

/// N VMs from one snapshot, started together.
pub async fn fork(jail: &Jail, assets: &Assets, snap: &Snapshot, cfg: &VmCfg, n: usize) -> Vec<Result<Vm>> {
    futures::future::join_all((0..n).map(|_| Vm::restore(jail, assets, snap, cfg))).await
}

/// One attempt of [`try_parallel`].
#[derive(Debug)]
pub struct Attempt {
    pub command: String,
    pub run: Result<Exec, String>,
    /// From the score command: its last line as a number, else 1 for exit 0 and 0 otherwise.
    /// Without a score command: 1 if the attempt itself exited 0.
    pub score: f64,
    pub score_output: Option<String>,
}

pub struct Tried {
    pub attempts: Vec<Attempt>,
    /// Index of the best attempt, if any finished.
    pub best: Option<usize>,
    /// The VM the best attempt ran in (the others are stopped).
    pub kept: Option<Vm>,
    pub fork_ms: f64,
    pub total_ms: f64,
}

/// Fork `snap` once per command, run each command in its own copy, score each copy with
/// `score` (run after the command, in the same VM), keep the best VM and stop the rest.
pub async fn try_parallel(
    jail: &Jail,
    assets: &Assets,
    snap: &Snapshot,
    cfg: &VmCfg,
    commands: &[String],
    score: Option<&str>,
    timeout: Duration,
) -> Result<Tried> {
    if commands.is_empty() {
        bail!("nothing to try");
    }
    let t = Instant::now();
    let vms = fork(jail, assets, snap, cfg, commands.len()).await;
    let fork_ms = t.elapsed().as_secs_f64() * 1e3;
    let runs = futures::future::join_all(vms.into_iter().zip(commands).map(|(vm, cmd)| async move {
        let mut vm = match vm {
            Ok(vm) => vm,
            Err(e) => {
                return (None, Attempt { command: cmd.clone(), run: Err(format!("{e:#}")), score: f64::NEG_INFINITY, score_output: None });
            }
        };
        let run = vm.exec(cmd, timeout).await.map_err(|e| format!("{e:#}"));
        let (s, out) = match (&run, score) {
            (Err(_), _) => (f64::NEG_INFINITY, None),
            (Ok(r), None) => (if r.ok() { 1.0 } else { 0.0 }, None),
            (Ok(_), Some(sc)) => match vm.exec(sc, timeout).await {
                Ok(o) => (parse_score(&o), Some(format!("{}{}", o.stdout, o.stderr).trim().to_string())),
                Err(e) => (f64::NEG_INFINITY, Some(format!("{e:#}"))),
            },
        };
        if !vm.alive() {
            vm.stop().await;
            return (None, Attempt { command: cmd.clone(), run, score: f64::NEG_INFINITY, score_output: out });
        }
        (Some(vm), Attempt { command: cmd.clone(), run, score: s, score_output: out })
    }))
    .await;
    // Best score; ties go to the faster run.
    let best = runs
        .iter()
        .enumerate()
        .filter(|(_, (vm, a))| vm.is_some() && a.score.is_finite())
        .max_by(|(_, (_, a)), (_, (_, b))| {
            a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal).then_with(|| {
                let ms = |x: &Attempt| x.run.as_ref().map(|r| r.ms).unwrap_or(u64::MAX);
                ms(b).cmp(&ms(a))
            })
        })
        .map(|(i, _)| i);
    let mut kept = None;
    let mut attempts = Vec::with_capacity(runs.len());
    for (i, (vm, a)) in runs.into_iter().enumerate() {
        if let Some(mut vm) = vm {
            if Some(i) == best {
                kept = Some(vm);
            } else {
                vm.stop().await;
            }
        }
        attempts.push(a);
    }
    Ok(Tried { attempts, best, kept, fork_ms, total_ms: t.elapsed().as_secs_f64() * 1e3 })
}

/// A score command's result: the last line of output as a number, else pass/fail.
pub fn parse_score(o: &Exec) -> f64 {
    let last = o.stdout.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
    match last.trim().parse::<f64>() {
        Ok(v) if v.is_finite() && o.ok() => v,
        _ => {
            if o.ok() {
                1.0
            } else {
                0.0
            }
        }
    }
}

/// Make sure a snapshot was taken with the same machine shape as `cfg`.
fn check_shape(snap: &Snapshot, cfg: &VmCfg) -> Result<()> {
    if snap.cfg.vcpus != cfg.vcpus || snap.cfg.mem_mib != cfg.mem_mib {
        bail!(
            "snapshot is {} vCPU / {} MiB but {} vCPU / {} MiB was asked for",
            snap.cfg.vcpus,
            snap.cfg.mem_mib,
            cfg.vcpus,
            cfg.mem_mib
        );
    }
    Ok(())
}

pub(crate) fn shape_ok(snap: &Snapshot, cfg: &VmCfg) -> Result<()> {
    check_shape(snap, cfg).context("restoring snapshot")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores() {
        let e = |exit, out: &str| Exec { exit, stdout: out.into(), ..Default::default() };
        assert_eq!(parse_score(&e(0, "tests: 5 passed\n0.83\n")), 0.83);
        assert_eq!(parse_score(&e(0, "all good")), 1.0);
        assert_eq!(parse_score(&e(1, "0.99")), 0.0);
        assert_eq!(parse_score(&e(0, "")), 1.0);
    }
}

#[cfg(test)]
mod e2e;

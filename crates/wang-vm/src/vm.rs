//! One Firecracker micro-VM, itself inside a wang jail.
//!
//! Inside the jail every VM sees the same paths: `/fc/{firecracker,vmlinux,rootfs}` (read-only
//! assets), `/vm` (its own run directory: API socket, vsock socket, new snapshots) and `/snap`
//! (the snapshot it was restored from, read-only). Snapshots record those paths, so one snapshot
//! restores into any number of VMs at once without their sockets colliding.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::json;
use wang_jail::proxy::EgressProxy;
use wang_jail::run::Running;
use wang_jail::{Bind, Jail, Limits, Policy};

use crate::agent::{self, Exec};
use crate::{Assets, Snapshot, VmCfg};

const BOOT_ARGS: &str = "console=ttyS0 quiet reboot=k panic=1 pci=off i8042.noaux i8042.nomux i8042.nopnp i8042.dumbkbd \
                         init=/wang-agent ro rootfstype=squashfs";

static NEXT: AtomicU64 = AtomicU64::new(0);

pub struct Vm {
    pub id: String,
    pub dir: PathBuf,
    running: Option<Running>,
    proxy: Option<EgressProxy>,
    pub cfg: VmCfg,
    /// Time from spawn until the agent answered.
    pub ready_ms: f64,
    snaps: u64,
}

impl Vm {
    fn new_dir(assets: &Assets) -> Result<(String, PathBuf)> {
        let id = format!("{}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed), chrono_like_now());
        let dir = assets.run_dir().join(&id);
        std::fs::create_dir_all(&dir)?;
        Ok((id, dir))
    }

    fn policy(assets: &Assets, cfg: &VmCfg, dir: &Path, snap: Option<&Path>) -> Policy {
        let ro = |src: &Path, dst: &str| Bind { src: src.to_path_buf(), dst: PathBuf::from(dst), writable: false };
        let mut binds = vec![
            ro(&assets.firecracker, "/fc/firecracker"),
            ro(&assets.kernel, "/fc/vmlinux"),
            ro(&assets.rootfs, "/fc/rootfs"),
            Bind { src: dir.to_path_buf(), dst: PathBuf::from("/vm"), writable: true },
        ];
        if let Some(s) = snap {
            binds.push(ro(s, "/snap"));
        }
        Policy {
            binds,
            devices: vec![PathBuf::from("/dev/kvm")],
            ksm: cfg.ksm,
            limits: Limits {
                // Guest RAM plus Firecracker's own overhead; no swap.
                memory_bytes: (cfg.mem_mib + 128) << 20,
                cpus: cfg.vcpus as f64,
                pids: 128,
                timeout: Duration::from_secs(7 * 24 * 3600),
                tmp_bytes: 16 << 20,
                max_output_bytes: 1 << 20,
            },
            ..Default::default()
        }
    }

    async fn spawn(jail: &Jail, assets: &Assets, cfg: &VmCfg, snap: Option<&Path>) -> Result<Self> {
        let (id, dir) = Self::new_dir(assets)?;
        let policy = Self::policy(assets, cfg, &dir, snap);
        let argv: Vec<String> = ["/fc/firecracker", "--api-sock", "/vm/api.sock", "--level", "Warning"].iter().map(|s| s.to_string()).collect();
        let running = jail.spawn_piped(&policy, &argv).await.context("starting firecracker in the jail")?;
        // The guest's 127.0.0.1:3128 reaches this proxy over vsock; it applies the allow-list.
        let proxy = if cfg.domains.is_empty() {
            None
        } else {
            Some(EgressProxy::start(&dir.join(format!("v.sock_{}", agent::PROXY_PORT)), cfg.domains.clone())?)
        };
        let mut vm = Self { id, dir, running: Some(running), proxy, cfg: cfg.clone(), ready_ms: 0.0, snaps: 0 };
        if let Err(e) = vm.wait_api().await {
            let out = vm.stop().await;
            bail!("{e}{}", tail(&out));
        }
        Ok(vm)
    }

    fn api(&self) -> PathBuf {
        self.dir.join("api.sock")
    }

    fn vsock(&self) -> PathBuf {
        self.dir.join("v.sock")
    }

    async fn wait_api(&self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if crate::api::call(&self.api(), "GET", "/", None).await.is_ok() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        bail!("firecracker didn't start (no API socket after 5 s)")
    }

    /// Cold boot: kernel + read-only image, RAM overlay on top.
    pub async fn boot(jail: &Jail, assets: &Assets, cfg: &VmCfg) -> Result<Self> {
        let t = Instant::now();
        let mut vm = Self::spawn(jail, assets, cfg, None).await?;
        let r = async {
            let api = vm.api();
            crate::api::put(&api, "/machine-config", json!({"vcpu_count": cfg.vcpus, "mem_size_mib": cfg.mem_mib})).await?;
            crate::api::put(&api, "/boot-source", json!({"kernel_image_path": "/fc/vmlinux", "boot_args": BOOT_ARGS})).await?;
            crate::api::put(&api, "/drives/root", json!({"drive_id": "root", "path_on_host": "/fc/rootfs", "is_root_device": true, "is_read_only": true})).await?;
            crate::api::put(&api, "/vsock", json!({"guest_cid": 3, "uds_path": "/vm/v.sock"})).await?;
            crate::api::put(&api, "/entropy", json!({})).await?;
            crate::api::put(&api, "/actions", json!({"action_type": "InstanceStart"})).await?;
            agent::wait_ready(&vm.vsock(), Duration::from_secs(20)).await
        }
        .await;
        if let Err(e) = r {
            let out = vm.stop().await;
            bail!("VM boot failed: {e:#}{}", tail(&out));
        }
        vm.ready_ms = t.elapsed().as_secs_f64() * 1e3;
        Ok(vm)
    }

    /// A new VM from a snapshot: same memory, same files, running where the snapshot left off.
    /// Its memory is the snapshot file mapped copy-on-write, so clones share unchanged pages.
    /// `cfg` must have the snapshot's vCPUs and memory; its network allow-list and KSM setting
    /// apply to the new VM.
    pub async fn restore(jail: &Jail, assets: &Assets, snap: &Snapshot, cfg: &VmCfg) -> Result<Self> {
        crate::shape_ok(snap, cfg)?;
        let t = Instant::now();
        let mut vm = Self::spawn(jail, assets, cfg, Some(&snap.dir)).await?;
        let r = async {
            crate::api::put(
                &vm.api(),
                "/snapshot/load",
                json!({"snapshot_path": "/snap/vmstate", "mem_backend": {"backend_type": "File", "backend_path": "/snap/mem"}, "resume_vm": true}),
            )
            .await?;
            agent::wait_ready(&vm.vsock(), Duration::from_secs(10)).await
        }
        .await;
        if let Err(e) = r {
            let out = vm.stop().await;
            bail!("VM restore failed: {e:#}{}", tail(&out));
        }
        vm.ready_ms = t.elapsed().as_secs_f64() * 1e3;
        Ok(vm)
    }

    /// Run a shell command in the VM (cwd /work by default).
    pub async fn exec(&self, cmd: &str, timeout: Duration) -> Result<Exec> {
        let mut req = json!({"op": "exec", "cmd": cmd, "timeout_ms": timeout.as_millis() as u64});
        if self.proxy.is_some() {
            let url = format!("http://127.0.0.1:{}", agent::PROXY_PORT);
            req["env"] = json!({"HTTP_PROXY": url, "HTTPS_PROXY": url, "http_proxy": url, "https_proxy": url,
                                "NO_PROXY": "localhost,127.0.0.1", "no_proxy": "localhost,127.0.0.1"});
        }
        let v = agent::call(&self.vsock(), req, timeout + Duration::from_secs(10)).await?;
        Ok(agent::parse_exec(&v))
    }

    pub async fn write_file(&self, path: &str, content: &str) -> Result<()> {
        agent::call(&self.vsock(), json!({"op": "write", "path": path, "content": content}), Duration::from_secs(30)).await?;
        Ok(())
    }

    pub async fn read_file(&self, path: &str) -> Result<String> {
        let v = agent::call(&self.vsock(), json!({"op": "read", "path": path}), Duration::from_secs(30)).await?;
        Ok(v["content"].as_str().unwrap_or("").to_string())
    }

    /// Save memory + device state. The VM keeps running afterwards unless `keep_paused`.
    pub async fn snapshot(&mut self, assets: &Assets, keep_paused: bool) -> Result<Snapshot> {
        self.snaps += 1;
        let name = format!("snap-{}", self.snaps);
        std::fs::create_dir_all(self.dir.join(&name))?;
        let t = Instant::now();
        let api = self.api();
        crate::api::patch(&api, "/vm", json!({"state": "Paused"})).await?;
        let r = crate::api::put(
            &api,
            "/snapshot/create",
            json!({"snapshot_type": "Full", "snapshot_path": format!("/vm/{name}/vmstate"), "mem_file_path": format!("/vm/{name}/mem")}),
        )
        .await;
        if !keep_paused {
            crate::api::patch(&api, "/vm", json!({"state": "Resumed"})).await?;
        }
        r?;
        let ms = t.elapsed().as_secs_f64() * 1e3;
        // Move it out of the run directory so it outlives this VM.
        let dest = assets.snap_dir().join(format!("{}-{name}", self.id));
        std::fs::create_dir_all(assets.snap_dir())?;
        std::fs::rename(self.dir.join(&name), &dest)?;
        let snap = Snapshot { dir: dest, cfg: self.cfg.clone(), create_ms: ms };
        snap.save_meta()?;
        Ok(snap)
    }

    /// Memory this VM actually costs: proportional set size of its processes (shared pages
    /// counted once across all VMs), and the cgroup's view.
    pub fn memory(&self) -> Memory {
        let mut m = Memory::default();
        if let Some(r) = &self.running {
            m.cgroup_bytes = r.memory_bytes().unwrap_or(0);
            for pid in r.pids() {
                if let Ok(s) = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")) {
                    for l in s.lines() {
                        let kb = |l: &str| l.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0) * 1024;
                        if l.starts_with("Pss:") {
                            m.pss_bytes += kb(l);
                        } else if l.starts_with("Rss:") {
                            m.rss_bytes += kb(l);
                        }
                    }
                }
                if let Ok(s) = std::fs::read_to_string(format!("/proc/{pid}/ksm_stat")) {
                    for l in s.lines() {
                        if let Some(v) = l.strip_prefix("ksm_merging_pages ") {
                            m.ksm_merging_pages += v.trim().parse::<u64>().unwrap_or(0);
                        }
                    }
                }
            }
        }
        m
    }

    pub fn alive(&mut self) -> bool {
        self.running.as_mut().is_some_and(|r| r.try_exit_code().is_none())
    }

    pub fn blocked_hosts(&self) -> Vec<String> {
        self.proxy.as_ref().map(|p| p.blocked()).unwrap_or_default()
    }

    /// Kill the VM and remove its run directory. Returns Firecracker's log output.
    pub async fn stop(&mut self) -> String {
        let mut out = String::new();
        if let Some(r) = self.running.take() {
            let st = r.stop().await;
            out = String::from_utf8_lossy(&st.stderr).into_owned();
        }
        if let Some(p) = self.proxy.take() {
            p.stop();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
        out
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        // `Running`'s own Drop kills the jail; clean up the directory too.
        self.running.take();
        if let Some(p) = self.proxy.take() {
            p.stop();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Memory {
    pub pss_bytes: u64,
    pub rss_bytes: u64,
    pub cgroup_bytes: u64,
    pub ksm_merging_pages: u64,
}

fn tail(log: &str) -> String {
    let lines: Vec<&str> = log.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return String::new();
    }
    format!("\n{}", lines[lines.len().saturating_sub(8)..].join("\n"))
}

fn chrono_like_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64 % 1_000_000_000).unwrap_or(0)
}

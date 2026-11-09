//! One VM per agent conversation, with a checkpoint to roll back to and parallel tries.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use tokio::sync::Mutex;
use wang_jail::Jail;

use crate::{Assets, Exec, Snapshot, Tried, Vm, VmCfg};

struct Session {
    vm: Option<Vm>,
    checkpoint: Option<Snapshot>,
    last_used: Instant,
}

pub struct Sessions {
    jail: Arc<Jail>,
    pub assets: Assets,
    pub cfg: VmCfg,
    pub max_vms: usize,
    pub idle_timeout: Duration,
    map: Mutex<HashMap<String, Arc<Mutex<Session>>>>,
}

fn mem_available_mib() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("MemAvailable:"))?.split_whitespace().nth(1)?.parse::<u64>().ok())
        .unwrap_or(u64::MAX)
        >> 10
}

impl Sessions {
    pub fn new(jail: Arc<Jail>, assets: Assets, cfg: VmCfg, max_vms: usize) -> Arc<Self> {
        assets.clean_stale();
        Arc::new(Self { jail, assets, cfg, max_vms: max_vms.max(1), idle_timeout: Duration::from_secs(900), map: Mutex::new(HashMap::new()) })
    }

    async fn session(&self, id: &str) -> Arc<Mutex<Session>> {
        self.map
            .lock()
            .await
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(Session { vm: None, checkpoint: None, last_used: Instant::now() })))
            .clone()
    }

    /// VMs running across all conversations (sessions busy right now count as running).
    async fn running(&self) -> usize {
        let map = self.map.lock().await;
        let mut n = 0;
        for s in map.values() {
            match s.try_lock() {
                Ok(s) => n += s.vm.is_some() as usize,
                Err(_) => n += 1,
            }
        }
        n
    }

    async fn room_for(&self, extra: usize) -> Result<()> {
        let running = self.running().await;
        if running + extra > self.max_vms {
            bail!("too many VMs: {running} running, {extra} more asked for, limit {} ([vm] max_vms)", self.max_vms);
        }
        // New VMs share the snapshot's pages, but each can grow to its full guest RAM.
        let need = self.cfg.mem_mib.min(1024) + 256;
        let avail = mem_available_mib();
        if avail < need {
            bail!("not starting a VM: only {avail} MiB of memory free (need {need} MiB)");
        }
        Ok(())
    }

    async fn ensure_vm<'a>(&self, s: &'a mut Session) -> Result<&'a mut Vm> {
        if let Some(vm) = s.vm.as_mut()
            && !vm.alive()
        {
            s.vm = None; // crashed or killed (e.g. out of memory): start fresh below
        }
        if s.vm.is_none() {
            // The caller holds this session's lock, so it counts as running already.
            self.room_for(0).await?;
            let vm = match &s.checkpoint {
                Some(c) => Vm::restore(&self.jail, &self.assets, c, &self.cfg).await?,
                None => crate::start(&self.jail, &self.assets, &self.cfg).await?,
            };
            s.vm = Some(vm);
        }
        Ok(s.vm.as_mut().expect("vm just started"))
    }

    pub async fn run(&self, id: &str, cmd: &str, timeout: Duration) -> Result<Exec> {
        let s = self.session(id).await;
        let mut s = s.lock().await;
        s.last_used = Instant::now();
        let vm = self.ensure_vm(&mut s).await?;
        match vm.exec(cmd, timeout).await {
            Ok(r) => Ok(r),
            Err(e) => {
                // The VM is up but its agent doesn't answer (e.g. killed by the guest's OOM
                // killer): replace it (from the checkpoint, if any) and try once more.
                tracing::warn!(session = id, "VM agent not answering ({e:#}); replacing the VM");
                if let Some(mut dead) = s.vm.take() {
                    dead.stop().await;
                }
                let vm = self.ensure_vm(&mut s).await?;
                vm.exec(cmd, timeout).await
            }
        }
    }

    /// Save the VM's whole state; `rollback` returns here. Replaces the previous checkpoint.
    pub async fn checkpoint(&self, id: &str) -> Result<f64> {
        let s = self.session(id).await;
        let mut s = s.lock().await;
        s.last_used = Instant::now();
        let vm = self.ensure_vm(&mut s).await?;
        let snap = vm.snapshot(&self.assets, false).await?;
        let ms = snap.create_ms;
        if let Some(old) = s.checkpoint.replace(snap) {
            old.remove();
        }
        Ok(ms)
    }

    /// Throw the VM away and continue from the last checkpoint (or a clean VM if none).
    pub async fn rollback(&self, id: &str) -> Result<bool> {
        let s = self.session(id).await;
        let mut s = s.lock().await;
        s.last_used = Instant::now();
        if let Some(mut vm) = s.vm.take() {
            vm.stop().await;
        }
        let had = s.checkpoint.is_some();
        self.ensure_vm(&mut s).await?;
        Ok(had)
    }

    /// Back to a clean VM; drops the checkpoint.
    pub async fn reset(&self, id: &str) {
        let s = self.session(id).await;
        let mut s = s.lock().await;
        if let Some(mut vm) = s.vm.take() {
            vm.stop().await;
        }
        if let Some(c) = s.checkpoint.take() {
            c.remove();
        }
    }

    /// Fork the current VM once per command; keep the best copy as this conversation's VM.
    pub async fn try_parallel(&self, id: &str, commands: &[String], score: Option<&str>, timeout: Duration) -> Result<Tried> {
        let s = self.session(id).await;
        let mut s = s.lock().await;
        s.last_used = Instant::now();
        self.room_for(commands.len()).await?;
        let vm = self.ensure_vm(&mut s).await?;
        let snap = vm.snapshot(&self.assets, true).await?;
        let mut tried = crate::try_parallel(&self.jail, &self.assets, &snap, &self.cfg, commands, score, timeout).await;
        snap.remove();
        if let Ok(t) = tried.as_mut()
            && let Some(best) = t.kept.take()
        {
            if let Some(mut old) = s.vm.replace(best) {
                old.stop().await;
            }
        } else if let Some(vm) = s.vm.as_mut() {
            // Nothing to keep: carry on with the original (it was only paused).
            crate::api::patch(&vm.dir.join("api.sock"), "/vm", serde_json::json!({"state": "Resumed"})).await.ok();
        }
        tried
    }

    /// Stop VMs idle longer than `idle_timeout` (checkpoints stay).
    pub async fn reap(&self) -> usize {
        let map = self.map.lock().await;
        let mut n = 0;
        for s in map.values() {
            if let Ok(mut s) = s.try_lock()
                && s.vm.is_some()
                && s.last_used.elapsed() > self.idle_timeout
                && let Some(mut vm) = s.vm.take()
            {
                vm.stop().await;
                n += 1;
            }
        }
        n
    }

    pub fn spawn_reaper(self: &Arc<Self>) {
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let Some(s) = me.upgrade() else { return };
                s.reap().await;
            }
        });
    }

    pub async fn shutdown(&self) {
        let all: Vec<_> = self.map.lock().await.drain().map(|(_, s)| s).collect();
        for s in all {
            let mut s = s.lock().await;
            if let Some(mut vm) = s.vm.take() {
                vm.stop().await;
            }
            if let Some(c) = s.checkpoint.take() {
                c.remove();
            }
        }
    }
}

//! `wang vm bench`: boot, restore, snapshot, fork N, memory sharing (page cache + KSM),
//! and try-N-in-parallel, on this machine.

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::Result;
use wang_jail::Jail;

use crate::{Assets, Vm, VmCfg};

fn mem_available_mib() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("MemAvailable:"))?.split_whitespace().nth(1)?.parse::<u64>().ok())
        .unwrap_or(0)
        >> 10
}

fn ksm_global() -> (u64, u64) {
    let r = |f: &str| std::fs::read_to_string(format!("/sys/kernel/mm/ksm/{f}")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    (r("pages_shared"), r("pages_sharing"))
}

/// KSM's system-wide scan speed in MB/s (pages_to_scan per sleep_millisecs; root sets it).
pub fn ksm_scan_mb_s() -> f64 {
    let r = |f: &str| std::fs::read_to_string(format!("/sys/kernel/mm/ksm/{f}")).ok().and_then(|s| s.trim().parse::<f64>().ok()).unwrap_or(0.0);
    let (pages, ms) = (r("pages_to_scan"), r("sleep_millisecs").max(1.0));
    pages * 4096.0 * (1000.0 / ms) / 1e6
}

fn stats(v: &mut [f64]) -> String {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    format!("median {:.0} ms (min {:.0}, max {:.0})", v[v.len() / 2], v[0], v[v.len() - 1])
}

pub async fn run(jail: &Jail, assets: &Assets, cfg: &VmCfg, clones: usize, out: &mut dyn Write) -> Result<()> {
    assets.clean_stale();
    writeln!(out, "wang vm bench: {} vCPU, {} MiB per VM, KSM {}", cfg.vcpus, cfg.mem_mib, if cfg.ksm { "on" } else { "off" })?;

    // Cold boots.
    let mut boots = vec![];
    for _ in 0..3 {
        let mut vm = Vm::boot(jail, assets, cfg).await?;
        boots.push(vm.ready_ms);
        vm.stop().await;
    }
    writeln!(out, "cold boot to ready agent      {}", stats(&mut boots))?;

    // Snapshot of an idle VM.
    let mut vm = Vm::boot(jail, assets, cfg).await?;
    let snap = vm.snapshot(assets, true).await?;
    vm.stop().await;
    let mem_file = std::fs::metadata(snap.dir.join("mem")).map(|m| m.len()).unwrap_or(0);
    let on_disk = std::process::Command::new("du").arg("-sk").arg(&snap.dir).output().ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().next()?.parse::<u64>().ok()).unwrap_or(0);
    writeln!(out, "snapshot (pause + save)       {:.0} ms · memory file {} MiB, {} MiB on disk", snap.create_ms, mem_file >> 20, on_disk >> 10)?;

    // Restores.
    let mut restores = vec![];
    for _ in 0..3 {
        let mut vm = Vm::restore(jail, assets, &snap, cfg).await?;
        restores.push(vm.ready_ms);
        vm.stop().await;
    }
    writeln!(out, "restore to ready agent        {}", stats(&mut restores))?;

    // Fork N at once.
    let avail0 = mem_available_mib();
    let t = Instant::now();
    let vms = crate::fork(jail, assets, &snap, cfg, clones).await;
    let fork_ms = t.elapsed().as_secs_f64() * 1e3;
    let mut vms: Vec<Vm> = vms.into_iter().collect::<Result<_>>()?;
    let pss = |vms: &[Vm]| vms.iter().map(|v| v.memory().pss_bytes).sum::<u64>() >> 20;
    writeln!(out, "fork {clones} VMs at once          {fork_ms:.0} ms until all {clones} agents answer")?;
    writeln!(out, "  idle: {} MiB PSS total ({} MiB per VM of {} MiB guest RAM); system MemAvailable −{} MiB",
        pss(&vms), pss(&vms) / clones as u64, cfg.mem_mib, avail0.saturating_sub(mem_available_mib()))?;

    // Same work in every clone: 96 MiB of identical data each, kept alive.
    let work = "setsid python3 -c \"import time; b = bytearray(b'wang' * (24 << 20)); time.sleep(900)\" >/dev/null 2>&1 & sleep 1";
    futures::future::join_all(vms.iter().map(|v| v.exec(work, Duration::from_secs(30)))).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let before = pss(&vms);
    writeln!(out, "  each allocates 96 MiB (identical): {before} MiB PSS total right after")?;
    if cfg.ksm {
        let rate = ksm_scan_mb_s();
        writeln!(out, "  KSM: these VMs are enrolled; the kernel scans {rate:.1} MB/s (system setting), so merging {} MiB takes ~{:.0} s",
            before, before as f64 * 1.048 / rate.max(0.01))?;
        if rate < 20.0 {
            writeln!(out, "       faster: echo 5000 | sudo tee /sys/kernel/mm/ksm/pages_to_scan; echo 20 | sudo tee /sys/kernel/mm/ksm/sleep_millisecs")?;
        }
        let (s0, sh0) = ksm_global();
        let t = Instant::now();
        let mut after = before;
        let mut merged = 0;
        while t.elapsed() < Duration::from_secs(90) {
            tokio::time::sleep(Duration::from_secs(5)).await;
            merged = vms.iter().map(|v| v.memory().ksm_merging_pages).sum::<u64>();
            let now = pss(&vms);
            if now + 8 >= after && t.elapsed() > Duration::from_secs(15) {
                after = now;
                break;
            }
            after = now;
        }
        let (s1, sh1) = ksm_global();
        writeln!(out, "  after KSM ({:.0} s): {after} MiB PSS total · {} MiB merged in these VMs · system pages_sharing +{} (shared +{})",
            t.elapsed().as_secs_f64(), merged * 4096 >> 20, sh1.saturating_sub(sh0), s1.saturating_sub(s0))?;
    }
    writeln!(out, "  system MemAvailable −{} MiB with {clones} busy VMs", avail0.saturating_sub(mem_available_mib()))?;
    for mut v in vms.drain(..) {
        v.stop().await;
    }

    // Try N approaches in parallel, keep the best.
    let approaches: Vec<String> = (1..=4).map(|k| format!("python3 -c 'import math; print(round(math.pi, {k}))' > answer")).collect();
    let score = "python3 -c \"import math; print(-abs(float(open('answer').read()) - math.pi))\"";
    let tried = crate::try_parallel(jail, assets, &snap, cfg, &approaches, Some(score), Duration::from_secs(60)).await?;
    writeln!(out, "try 4 approaches in parallel  fork {:.0} ms, all scored in {:.0} ms, best = #{} ({})",
        tried.fork_ms, tried.total_ms, tried.best.map(|b| b + 1).unwrap_or(0),
        tried.best.map(|b| tried.attempts[b].command.as_str()).unwrap_or("-"))?;
    if let Some(mut k) = tried.kept {
        k.stop().await;
    }
    snap.remove();
    Ok(())
}

//! Real Firecracker VMs in the real jail. Skipped unless `wang vm setup` has been run, /dev/kvm
//! works and the `jail` helper is built (`cargo build -p wang-jail --bin jail`).

use std::sync::Arc;
use std::time::Duration;

use wang_jail::Jail;

use crate::*;

fn env() -> Option<(Arc<Jail>, Assets)> {
    let helper = std::env::current_exe().ok()?.parent()?.parent()?.join("jail");
    let assets = Assets::find(None).ok()?;
    if !helper.exists() || assets.base(&VmCfg::default()).is_none() {
        eprintln!("skipping: needs `wang vm setup` and {}", helper.display());
        return None;
    }
    Some((Arc::new(Jail::with_helper(helper).ok()?), assets))
}

const T: Duration = Duration::from_secs(60);

#[tokio::test(flavor = "current_thread")]
async fn vm_runs_isolated_with_allow_listed_network() {
    let Some((jail, assets)) = env() else { return };
    // A host service the guest must not reach, and a host file it must not see.
    let host_srv = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = host_srv.local_addr().unwrap().port();
    let cfg = VmCfg { domains: vec!["pypi.org".into()], ..Default::default() };
    let mut vm = start(&jail, &assets, &cfg).await.unwrap();
    assert!(vm.ready_ms < 2_000.0, "slow start: {} ms", vm.ready_ms);

    let r = vm.exec("id -u; hostname; python3 -c 'print(6*7)'; ls /home", T).await.unwrap();
    assert!(r.ok(), "{r:?}");
    assert_eq!(r.stdout.lines().collect::<Vec<_>>()[..3], ["0", "wang-vm", "42"]);
    assert!(!r.stdout.contains("hung"), "host home visible: {}", r.stdout);

    // Files: write/read and the RAM overlay over the read-only image.
    vm.write_file("/work/a/notes.txt", "hello vm\n").await.unwrap();
    assert_eq!(vm.read_file("/work/a/notes.txt").await.unwrap(), "hello vm\n");
    assert!(vm.exec("echo x > /usr/local/x && cat /usr/local/x", T).await.unwrap().ok());

    // Clock follows the host (a restored snapshot's clock would be stale).
    let r = vm.exec("date +%s", T).await.unwrap();
    let guest: i64 = r.stdout.trim().parse().unwrap();
    let host = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    assert!((guest - host).abs() <= 2, "guest clock {guest} vs host {host}");

    // Network: no interface but loopback; host services unreachable; only the allow-list via the proxy.
    let r = vm.exec("ls /sys/class/net", T).await.unwrap();
    assert_eq!(r.stdout.trim(), "lo", "{r:?}");
    let probe = format!(
        "python3 - <<'EOF'\nimport socket\nfor host in ['127.0.0.1', '10.0.2.2', '192.168.1.1']:\n    try:\n        socket.create_connection((host, {port}), timeout=1); print('REACHED', host)\n    except OSError as e:\n        print('blocked', host)\nEOF"
    );
    let r = vm.exec(&probe, T).await.unwrap();
    assert!(!r.stdout.contains("REACHED"), "{}", r.stdout);
    let r = vm
        .exec("python3 -c \"import urllib.request as u; print(u.urlopen('https://pypi.org/simple/six/', timeout=20).status)\"", T)
        .await
        .unwrap();
    assert_eq!(r.stdout.trim(), "200", "allowed domain failed: {r:?}");
    let r = vm.exec("python3 -c \"import urllib.request as u; u.urlopen('https://example.com/', timeout=20)\"", T).await.unwrap();
    assert!(!r.ok() && r.stderr.contains("403"), "blocked domain got through: {r:?}");
    assert!(vm.blocked_hosts().iter().any(|h| h.starts_with("example.com")), "{:?}", vm.blocked_hosts());

    // Timeouts kill the command, not the VM.
    let r = vm.exec("sleep 30", Duration::from_secs(1)).await.unwrap();
    assert!(r.timed_out);
    assert!(vm.exec("true", T).await.unwrap().ok());
    drop(host_srv);
    vm.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn snapshot_fork_rollback_and_try_keep_best() {
    let Some((jail, assets)) = env() else { return };
    let cfg = VmCfg::default();
    let mut vm = start(&jail, &assets, &cfg).await.unwrap();
    vm.exec("echo base > /work/state", T).await.unwrap();
    let snap = vm.snapshot(&assets, false).await.unwrap();
    vm.exec("echo changed > /work/state; rm -rf /usr/bin", T).await.unwrap();

    // Forks start from the snapshot, independently.
    let forks: Vec<Vm> = fork(&jail, &assets, &snap, &cfg, 3).await.into_iter().map(|r| r.unwrap()).collect();
    for (i, f) in forks.iter().enumerate() {
        assert_eq!(f.read_file("/work/state").await.unwrap(), "base\n", "fork {i}");
        f.exec(&format!("echo {i} > /work/state"), T).await.unwrap();
    }
    for (i, f) in forks.iter().enumerate() {
        assert_eq!(f.read_file("/work/state").await.unwrap(), format!("{i}\n"), "forks share writes");
    }
    // Idle forks share the snapshot's pages: far less than 3 × guest RAM.
    let pss: u64 = forks.iter().map(|f| f.memory().pss_bytes).sum();
    assert!(pss < 3 * (cfg.mem_mib << 20) / 4, "forks don't share memory: {} MiB", pss >> 20);
    for mut f in forks {
        f.stop().await;
    }

    // Try: the best score wins and its VM is kept.
    let cmds: Vec<String> = ["echo 3 > n", "echo 9 > n", "exit 1", "echo 5 > n"].iter().map(|s| s.to_string()).collect();
    let t = try_parallel(&jail, &assets, &snap, &cfg, &cmds, Some("cat n"), T).await.unwrap();
    assert_eq!(t.best, Some(1), "{:?}", t.attempts);
    assert_eq!(t.attempts[2].score, 0.0);
    let mut kept = t.kept.unwrap();
    assert_eq!(kept.read_file("/work/n").await.unwrap(), "9\n");
    kept.stop().await;
    vm.stop().await;
    snap.remove();
}

#[tokio::test(flavor = "current_thread")]
async fn sessions_checkpoint_rollback_and_memory_cap() {
    let Some((jail, assets)) = env() else { return };
    let s = Sessions::new(jail, assets, VmCfg::default(), 6);
    assert!(s.run("c", "echo one > /work/f", T).await.unwrap().ok());
    s.checkpoint("c").await.unwrap();
    s.run("c", "echo two > /work/f", T).await.unwrap();
    assert!(s.rollback("c").await.unwrap());
    assert_eq!(s.run("c", "cat /work/f", T).await.unwrap().stdout, "one\n");
    // Another conversation has its own VM.
    assert!(!s.run("d", "test -e /work/f", T).await.unwrap().ok());

    // Try from the session's current state; the best copy becomes the session VM.
    let t = s.try_parallel("c", &["echo A >> /work/f".into(), "echo B >> /work/f; exit 2".into()], None, T).await.unwrap();
    assert_eq!(t.best, Some(0));
    assert_eq!(s.run("c", "cat /work/f", T).await.unwrap().stdout, "one\nA\n");

    // A memory bomb in the guest is stopped by the guest kernel or the jail's cap, not the host.
    let r = s.run("c", "python3 -c 'b=[]\nwhile True: b.append(bytearray(64<<20))'", T).await;
    match r {
        Ok(r) => assert!(!r.ok(), "memory bomb succeeded?"),
        Err(e) => eprintln!("VM died under the memory bomb (ok): {e:#}"),
    }
    // The conversation keeps working (a dead VM is replaced from the checkpoint).
    assert!(s.run("c", "true", T).await.unwrap().ok());
    assert!(s.try_parallel("c", &vec!["true".to_string(); 9], None, T).await.err().unwrap().to_string().contains("too many VMs"));
    s.reset("c").await;
    assert!(!s.run("c", "test -e /work/f", T).await.unwrap().ok());
    s.shutdown().await;
}

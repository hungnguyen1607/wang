//! `jail selftest`: escape attempts that must all be blocked, plus checks that normal work
//! still works. Run on every change (also wired into `cargo test`).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;

use crate::policy::{ALWAYS_HIDDEN, Limits, Policy, expand, home};
use crate::run::{Io, Jail, JailResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// An escape attempt: passes when it's blocked.
    Escape,
    /// Something legitimate that must keep working.
    Works,
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub kind: Kind,
    pub passed: bool,
    pub detail: String,
}

fn sh(cmd: &str) -> Vec<String> {
    vec!["bash".into(), "-c".into(), cmd.into()]
}

fn out(r: &JailResult) -> String {
    let mut s = String::from_utf8_lossy(&r.stdout).trim().to_string();
    let e = String::from_utf8_lossy(&r.stderr).trim().to_string();
    if !e.is_empty() {
        if !s.is_empty() {
            s.push_str(" | ");
        }
        s.push_str(&e);
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(200).collect()
}

fn home_policy() -> Policy {
    Policy { read: vec![home()], cwd: Some(home()), ..Default::default() }
}

pub async fn run_all(jail: &Jail, with_internet: bool) -> Result<Vec<Check>> {
    let mut checks = vec![];
    let mut add = |name, kind, passed, detail: String| checks.push(Check { name, kind, passed, detail });

    // --- files -----------------------------------------------------------------------
    let present: Vec<PathBuf> = ALWAYS_HIDDEN.iter().map(|p| expand(p)).filter(|p| p.exists()).collect();
    let script: String = present
        .iter()
        .map(|p| {
            let q = p.to_string_lossy().replace('\'', "'\\''");
            if p.is_dir() {
                format!("[ -z \"$(ls -A '{q}' 2>/dev/null)\" ] || echo 'LEAK {q}'; ")
            } else {
                format!("[ ! -s '{q}' ] || echo 'LEAK {q}'; ")
            }
        })
        .collect();
    let r = jail.run(&home_policy(), &sh(&format!("{script} true")), Io::Capture).await?;
    let leaks = String::from_utf8_lossy(&r.stdout).lines().filter(|l| l.starts_with("LEAK")).count();
    add(
        "secrets are hidden (~/.ssh, ~/.gnupg, wang's .env and DB, browser profiles, ...)",
        Kind::Escape,
        leaks == 0 && r.exit_code == 0,
        format!("{} secret paths present on this machine, {leaks} readable", present.len()),
    );

    // Canary: a hidden dir inside a visible dir, independent of what this machine has.
    let canary = std::env::temp_dir().join(format!("wang-canary-{}", std::process::id()));
    std::fs::create_dir_all(canary.join("secret"))?;
    std::fs::write(canary.join("secret/key"), "TOP-SECRET")?;
    std::fs::write(canary.join("public.txt"), "hello")?;
    let pol = Policy { read: vec![canary.clone()], hide: vec![canary.join("secret")], ..Default::default() };
    let c = canary.to_string_lossy();
    let r = jail.run(&pol, &sh(&format!("cat {c}/public.txt; cat {c}/secret/key 2>&1; ls -A {c}/secret")), Io::Capture).await?;
    let o = out(&r);
    add("declared-hidden canary can't be read", Kind::Escape, !o.contains("TOP-SECRET"), o);

    let r = jail.run(&home_policy(), &sh("touch ~/.wang-jail-write-test 2>&1; echo rc=$?"), Io::Capture).await?;
    let wrote = home().join(".wang-jail-write-test").exists();
    let _ = std::fs::remove_file(home().join(".wang-jail-write-test"));
    add("read-only home can't be written", Kind::Escape, !wrote, out(&r));

    let probe = std::env::temp_dir().join(format!("wang-jail-outside-{}", std::process::id()));
    let r = jail
        .run(&Policy::default(), &sh(&format!("echo pwned > {} 2>&1; echo rc=$?", probe.display())), Io::Capture)
        .await?;
    let escaped = probe.exists();
    let _ = std::fs::remove_file(&probe);
    add("host /tmp is not reachable (jail /tmp is private)", Kind::Escape, !escaped, out(&r));

    // Overlay: writes vanish.
    std::fs::write(canary.join("public.txt"), "hello")?;
    let pol = Policy { overlay: vec![canary.clone()], hide: vec![canary.join("secret")], ..Default::default() };
    let r = jail.run(&pol, &sh(&format!("echo changed > {c}/public.txt && cat {c}/public.txt")), Io::Capture).await?;
    let host = std::fs::read_to_string(canary.join("public.txt")).unwrap_or_default();
    add(
        "overlay writes stay in the jail",
        Kind::Escape,
        host == "hello",
        format!("inside: {:?}, host after: {host:?}", out(&r)),
    );

    // Write grant works.
    let pol = Policy { write: vec![canary.clone()], hide: vec![canary.join("secret")], ..Default::default() };
    let r = jail.run(&pol, &sh(&format!("echo saved > {c}/out.txt")), Io::Capture).await?;
    let saved = std::fs::read_to_string(canary.join("out.txt")).unwrap_or_default();
    add("declared write path is writable", Kind::Works, saved.trim() == "saved" && r.exit_code == 0, out(&r));
    let _ = std::fs::remove_dir_all(&canary);

    // --- network ---------------------------------------------------------------------
    let py_connect = "python3 -I -c \"import socket\ntry:\n  socket.create_connection(('1.1.1.1', 443), timeout=3); print('CONNECTED')\nexcept Exception as e: print('blocked:', e)\"";
    let r = jail.run(&Policy::default(), &sh(py_connect), Io::Capture).await?;
    let o = out(&r);
    add("no network without an allow-list (direct TCP to 1.1.1.1)", Kind::Escape, !o.contains("CONNECTED"), o);

    let pol = Policy { domains: vec!["example.com".into()], ..Default::default() };
    let r = jail
        .run(&pol, &sh("curl -sS -m 5 -o /dev/null -w '%{http_code}' https://evil.invalid/ 2>&1; echo"), Io::Capture)
        .await?;
    let blocked = r.blocked_hosts.iter().any(|h| h.starts_with("evil.invalid"));
    add(
        "undeclared domain refused by the egress proxy",
        Kind::Escape,
        blocked,
        format!("{} · proxy blocked: {:?} · exit {}{}", out(&r), r.blocked_hosts, r.exit_code, if r.timed_out { " (timed out)" } else { "" }),
    );

    let r = jail.run(&pol, &sh(&format!("{} ; curl -sS -m 5 --noproxy '*' https://example.com -o /dev/null 2>&1; echo rc=$?", py_connect)), Io::Capture).await?;
    let o = out(&r);
    add("proxy can't be bypassed (direct connections still fail)", Kind::Escape, !o.contains("CONNECTED") && !o.contains("rc=0"), o);

    let star = Policy { domains: vec!["*".into()], ..Default::default() };
    let r = jail
        .run(
            &star,
            // An attacker would use the proxy on purpose (NO_PROXY only skips it for the jail's own loopback).
            &sh("P='--proxy http://127.0.0.1:3128 --noproxy \"\"'; eval curl -sS -m 5 $P -o /dev/null http://127.0.0.1/ 2>&1; eval curl -sS -m 5 $P -o /dev/null https://localhost/ 2>&1; echo"),
            Io::Capture,
        )
        .await?;
    let both = r.blocked_hosts.iter().filter(|h| h.contains("private address")).count() == 2;
    add(
        "wildcard allow-list can't reach the host or LAN (localhost, 127.0.0.1)",
        Kind::Escape,
        both,
        format!("proxy blocked: {:?}", r.blocked_hosts),
    );

    if with_internet {
        let r = jail
            .run(&pol, &sh("curl -sS -m 10 -o /dev/null -w '%{http_code}' https://example.com/ 2>&1"), Io::Capture)
            .await?;
        let o = out(&r);
        add("declared domain is reachable through the proxy", Kind::Works, o.starts_with('2') || o.starts_with('3'), o);
    }

    // --- resources -------------------------------------------------------------------
    let pol = Policy {
        limits: Limits { pids: 64, timeout: Duration::from_secs(5), ..Default::default() },
        ..Default::default()
    };
    let r = jail.run(&pol, &sh(":(){ :|:& };: 2>/dev/null; sleep 1"), Io::Capture).await?;
    add(
        "fork bomb is contained by the process limit",
        Kind::Escape,
        r.usage.pids_limit_hits > 0,
        format!(
            "{} forks refused at pids.max=64, finished in {:.1}s{}",
            r.usage.pids_limit_hits,
            r.wall_ms / 1e3,
            if r.timed_out { " (killed at timeout)" } else { "" }
        ),
    );

    let pol = Policy { limits: Limits { memory_bytes: 128 << 20, timeout: Duration::from_secs(20), ..Default::default() }, ..Default::default() };
    let r = jail
        .run(&pol, &sh("python3 -I -c \"a=[]\nwhile True: a.append(bytearray(16<<20))\""), Io::Capture)
        .await?;
    let peak = r.usage.peak_memory_bytes.unwrap_or(0);
    add(
        "memory bomb is killed at the RAM limit (no swap)",
        Kind::Escape,
        r.oom_killed() && peak <= (128u64 << 20) + (8 << 20),
        format!("oom kills: {}, peak {} MiB, exit {}", r.usage.oom_kills, peak >> 20, r.exit_code),
    );

    // --- kernel surface --------------------------------------------------------------
    let r = jail
        .run(
            &Policy::default(),
            &sh("python3 -I -c \"import ctypes,os\nl=ctypes.CDLL(None,use_errno=True)\nr=l.ptrace(0,0,0,0)\nprint('ptrace', r, os.strerror(ctypes.get_errno()))\""),
            Io::Capture,
        )
        .await?;
    let o = out(&r);
    add("ptrace is blocked (seccomp)", Kind::Escape, o.contains("ptrace -1"), o);

    let r = jail.run(&Policy::default(), &sh("unshare -Ur true 2>&1; echo rc=$?"), Io::Capture).await?;
    let o = out(&r);
    add("nested user namespaces are blocked", Kind::Escape, !o.ends_with("rc=0"), o);

    let r = jail.run(&Policy::default(), &sh("mkdir -p /tmp/m && mount -t tmpfs none /tmp/m 2>&1; echo rc=$?"), Io::Capture).await?;
    let o = out(&r);
    add("mounting is blocked", Kind::Escape, !o.ends_with("rc=0"), o);

    let r = jail.run(&Policy::default(), &sh("ls /proc | grep -c '^[0-9]'"), Io::Capture).await?;
    let n: Option<u32> = String::from_utf8_lossy(&r.stdout).trim().parse().ok();
    add(
        "host processes are invisible (PID namespace)",
        Kind::Escape,
        n.is_some_and(|n| n < 10),
        match n {
            Some(n) => format!("{n} processes visible"),
            None => format!("unexpected output: {}", out(&r)),
        },
    );

    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user".into());
    let r = jail.run(&home_policy(), &sh(&format!("ls -A {runtime} 2>&1 | head -3; echo rc=$?")), Io::Capture).await?;
    let o = out(&r);
    add(
        "wang's daemon socket and the user runtime dir are not reachable",
        Kind::Escape,
        !o.contains("wang.sock") && !o.contains("bus"),
        o,
    );

    // --- normal work -----------------------------------------------------------------
    let r = jail
        .run(&home_policy(), &sh("ls ~ >/dev/null && python3 -c 'print(6*7)' && echo ok"), Io::Capture)
        .await?;
    add("normal commands work (ls home, python)", Kind::Works, r.exit_code == 0 && out(&r).contains("42"), out(&r));

    Ok(checks)
}

pub fn report(checks: &[Check]) -> (String, bool) {
    let mut s = String::new();
    let esc: Vec<_> = checks.iter().filter(|c| c.kind == Kind::Escape).collect();
    let works: Vec<_> = checks.iter().filter(|c| c.kind == Kind::Works).collect();
    s += "escape attempts\n";
    for c in &esc {
        s += &format!("  {} {}\n      {}\n", if c.passed { "BLOCKED" } else { "ESCAPED" }, c.name, c.detail);
    }
    s += "\nnormal work\n";
    for c in &works {
        s += &format!("  {} {}\n      {}\n", if c.passed { "OK     " } else { "BROKEN " }, c.name, c.detail);
    }
    let blocked = esc.iter().filter(|c| c.passed).count();
    let ok = works.iter().filter(|c| c.passed).count();
    s += &format!("\n{blocked}/{} escapes blocked · {ok}/{} normal checks ok\n", esc.len(), works.len());
    (s, blocked == esc.len() && ok == works.len())
}

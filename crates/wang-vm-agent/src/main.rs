//! wang-vm-agent: PID 1 inside a wang micro-VM.
//!
//! Boot: mount /proc /sys /dev, put a RAM overlay over the read-only root (so the image is shared
//! by every VM and every write lands in guest memory, which snapshots capture), bring up
//! loopback, then fork a worker and reap orphans forever.
//!
//! The worker serves the host over vsock port 52: one JSON request per line, one JSON reply per
//! line (`ping`, `exec`, `write`, `read`). It also forwards 127.0.0.1:3128 in the guest to vsock
//! port 3128 on the host, where wang's egress proxy applies the domain allow-list (the VM has no
//! network device at all).
//!
//! `--serve-unix PATH` serves the same protocol on a Unix socket, for tests on the host.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

pub const AGENT_PORT: u32 = 52;
pub const PROXY_PORT: u32 = 3128;
const HOST_CID: u32 = 2;
const MAX_OUTPUT: usize = 1 << 20;
const MAX_FILE: u64 = 8 << 20;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--serve-unix") {
        let path = args.get(i + 1).expect("--serve-unix PATH");
        let _ = std::fs::remove_file(path);
        let l = std::os::unix::net::UnixListener::bind(path).expect("bind");
        for c in l.incoming().flatten() {
            std::thread::spawn(move || serve(c.try_clone().unwrap(), c));
        }
        return;
    }
    if std::process::id() != 1 {
        eprintln!("wang-vm-agent runs as init inside a wang VM (or use --serve-unix PATH)");
        std::process::exit(2);
    }
    if let Err(e) = init() {
        eprintln!("wang-vm-agent: init failed: {e}");
    }
    // PID 1: keep a worker running, reap everything else.
    loop {
        let worker = unsafe { libc::fork() };
        if worker == 0 {
            worker_main();
            std::process::exit(0);
        }
        loop {
            let mut status = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
            if pid == worker || (pid < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD)) {
                break; // worker gone: start another
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn cstr(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s).unwrap()
}

fn mount(src: &str, target: &str, fstype: &str, flags: libc::c_ulong, data: &str) -> std::io::Result<()> {
    let _ = std::fs::create_dir_all(target);
    let (s, t, f, d) = (cstr(src), cstr(target), cstr(fstype), cstr(data));
    let r = unsafe { libc::mount(s.as_ptr(), t.as_ptr(), f.as_ptr(), flags, if data.is_empty() { std::ptr::null() } else { d.as_ptr().cast() }) };
    if r != 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

fn init() -> std::io::Result<()> {
    let _ = mount("proc", "/proc", "proc", 0, "");
    let _ = mount("sysfs", "/sys", "sysfs", 0, "");
    let _ = mount("devtmpfs", "/dev", "devtmpfs", 0, ""); // may already be mounted by the kernel
    // RAM overlay over the read-only image, then make it the root.
    mount("tmpfs", "/wang", "tmpfs", 0, "mode=755")?;
    for d in ["/wang/upper", "/wang/work", "/wang/root"] {
        std::fs::create_dir_all(d)?;
    }
    mount("overlay", "/wang/root", "overlay", 0, "lowerdir=/,upperdir=/wang/upper,workdir=/wang/work")?;
    for d in ["proc", "sys", "dev"] {
        let _ = mount(&format!("/{d}"), &format!("/wang/root/{d}"), "", libc::MS_MOVE, "");
    }
    std::fs::create_dir_all("/wang/root/.oldroot")?;
    std::env::set_current_dir("/wang/root")?;
    let (new, old) = (cstr("."), cstr(".oldroot"));
    if unsafe { libc::syscall(libc::SYS_pivot_root, new.as_ptr(), old.as_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let dot = cstr(".");
    unsafe { libc::chroot(dot.as_ptr()) };
    let _ = std::env::set_current_dir("/");
    let o = cstr("/.oldroot");
    unsafe { libc::umount2(o.as_ptr(), libc::MNT_DETACH) };
    let _ = std::fs::remove_dir("/.oldroot");
    let _ = mount("tmpfs", "/dev/shm", "tmpfs", 0, "mode=1777");
    let _ = mount("devpts", "/dev/pts", "devpts", 0, "");
    let _ = std::fs::create_dir_all("/work");
    let _ = std::fs::create_dir_all("/tmp");
    unsafe { libc::chmod(cstr("/tmp").as_ptr(), 0o1777) };
    let host = b"wang-vm";
    unsafe { libc::sethostname(host.as_ptr().cast(), host.len()) };
    loopback_up();
    Ok(())
}

fn loopback_up() {
    unsafe {
        let s = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if s < 0 {
            return;
        }
        let mut req: libc::ifreq = std::mem::zeroed();
        for (i, b) in b"lo".iter().enumerate() {
            req.ifr_name[i] = *b as libc::c_char;
        }
        if libc::ioctl(s, libc::SIOCGIFFLAGS, &mut req) == 0 {
            req.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
            libc::ioctl(s, libc::SIOCSIFFLAGS, &req);
        }
        libc::close(s);
    }
}

// ---- vsock ------------------------------------------------------------------------------------

fn vsock_addr(cid: u32, port: u32) -> libc::sockaddr_vm {
    let mut a: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    a.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    a.svm_cid = cid;
    a.svm_port = port;
    a
}

fn vsock_listen(port: u32) -> std::io::Result<OwnedFd> {
    unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = OwnedFd::from_raw_fd(fd);
        let a = vsock_addr(libc::VMADDR_CID_ANY, port);
        if libc::bind(fd.as_raw_fd(), (&a as *const libc::sockaddr_vm).cast(), std::mem::size_of::<libc::sockaddr_vm>() as u32) != 0
            || libc::listen(fd.as_raw_fd(), 128) != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(fd)
    }
}

fn vsock_accept(l: &OwnedFd) -> std::io::Result<std::fs::File> {
    let fd = unsafe { libc::accept4(l.as_raw_fd(), std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) };
    if fd < 0 { Err(std::io::Error::last_os_error()) } else { Ok(unsafe { std::fs::File::from_raw_fd(fd) }) }
}

fn vsock_connect(cid: u32, port: u32) -> std::io::Result<std::fs::File> {
    unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let f = std::fs::File::from_raw_fd(fd);
        let a = vsock_addr(cid, port);
        if libc::connect(fd, (&a as *const libc::sockaddr_vm).cast(), std::mem::size_of::<libc::sockaddr_vm>() as u32) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(f)
    }
}

fn worker_main() {
    std::thread::spawn(proxy_forwarder);
    let l = match vsock_listen(AGENT_PORT) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("wang-vm-agent: vsock listen: {e}");
            std::thread::sleep(Duration::from_secs(1));
            return;
        }
    };
    loop {
        match vsock_accept(&l) {
            Ok(c) => {
                std::thread::spawn(move || {
                    if let Ok(w) = c.try_clone() {
                        serve(c, w)
                    }
                });
            }
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// 127.0.0.1:3128 in the guest → host vsock 3128 (wang's egress proxy).
fn proxy_forwarder() {
    let Ok(l) = TcpListener::bind(("127.0.0.1", PROXY_PORT as u16)) else { return };
    for c in l.incoming().flatten() {
        std::thread::spawn(move || {
            let Ok(up) = vsock_connect(HOST_CID, PROXY_PORT) else { return };
            pipe(c, up);
        });
    }
}

fn pipe(tcp: TcpStream, vs: std::fs::File) {
    let (Ok(mut t2), Ok(mut v2)) = (tcp.try_clone(), vs.try_clone()) else { return };
    let (mut t1, mut v1) = (tcp, vs);
    let a = std::thread::spawn(move || {
        let _ = std::io::copy(&mut t1, &mut v1);
        unsafe { libc::shutdown(v1.as_raw_fd(), libc::SHUT_WR) };
    });
    let _ = std::io::copy(&mut v2, &mut t2);
    let _ = t2.shutdown(std::net::Shutdown::Write);
    let _ = a.join();
}

// ---- protocol ---------------------------------------------------------------------------------

fn serve<R: Read, W: Write>(r: R, mut w: W) {
    let mut lines = BufReader::new(r);
    let mut line = String::new();
    loop {
        line.clear();
        match lines.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(req) => handle(&req),
            Err(e) => json!({"ok": false, "error": format!("bad request: {e}")}),
        };
        if w.write_all(format!("{reply}\n").as_bytes()).is_err() {
            return;
        }
        let _ = w.flush();
    }
}

pub fn handle(req: &Value) -> Value {
    if let Some(now) = req["now_ms"].as_u64() {
        sync_clock(now);
    }
    let r = match req["op"].as_str().unwrap_or("") {
        "ping" => Ok(json!({"pid": std::process::id(), "uptime_ms": uptime_ms()})),
        "exec" => exec(req),
        "write" => write_file(req),
        "read" => read_file(req),
        other => Err(format!("unknown op '{other}'")),
    };
    match r {
        Ok(mut v) => {
            v["ok"] = json!(true);
            v
        }
        Err(e) => json!({"ok": false, "error": e}),
    }
}

/// A VM restored from a snapshot wakes up with the clock it had when the snapshot was taken.
/// The host sends its time with each request; step ours if it's off by more than a second
/// (only possible as root, i.e. as PID 1's worker inside the VM).
fn sync_clock(now_ms: u64) {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    let ours = ts.tv_sec as i64 * 1000 + ts.tv_nsec as i64 / 1_000_000;
    if (ours - now_ms as i64).abs() > 1000 && unsafe { libc::geteuid() } == 0 {
        let t = libc::timespec { tv_sec: (now_ms / 1000) as libc::time_t, tv_nsec: ((now_ms % 1000) * 1_000_000) as libc::c_long };
        unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &t) };
    }
}

fn uptime_ms() -> u64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
        .map(|s| (s * 1000.0) as u64)
        .unwrap_or(0)
}

fn read_capped(mut r: impl Read + Send + 'static) -> std::thread::JoinHandle<(Vec<u8>, bool)> {
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut buf = [0u8; 65536];
        let mut cut = false;
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = MAX_OUTPUT.saturating_sub(out.len());
                    out.extend_from_slice(&buf[..n.min(room)]);
                    cut |= n > room;
                }
            }
        }
        (out, cut)
    })
}

fn exec(req: &Value) -> Result<Value, String> {
    let cmd = req["cmd"].as_str().ok_or("exec needs 'cmd'")?;
    let cwd = req["cwd"].as_str().unwrap_or("/work");
    let timeout = Duration::from_millis(req["timeout_ms"].as_u64().unwrap_or(120_000));
    let mut c = Command::new("/bin/sh");
    c.arg("-c").arg(cmd).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    c.env_clear()
        .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
        .env("HOME", "/root")
        .env("LANG", "C.UTF-8")
        .env("TERM", "dumb");
    if let Some(env) = req["env"].as_object() {
        for (k, v) in env {
            if let Some(v) = v.as_str() {
                c.env(k, v);
            }
        }
    }
    c.process_group(0);
    let started = Instant::now();
    let mut child = c.spawn().map_err(|e| format!("spawn: {e}"))?;
    let out = read_capped(child.stdout.take().unwrap());
    let err = read_capped(child.stderr.take().unwrap());
    let mut timed_out = false;
    let status = loop {
        if let Some(st) = child.try_wait().map_err(|e| e.to_string())? {
            break st;
        }
        if started.elapsed() > timeout {
            timed_out = true;
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            break child.wait().map_err(|e| e.to_string())?;
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    // Background processes left holding the pipes would block the readers: kill the group.
    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    let (stdout, out_cut) = out.join().unwrap_or_default();
    let (stderr, err_cut) = err.join().unwrap_or_default();
    use std::os::unix::process::ExitStatusExt;
    let code = status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    Ok(json!({
        "exit": code,
        "stdout": String::from_utf8_lossy(&stdout),
        "stderr": String::from_utf8_lossy(&stderr),
        "truncated": out_cut || err_cut,
        "timed_out": timed_out,
        "ms": started.elapsed().as_millis() as u64,
    }))
}

fn write_file(req: &Value) -> Result<Value, String> {
    let path = req["path"].as_str().ok_or("write needs 'path'")?;
    let content = req["content"].as_str().ok_or("write needs 'content'")?;
    if let Some(dir) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(path, content).map_err(|e| format!("{path}: {e}"))?;
    if let Some(mode) = req["mode"].as_u64() {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode as u32));
    }
    Ok(json!({"bytes": content.len()}))
}

fn read_file(req: &Value) -> Result<Value, String> {
    let path = req["path"].as_str().ok_or("read needs 'path'")?;
    let meta = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?;
    if meta.len() > MAX_FILE {
        return Err(format!("{path} is {} bytes (limit {MAX_FILE})", meta.len()));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let content = String::from_utf8(bytes).map_err(|_| format!("{path} is not UTF-8 text"))?;
    Ok(json!({"content": content}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_write_read_errors() {
        let dir = std::env::temp_dir().join(format!("wang-agent-{}", std::process::id()));
        let d = dir.to_str().unwrap();
        let r = handle(&json!({"op": "write", "path": format!("{d}/a/b.txt"), "content": "hi\n"}));
        assert_eq!(r["ok"], true, "{r}");
        let r = handle(&json!({"op": "read", "path": format!("{d}/a/b.txt")}));
        assert_eq!(r["content"], "hi\n");
        let r = handle(&json!({"op": "exec", "cmd": "cat a/b.txt; echo err >&2; exit 3", "cwd": d}));
        assert_eq!((r["exit"].as_i64(), r["stdout"].as_str(), r["stderr"].as_str()), (Some(3), Some("hi\n"), Some("err\n")));
        let r = handle(&json!({"op": "exec", "cmd": "sleep 5 & sleep 5", "cwd": d, "timeout_ms": 200}));
        assert_eq!(r["timed_out"], true, "{r}");
        assert!(r["ms"].as_u64().unwrap() < 2000, "background child must not hold the reply: {r}");
        let r = handle(&json!({"op": "exec", "cmd": "echo $FOO", "cwd": d, "env": {"FOO": "bar"}}));
        assert_eq!(r["stdout"], "bar\n");
        assert_eq!(handle(&json!({"op": "nope"}))["ok"], false);
        assert!(handle(&json!({"op": "read", "path": "/nonexistent"}))["error"].as_str().unwrap().contains("nonexistent"));
        std::fs::remove_dir_all(dir).ok();
    }
}

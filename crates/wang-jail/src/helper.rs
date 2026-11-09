//! Code that runs *inside* the jail. The host binary (`wang` or `jail`) is bind-mounted at
//! `/run/wang/helper` and re-executed with a hidden first argument.
//!
//! - `__wang-jail-exec [--fwd] [--pipe-fds] -- CMD...`: optionally start the loopback →
//!   egress-proxy forwarder, optionally hand our stdin/stdout to CMD as fds 3/4 (CMD's own
//!   stdout then goes to stderr), then run CMD and exit with its status.
//! - `__wang-jail-read PATH OFFSET LIMIT`: print a text file (or a line range), so `read_file`
//!   sees exactly what the jail sees.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;

pub const HELPER_PATH: &str = "/run/wang/helper";
pub const PROXY_SOCKET: &str = "/run/wang/proxy.sock";
pub const PROXY_PORT: u16 = 3128;
pub const EXEC: &str = "__wang-jail-exec";
/// Helper flag: give the command our stdin/stdout as fds 3/4 (`Io::Pipe`).
pub const PIPE_FDS: &str = "--pipe-fds";
/// Helper flag: mark the command's memory mergeable by KSM (inherited across fork and exec).
pub const KSM: &str = "--ksm";
pub const READ: &str = "__wang-jail-read";
pub const MAX_READ_BYTES: u64 = 5 << 20;

/// Call first thing in `main()` of any binary that can act as the helper.
/// Returns only if argv isn't a helper invocation.
pub fn dispatch() {
    let args: Vec<String> = std::env::args().collect();
    let code = match args.get(1).map(String::as_str) {
        Some(EXEC) => exec(&args[2..]),
        Some(READ) => read(&args[2..]),
        _ => return,
    };
    std::process::exit(code);
}

fn exec(args: &[String]) -> i32 {
    let Some(sep) = args.iter().position(|a| a == "--") else {
        eprintln!("wang-jail helper: missing --");
        return 125;
    };
    let fwd = args[..sep].iter().any(|a| a == "--fwd");
    let pipe_fds = args[..sep].iter().any(|a| a == PIPE_FDS);
    if args[..sep].iter().any(|a| a == KSM) {
        // SAFETY: plain prctl; failure (old kernel) just means no merging.
        unsafe { libc::prctl(libc::PR_SET_MEMORY_MERGE, 1, 0, 0, 0) };
    }
    let cmd = &args[sep + 1..];
    if cmd.is_empty() {
        eprintln!("wang-jail helper: no command");
        return 125;
    }
    let mut command = std::process::Command::new(&cmd[0]);
    command.args(&cmd[1..]);
    if pipe_fds {
        // Before the forwarder opens sockets, so fds 3 and 4 are free to take. dup2'd fds
        // are not close-on-exec, so CMD inherits them.
        // SAFETY: plain fd syscalls, single-threaded at this point.
        unsafe {
            if libc::dup2(0, 3) < 0 || libc::dup2(1, 4) < 0 {
                eprintln!("wang-jail helper: dup2: {}", std::io::Error::last_os_error());
                return 125;
            }
        }
        let Ok(err) = std::io::stderr().as_fd().try_clone_to_owned() else { return 125 };
        command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::from(err));
    }
    if fwd {
        if let Err(e) = start_forwarder() {
            eprintln!("wang-jail helper: proxy forwarder failed: {e}");
            return 125;
        }
    }
    match command.status() {
        Ok(st) => st.code().unwrap_or_else(|| 128 + st.signal().unwrap_or(0)),
        Err(e) => {
            eprintln!("{}: {e}", cmd[0]);
            if e.kind() == std::io::ErrorKind::NotFound { 127 } else { 126 }
        }
    }
}

fn start_forwarder() -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", PROXY_PORT))?;
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            std::thread::spawn(move || {
                if let Ok(up) = UnixStream::connect(PROXY_SOCKET) {
                    pipe(conn, up);
                }
            });
        }
    });
    Ok(())
}

fn pipe(tcp: TcpStream, unix: UnixStream) {
    let (Ok(mut t2), Ok(mut u2)) = (tcp.try_clone(), unix.try_clone()) else { return };
    let (mut t1, mut u1) = (tcp, unix);
    let a = std::thread::spawn(move || {
        let _ = std::io::copy(&mut t1, &mut u1);
        let _ = u1.shutdown(std::net::Shutdown::Write);
    });
    let _ = std::io::copy(&mut u2, &mut t2);
    let _ = t2.shutdown(std::net::Shutdown::Write);
    let _ = a.join();
}

fn read(args: &[String]) -> i32 {
    let (Some(path), offset, limit) = (
        args.first(),
        args.get(1).and_then(|s| s.parse::<usize>().ok()).unwrap_or(1).max(1),
        args.get(2).and_then(|s| s.parse::<usize>().ok()).filter(|l| *l > 0),
    ) else {
        eprintln!("usage: {READ} PATH [OFFSET] [LIMIT]");
        return 2;
    };
    match read_text(path, offset, limit) {
        Ok(s) => {
            let _ = std::io::stdout().write_all(s.as_bytes());
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

fn read_text(path: &str, offset: usize, limit: Option<usize>) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?;
    if meta.is_dir() {
        return Err(format!("{path} is a directory; use shell with ls"));
    }
    if meta.len() > MAX_READ_BYTES {
        return Err(format!("{path} is {} bytes (limit {MAX_READ_BYTES}); use shell with head/tail/grep", meta.len()));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path).and_then(|mut f| f.read_to_end(&mut bytes)).map_err(|e| format!("{path}: {e}"))?;
    if bytes.contains(&0) {
        return Err(format!("{path} looks like a binary file"));
    }
    let text = String::from_utf8_lossy(&bytes);
    if offset == 1 && limit.is_none() {
        return Ok(text.into_owned());
    }
    let total = text.lines().count();
    let selected: Vec<&str> = text.lines().skip(offset - 1).take(limit.unwrap_or(usize::MAX)).collect();
    let end = offset - 1 + selected.len();
    Ok(format!("[lines {offset}-{end} of {total}]\n{}", selected.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_text_paging() {
        let p = std::env::temp_dir().join(format!("wang-helper-{}.txt", std::process::id()));
        std::fs::write(&p, "a\nb\nc\nd\n").unwrap();
        let ps = p.to_str().unwrap();
        assert_eq!(read_text(ps, 1, None).unwrap(), "a\nb\nc\nd\n");
        assert_eq!(read_text(ps, 2, Some(2)).unwrap(), "[lines 2-3 of 4]\nb\nc");
        std::fs::write(&p, b"a\0b").unwrap();
        assert!(read_text(ps, 1, None).unwrap_err().contains("binary"));
        std::fs::remove_file(&p).unwrap();
        assert!(read_text("/nonexistent/x", 1, None).is_err());
    }
}

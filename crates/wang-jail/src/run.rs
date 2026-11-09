//! Running a command in the jail.
//!
//! Layers, outside in:
//! 1. cgroup (joined before exec): RAM with no swap, CPU quota, process count; kill-all on timeout
//! 2. bubblewrap: new user, mount, PID, network, IPC and UTS namespaces; a fresh root with
//!    /usr and /etc read-only, private /tmp and /proc, only the declared paths visible,
//!    secrets hidden; nested user namespaces disabled; new session (no TTY injection)
//! 3. seccomp deny-list (loaded by bwrap right before exec)
//! 4. network: loopback only; declared domains go through the egress proxy

use std::ffi::{CString, OsString};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::cgroup::{CgroupRoot, Usage};
use crate::helper::{EXEC, HELPER_PATH, KSM, PIPE_FDS, PROXY_PORT, PROXY_SOCKET};
use crate::policy::{Policy, hidden_paths};
use crate::proxy::EgressProxy;

const SECCOMP_FD: i32 = 10;
const INFO_FD: i32 = 11;
static RUNS: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Io {
    /// Capture stdout/stderr (tools).
    Capture,
    /// Pass the terminal through (`jail run`).
    Inherit,
    /// Long-running process driven over pipes ([`Jail::spawn_piped`]): inside the jail the
    /// command's fd 3 reads what the caller writes and its fd 4 is what the caller reads
    /// (Chrome's `--remote-debugging-pipe` convention); its stdout and stderr are captured.
    Pipe,
}

#[derive(Debug, Clone, Default)]
pub struct JailResult {
    /// Exit code; signals are reported as 128 + signal.
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub timed_out: bool,
    pub usage: Usage,
    /// Time from spawn until bubblewrap reported the sandbox ready.
    pub setup_ms: Option<f64>,
    pub wall_ms: f64,
    pub blocked_hosts: Vec<String>,
    pub allowed_hosts: Vec<String>,
    /// Declared paths that don't exist on this machine (skipped).
    pub missing_paths: Vec<PathBuf>,
}

impl JailResult {
    pub fn oom_killed(&self) -> bool {
        self.usage.oom_kills > 0
    }

    /// Plain-language notes about what the sandbox did, for the model and for humans.
    pub fn notes(&self, policy: &Policy) -> Vec<String> {
        let mut n = vec![];
        if self.timed_out {
            n.push(format!("killed after the {:.0}s time limit", policy.limits.timeout.as_secs_f64()));
        }
        if self.oom_killed() {
            n.push(format!("killed: memory limit of {} MiB exceeded", policy.limits.memory_bytes >> 20));
        }
        if self.usage.pids_limit_hits > 0 {
            n.push(format!(
                "process limit of {} reached ({} fork(s) refused)",
                policy.limits.pids, self.usage.pids_limit_hits
            ));
        }
        for h in &self.blocked_hosts {
            match h.strip_suffix(" (private address)") {
                Some(h) => n.push(format!("network access to {h} blocked (private address; list the host exactly to allow it)")),
                None => n.push(format!("network access to {h} blocked (not in the allow-list)")),
            }
        }
        if self.stdout_truncated || self.stderr_truncated {
            n.push(format!("output truncated at {} MiB", policy.limits.max_output_bytes >> 20));
        }
        n
    }
}

pub struct Jail {
    pub bwrap: PathBuf,
    pub helper: PathBuf,
    pub cgroups: CgroupRoot,
    pub rundir: PathBuf,
    seccomp_path: PathBuf,
    /// Caps concurrent cgroup-create + fork/exec. Each fork from this multi-threaded process
    /// holds its memory-map lock while copying page tables; hundreds at once stalled every
    /// thread (the event loop included) for seconds, while the kernel only creates sandboxes
    /// at ~75/s anyway (bench/m5-scale.md).
    spawn_slots: Arc<tokio::sync::Semaphore>,
    spawner: Spawner,
}

/// Long-lived threads that fork bubblewrap. bwrap's `--die-with-parent` (PR_SET_PDEATHSIG)
/// fires when the *thread* that forked it exits, and tokio's blocking-pool threads exit after
/// 10 s idle: jailed commands spawned from there were SIGKILLed at ~10 s. These threads live as
/// long as the `Jail`.
struct Spawner {
    tx: Mutex<Option<std::sync::mpsc::Sender<Box<dyn FnOnce() + Send>>>>,
}

impl Spawner {
    fn new(threads: usize) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<Box<dyn FnOnce() + Send>>();
        let rx = Arc::new(Mutex::new(rx));
        for i in 0..threads {
            let rx = rx.clone();
            let _ = std::thread::Builder::new().name(format!("wang-jail-spawn-{i}")).spawn(move || {
                loop {
                    let job = match rx.lock().unwrap().recv() {
                        Ok(job) => job,
                        Err(_) => return,
                    };
                    job();
                }
            });
        }
        Self { tx: Mutex::new(Some(tx)) }
    }

    /// Run `f` on a spawner thread (inside the current tokio runtime's context).
    async fn run<T: Send + 'static>(&self, f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
        let handle = tokio::runtime::Handle::current();
        let (done, rx) = tokio::sync::oneshot::channel();
        let job: Box<dyn FnOnce() + Send> = Box::new(move || {
            let _ctx = handle.enter();
            let _ = done.send(f());
        });
        self.tx.lock().unwrap().as_ref().context("jail spawner stopped")?.send(job).ok().context("jail spawner stopped")?;
        rx.await.context("jail spawner thread failed")
    }
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
}

impl Jail {
    /// The helper is our own executable: `wang` and `jail` both call `helper::dispatch()` first.
    /// `WANG_JAIL_HELPER` overrides it.
    pub fn new() -> Result<Self> {
        let helper = match std::env::var_os("WANG_JAIL_HELPER") {
            Some(p) => PathBuf::from(p),
            None => std::env::current_exe().context("locating own executable")?,
        };
        Self::with_helper(helper)
    }

    pub fn with_helper(helper: PathBuf) -> Result<Self> {
        let bwrap = std::env::var_os("WANG_BWRAP")
            .map(PathBuf::from)
            .or_else(|| which("bwrap"))
            .or_else(|| Some(PathBuf::from("/usr/bin/bwrap")).filter(|p| p.exists()))
            .context("bubblewrap (bwrap) not found: install it with `sudo apt install bubblewrap`")?;
        let cgroups = CgroupRoot::discover()?;
        let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        // One directory per Jail: dropping a Jail removes its directory, which must not pull the
        // seccomp program and proxy sockets out from under another Jail in the same process.
        static INSTANCES: AtomicU64 = AtomicU64::new(0);
        let rundir = base.join(format!("wang-jail-{}-{}", std::process::id(), INSTANCES.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&rundir)?;
        std::fs::set_permissions(&rundir, std::fs::Permissions::from_mode(0o700))?;
        let seccomp_path = rundir.join("seccomp.bpf");
        std::fs::write(&seccomp_path, crate::seccomp::compile()?)?;
        let slots = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        Ok(Self {
            bwrap,
            helper,
            cgroups,
            rundir,
            seccomp_path,
            spawn_slots: Arc::new(tokio::sync::Semaphore::new(slots)),
            spawner: Spawner::new(slots),
        })
    }

    /// Run `true` in a jail; errors explain what's missing.
    pub async fn check(&self) -> Result<()> {
        let r = self.run(&Policy::default(), &["true".to_string()], Io::Capture).await?;
        if r.exit_code != 0 {
            bail!("sandbox self-check failed (exit {}): {}", r.exit_code, String::from_utf8_lossy(&r.stderr).trim());
        }
        Ok(())
    }

    async fn start(&self, policy: &Policy, argv: &[String], io: Io) -> Result<Started> {
        if argv.is_empty() {
            bail!("no command");
        }
        let n = RUNS.fetch_add(1, Ordering::Relaxed);
        let proxy = if policy.domains.is_empty() {
            None
        } else {
            Some(EgressProxy::start(&self.rundir.join(format!("proxy-{n}.sock")), policy.domains.clone())?)
        };
        let plan = build_args(policy, &self.helper, proxy.as_ref().map(|p| p.socket.as_path()), argv, io == Io::Pipe);

        // cgroup setup and fork/exec are blocking syscalls (the parent waits until the child has
        // exec'd); on the runtime thread they serialized every jailed call under load and stalled
        // the daemon's event loop (bench/m5-scale.md). They run on the spawner threads instead.
        let (cgroups, limits, args, bwrap, seccomp_path) =
            (self.cgroups.clone(), policy.limits, plan.args.clone(), self.bwrap.clone(), self.seccomp_path.clone());
        let permit = self.spawn_slots.clone().acquire_owned().await.context("jail spawn slots closed")?;
        let spawned = self.spawner.run(move || {
            let cg = cgroups.create(&limits)?;
            let procs_file = cg.procs_file();
            let r = (|| -> Result<_> {
                // Info pipe: bwrap writes to it once the sandbox is set up.
                let mut fds = [0i32; 2];
                if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
                    return Err(anyhow::anyhow!("pipe2: {}", std::io::Error::last_os_error()));
                }
                let (info_r, info_w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };

                let procs = CString::new(procs_file.as_os_str().as_bytes())?;
                let bpf = CString::new(seccomp_path.as_os_str().as_bytes())?;
                let info_w_raw = info_w.as_raw_fd();

                let mut cmd = tokio::process::Command::new(&bwrap);
                cmd.args(&args).kill_on_drop(true);
                match io {
                    Io::Capture => cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()),
                    Io::Inherit => cmd.stdin(Stdio::null()).stdout(Stdio::inherit()).stderr(Stdio::inherit()),
                    Io::Pipe => cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()),
                };
                // SAFETY: only async-signal-safe libc calls on pre-built C strings.
                unsafe {
                    cmd.pre_exec(move || {
                        // 1. join the jail's cgroup before anything else runs
                        let fd = libc::open(procs.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
                        if fd < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        let ok = libc::write(fd, b"0".as_ptr().cast(), 1) == 1;
                        libc::close(fd);
                        if !ok {
                            return Err(std::io::Error::last_os_error());
                        }
                        // 2. info fd first: the pipe's own fd number may be SECCOMP_FD, which
                        //    the next step overwrites.
                        if libc::dup2(info_w_raw, INFO_FD) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        libc::fcntl(INFO_FD, libc::F_SETFD, 0);
                        // 3. seccomp program on a fresh fd (bwrap reads it from offset 0)
                        let s = libc::open(bpf.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
                        if s < 0 || libc::dup2(s, SECCOMP_FD) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        if s != SECCOMP_FD {
                            libc::close(s);
                        }
                        libc::fcntl(SECCOMP_FD, libc::F_SETFD, 0);
                        Ok(())
                    });
                }
                let child = cmd.spawn().context("starting bubblewrap (could not join the jail cgroup or spawn bwrap)")?;
                drop(info_w);
                Ok((child, info_r))
            })();
            match r {
                Ok((child, info_r)) => Ok((cg, child, info_r)),
                Err(e) => {
                    cg.destroy_blocking();
                    Err(e)
                }
            }
        })
        .await?;
        drop(permit);
        match spawned {
            Ok((cg, child, info_r)) => Ok(Started { cg, child, info_r, proxy, missing: plan.missing }),
            Err(e) => {
                if let Some(p) = proxy {
                    p.stop();
                }
                Err(e)
            }
        }
    }

    pub async fn run(&self, policy: &Policy, argv: &[String], io: Io) -> Result<JailResult> {
        if io == Io::Pipe {
            bail!("Io::Pipe is for Jail::spawn_piped");
        }
        let started = Instant::now();
        let Started { cg, mut child, info_r, proxy, missing } = self.start(policy, argv, io).await?;

        // Async read: a blocking thread per in-flight jail exhausted the blocking pool at scale.
        // Keep draining until bwrap closes it: dropping the read end after the first chunk let a
        // second write (bwrap's info arriving in pieces under load) kill bwrap with SIGPIPE.
        let ready = tokio::spawn(async move {
            let mut rx = tokio::net::unix::pipe::Receiver::from_owned_fd(info_r).ok()?;
            let mut b = [0u8; 512];
            let mut first = None;
            while let Ok(n) = rx.read(&mut b).await {
                if n == 0 {
                    break;
                }
                first.get_or_insert_with(Instant::now);
            }
            first
        });

        let max = policy.limits.max_output_bytes;
        let out = child.stdout.take().map(|s| tokio::spawn(read_capped(s, max)));
        let err = child.stderr.take().map(|s| tokio::spawn(read_capped(s, max)));

        let mut timed_out = false;
        let status = match tokio::time::timeout(policy.limits.timeout, child.wait()).await {
            Ok(s) => s?,
            Err(_) => {
                timed_out = true;
                cg.kill();
                child.wait().await?
            }
        };
        let wall_ms = started.elapsed().as_secs_f64() * 1e3;
        let usage = cg.finish().await;
        let (blocked_hosts, allowed_hosts) = proxy.map(|p| p.stop()).unwrap_or_default();

        let (stdout, stdout_truncated) = match out {
            Some(t) => t.await??,
            None => (vec![], false),
        };
        let (stderr, stderr_truncated) = match err {
            Some(t) => t.await??,
            None => (vec![], false),
        };
        let setup_ms = ready.await.ok().flatten().map(|t| t.duration_since(started).as_secs_f64() * 1e3);
        let exit_code = status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
        Ok(JailResult {
            exit_code,
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
            timed_out,
            usage,
            setup_ms,
            wall_ms,
            blocked_hosts,
            allowed_hosts,
            missing_paths: missing,
        })
    }
}

impl Jail {
    /// Start a long-running command (a browser) and hand back its pipes. Limits other than the
    /// timeout apply for its whole life; the caller decides when it ends ([`Running::stop`]).
    pub async fn spawn_piped(&self, policy: &Policy, argv: &[String]) -> Result<Running> {
        let started = Instant::now();
        let Started { cg, mut child, info_r, proxy, missing } = self.start(policy, argv, Io::Pipe).await?;
        // Keep reading bwrap's info pipe: closing it would kill bwrap with SIGPIPE when it
        // reports the sandbox ready.
        tokio::spawn(async move {
            if let Ok(mut rx) = tokio::net::unix::pipe::Receiver::from_owned_fd(info_r) {
                let mut b = [0u8; 512];
                while matches!(rx.read(&mut b).await, Ok(n) if n > 0) {}
            }
        });
        let stdin = child.stdin.take().context("jail stdin")?;
        let stdout = child.stdout.take().context("jail stdout")?;
        let max = policy.limits.max_output_bytes;
        let stderr = child.stderr.take().map(|s| tokio::spawn(read_capped(s, max)));
        Ok(Running { stdin: Some(stdin), stdout: Some(stdout), child, cg: Some(cg), proxy, stderr, started, missing_paths: missing })
    }
}

struct Started {
    cg: crate::cgroup::Cgroup,
    child: tokio::process::Child,
    info_r: OwnedFd,
    proxy: Option<EgressProxy>,
    missing: Vec<PathBuf>,
}

/// A jailed process started with [`Jail::spawn_piped`]. Dropping it kills everything in its
/// cgroup.
pub struct Running {
    /// Write end of the command's fd 3.
    pub stdin: Option<tokio::process::ChildStdin>,
    /// Read end of the command's fd 4.
    pub stdout: Option<tokio::process::ChildStdout>,
    child: tokio::process::Child,
    cg: Option<crate::cgroup::Cgroup>,
    proxy: Option<EgressProxy>,
    stderr: Option<tokio::task::JoinHandle<Result<(Vec<u8>, bool)>>>,
    pub started: Instant,
    pub missing_paths: Vec<PathBuf>,
}

/// How a [`Running`] process ended.
#[derive(Debug, Clone, Default)]
pub struct Stopped {
    pub exit_code: i32,
    pub usage: Usage,
    /// The command's stdout and stderr (capped).
    pub stderr: Vec<u8>,
    pub blocked_hosts: Vec<String>,
    pub allowed_hosts: Vec<String>,
    pub wall_ms: f64,
}

impl Running {
    /// Current memory use of everything in the jail (cgroup `memory.current`).
    pub fn memory_bytes(&self) -> Option<u64> {
        self.cg.as_ref().and_then(|c| c.memory_current())
    }

    /// Processes in the jail right now.
    pub fn pids(&self) -> Vec<u32> {
        self.cg
            .as_ref()
            .and_then(|c| std::fs::read_to_string(c.procs_file()).ok())
            .map(|s| s.lines().filter_map(|l| l.trim().parse().ok()).collect())
            .unwrap_or_default()
    }

    /// Hosts the egress proxy refused so far.
    pub fn blocked_hosts(&self) -> Vec<String> {
        self.proxy.as_ref().map(|p| p.blocked()).unwrap_or_default()
    }

    /// `Some(exit code)` once the process has exited on its own.
    pub fn try_exit_code(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            Ok(Some(st)) => Some(st.code().unwrap_or_else(|| 128 + st.signal().unwrap_or(0))),
            _ => None,
        }
    }

    /// Kill everything in the jail and collect what happened.
    pub async fn stop(mut self) -> Stopped {
        let cg = self.cg.take();
        if let Some(cg) = &cg {
            cg.kill();
        }
        let status = self.child.wait().await.ok();
        let wall_ms = self.started.elapsed().as_secs_f64() * 1e3;
        let usage = match cg {
            Some(cg) => cg.finish().await,
            None => Usage::default(),
        };
        let (blocked_hosts, allowed_hosts) = self.proxy.take().map(|p| p.stop()).unwrap_or_default();
        let stderr = match self.stderr.take() {
            Some(t) => t.await.ok().and_then(|r| r.ok()).map(|(b, _)| b).unwrap_or_default(),
            None => vec![],
        };
        let exit_code = status.map(|st| st.code().unwrap_or_else(|| 128 + st.signal().unwrap_or(0))).unwrap_or(-1);
        Stopped { exit_code, usage, stderr, blocked_hosts, allowed_hosts, wall_ms }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(cg) = self.cg.take() {
            cg.kill();
            match tokio::runtime::Handle::try_current() {
                Ok(h) => {
                    h.spawn_blocking(move || cg.destroy_blocking());
                }
                Err(_) => cg.destroy_blocking(),
            }
        }
        if let Some(p) = self.proxy.take() {
            p.stop();
        }
    }
}

impl Drop for Jail {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.rundir);
    }
}

async fn read_capped<R: AsyncRead + Unpin>(mut r: R, max: usize) -> Result<(Vec<u8>, bool)> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut truncated = false;
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let room = max.saturating_sub(out.len());
        if room > 0 {
            out.extend_from_slice(&buf[..n.min(room)]);
        }
        if n > room {
            truncated = true; // keep draining so the child never blocks on a full pipe
        }
    }
    Ok((out, truncated))
}

pub struct Plan {
    pub args: Vec<OsString>,
    pub missing: Vec<PathBuf>,
}

/// The bubblewrap command line for a policy (pure, so it can be unit tested).
/// `pipe_fds`: the command gets the jail's stdin/stdout as fds 3/4 (see [`Io::Pipe`]).
pub fn build_args(policy: &Policy, helper: &Path, proxy_socket: Option<&Path>, argv: &[String], pipe_fds: bool) -> Plan {
    let mut a: Vec<OsString> = Vec::new();
    let mut push = |xs: &[&std::ffi::OsStr]| a.extend(xs.iter().map(|x| x.to_os_string()));
    let s = |x: &str| OsString::from(x);
    let o = |p: &Path| p.as_os_str().to_os_string();
    let home = crate::policy::home();

    for f in ["--unshare-all", "--unshare-user", "--die-with-parent", "--new-session", "--disable-userns", "--clearenv"] {
        push(&[s(f).as_os_str()]);
    }
    push(&[s("--hostname").as_os_str(), s("wang-jail").as_os_str()]);
    push(&[s("--seccomp").as_os_str(), s(&SECCOMP_FD.to_string()).as_os_str()]);
    push(&[s("--info-fd").as_os_str(), s(&INFO_FD.to_string()).as_os_str()]);

    // Base system, read-only. Recreate the usr-merge symlinks as they are on the host.
    push(&[s("--ro-bind").as_os_str(), s("/usr").as_os_str(), s("/usr").as_os_str()]);
    for top in ["/bin", "/sbin", "/lib", "/lib64", "/lib32", "/libx32"] {
        let p = Path::new(top);
        if let Ok(target) = std::fs::read_link(p) {
            push(&[s("--symlink").as_os_str(), target.as_os_str(), p.as_os_str()]);
        } else if p.is_dir() {
            push(&[s("--ro-bind").as_os_str(), p.as_os_str(), p.as_os_str()]);
        }
    }
    push(&[s("--ro-bind").as_os_str(), s("/etc").as_os_str(), s("/etc").as_os_str()]);
    push(&[s("--proc").as_os_str(), s("/proc").as_os_str(), s("--dev").as_os_str(), s("/dev").as_os_str()]);
    push(&[
        s("--size").as_os_str(),
        s(&policy.limits.tmp_bytes.to_string()).as_os_str(),
        s("--tmpfs").as_os_str(),
        s("/tmp").as_os_str(),
    ]);
    push(&[s("--ro-bind").as_os_str(), helper.as_os_str(), s(HELPER_PATH).as_os_str()]);
    if let Some(sock) = proxy_socket {
        push(&[s("--bind").as_os_str(), sock.as_os_str(), s(PROXY_SOCKET).as_os_str()]);
    }
    // An empty home, so ~ exists; declared paths are mounted on top.
    push(&[s("--tmpfs").as_os_str(), home.as_os_str()]);

    // Declared paths, parents before children.
    #[derive(Clone, Copy, PartialEq)]
    enum M {
        Ro,
        Rw,
        Ov,
    }
    let mut mounts: Vec<(PathBuf, M)> = policy
        .read
        .iter()
        .map(|p| (p.clone(), M::Ro))
        .chain(policy.write.iter().map(|p| (p.clone(), M::Rw)))
        .chain(policy.overlay.iter().map(|p| (p.clone(), M::Ov)))
        .collect();
    mounts.sort_by_key(|(p, _)| p.components().count());
    let mut missing = vec![];
    let mut visible: Vec<PathBuf> = vec![];
    for (p, m) in mounts {
        if !p.exists() {
            missing.push(p);
            continue;
        }
        match m {
            M::Ro => push(&[s("--ro-bind").as_os_str(), p.as_os_str(), p.as_os_str()]),
            M::Rw => push(&[s("--bind").as_os_str(), p.as_os_str(), p.as_os_str()]),
            M::Ov if p.is_dir() => {
                push(&[s("--overlay-src").as_os_str(), p.as_os_str(), s("--tmp-overlay").as_os_str(), p.as_os_str()])
            }
            M::Ov => push(&[s("--ro-bind").as_os_str(), p.as_os_str(), p.as_os_str()]),
        }
        visible.push(p);
    }

    // Explicit host → jail mounts, and devices.
    for b in &policy.binds {
        if !b.src.exists() {
            missing.push(b.src.clone());
            continue;
        }
        let flag = if b.writable { "--bind" } else { "--ro-bind" };
        push(&[s(flag).as_os_str(), b.src.as_os_str(), b.dst.as_os_str()]);
    }
    for d in &policy.devices {
        if d.exists() {
            push(&[s("--dev-bind").as_os_str(), d.as_os_str(), d.as_os_str()]);
        } else {
            missing.push(d.clone());
        }
    }

    // Secrets: hidden wherever a parent is visible.
    for h in hidden_paths(&policy.hide) {
        if !visible.iter().any(|v| h.starts_with(v)) {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&h) else { continue };
        if meta.is_dir() {
            push(&[s("--tmpfs").as_os_str(), h.as_os_str()]);
        } else {
            push(&[s("--ro-bind").as_os_str(), s("/dev/null").as_os_str(), h.as_os_str()]);
        }
    }

    // Environment: minimal, plus host PATH entries that are visible in the jail.
    let host_path = std::env::var("PATH").unwrap_or_default();
    let mut path: Vec<String> = vec!["/usr/local/bin".into(), "/usr/bin".into(), "/bin".into(), "/usr/sbin".into(), "/sbin".into()];
    for d in host_path.split(':') {
        let dp = Path::new(d);
        if !d.is_empty() && !path.iter().any(|x| x == d) && visible.iter().any(|v| dp.starts_with(v)) {
            path.insert(0, d.to_string());
        }
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    let mut env: Vec<(String, String)> = vec![
        ("PATH".into(), path.join(":")),
        ("HOME".into(), home.to_string_lossy().into_owned()),
        ("USER".into(), user.clone()),
        ("LOGNAME".into(), user),
        ("LANG".into(), std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".into())),
        ("TMPDIR".into(), "/tmp".into()),
        ("TERM".into(), "dumb".into()),
        ("WANG_JAIL".into(), "1".into()),
    ];
    if proxy_socket.is_some() {
        let url = format!("http://127.0.0.1:{PROXY_PORT}");
        for k in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"] {
            env.push((k.into(), url.clone()));
        }
        env.push(("NO_PROXY".into(), "localhost,127.0.0.1".into()));
        env.push(("no_proxy".into(), "localhost,127.0.0.1".into()));
    }
    env.extend(policy.env.iter().cloned());
    for (k, v) in &env {
        push(&[s("--setenv").as_os_str(), s(k).as_os_str(), s(v).as_os_str()]);
    }

    let cwd = policy
        .cwd
        .clone()
        .filter(|c| visible.iter().any(|v| c.starts_with(v)) && !crate::policy::is_hidden(c, &policy.hide))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    push(&[s("--chdir").as_os_str(), cwd.as_os_str()]);

    push(&[s("--").as_os_str(), s(HELPER_PATH).as_os_str(), s(EXEC).as_os_str()]);
    if proxy_socket.is_some() {
        push(&[s("--fwd").as_os_str()]);
    }
    if pipe_fds {
        push(&[s(PIPE_FDS).as_os_str()]);
    }
    if policy.ksm {
        push(&[s(KSM).as_os_str()]);
    }
    push(&[s("--").as_os_str()]);
    a.extend(argv.iter().map(OsString::from));
    let _ = o;
    Plan { args: a, missing }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::home;

    fn args_str(p: &Plan) -> Vec<String> {
        p.args.iter().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    fn has_seq(v: &[String], seq: &[&str]) -> bool {
        v.windows(seq.len()).any(|w| w.iter().zip(seq).all(|(a, b)| a == b))
    }

    #[test]
    fn args_isolate_and_hide() {
        let dir = std::env::temp_dir().join(format!("wang-args-{}", std::process::id()));
        let secret = dir.join("secret");
        std::fs::create_dir_all(&secret).unwrap();
        let pol = Policy {
            read: vec![dir.clone(), PathBuf::from("/definitely/missing")],
            hide: vec![secret.clone()],
            ..Default::default()
        };
        let plan = build_args(&pol, Path::new("/x/wang"), None, &["echo".into(), "hi".into()], false);
        let v = args_str(&plan);
        for f in ["--unshare-all", "--unshare-user", "--die-with-parent", "--new-session", "--disable-userns", "--clearenv"] {
            assert!(v.contains(&f.to_string()), "{f}");
        }
        assert!(!v.contains(&"--share-net".to_string()));
        assert!(has_seq(&v, &["--ro-bind", "/usr", "/usr"]));
        assert!(has_seq(&v, &["--tmpfs", home().to_str().unwrap()]));
        let d = dir.to_str().unwrap().to_string();
        assert!(has_seq(&v, &["--ro-bind", &d, &d]));
        assert!(has_seq(&v, &["--tmpfs", secret.to_str().unwrap()]));
        assert_eq!(plan.missing, vec![PathBuf::from("/definitely/missing")]);
        assert!(has_seq(&v, &["--", HELPER_PATH, EXEC, "--", "echo", "hi"]));
        assert!(!v.iter().any(|x| x.contains("HTTP_PROXY")));
        // the hide comes after the parent mount
        let pos = |seq: &[&str]| v.windows(seq.len()).position(|w| w.iter().zip(seq).all(|(a, b)| a == b)).unwrap();
        assert!(pos(&["--tmpfs", secret.to_str().unwrap()]) > pos(&["--ro-bind", &d, &d]));
        std::fs::remove_dir_all(dir).ok();
    }

    /// Regression: bwrap's --die-with-parent killed jailed commands once the tokio blocking-pool
    /// thread that forked them exited (10 s idle by default; 100 ms here).
    #[test]
    fn long_commands_outlive_idle_runtime_threads() {
        let helper = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("jail");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .thread_keep_alive(std::time::Duration::from_millis(100))
            .build()
            .unwrap();
        rt.block_on(async {
            let jail = match Jail::with_helper(helper.clone()) {
                Ok(j) if helper.exists() && j.check().await.is_ok() => j,
                _ => {
                    eprintln!("skipping: sandbox or {} not available", helper.display());
                    return;
                }
            };
            let r = jail.run(&Policy::default(), &["sleep".into(), "1".into()], Io::Capture).await.unwrap();
            assert_eq!(r.exit_code, 0, "killed: {r:?}");
        });
    }

    /// Regression: bwrap writes its --info-fd message in several writes; reading only the first
    /// chunk and closing the pipe killed bwrap with SIGPIPE (exit 141) when the reads interleaved,
    /// which happens under load. Many jails at once make that likely.
    #[test]
    fn many_concurrent_jails_all_complete() {
        let helper = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join("jail");
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let jail = match Jail::with_helper(helper.clone()) {
                Ok(j) if helper.exists() && j.check().await.is_ok() => j,
                _ => {
                    eprintln!("skipping: sandbox or {} not available", helper.display());
                    return;
                }
            };
            let argv: Vec<String> = vec!["sh".into(), "-c".into(), "echo ok".into()];
            let pol = Policy::default();
            let runs = futures::future::join_all((0..64).map(|_| jail.run(&pol, &argv, Io::Capture))).await;
            let bad: Vec<String> = runs
                .into_iter()
                .filter_map(|r| match r {
                    Ok(r) if r.exit_code == 0 && r.stdout == b"ok\n" => None,
                    Ok(r) => Some(format!("exit {} out {:?}", r.exit_code, String::from_utf8_lossy(&r.stdout))),
                    Err(e) => Some(e.to_string()),
                })
                .collect();
            assert!(bad.is_empty(), "{} of 64 jails failed: {:?}", bad.len(), &bad[..bad.len().min(5)]);
        });
    }

    #[test]
    fn args_with_proxy() {
        let pol = Policy { domains: vec!["example.com".into()], ..Default::default() };
        let v = args_str(&build_args(&pol, Path::new("/x/wang"), Some(Path::new("/run/u/p.sock")), &["curl".into()], true));
        assert!(has_seq(&v, &["--bind", "/run/u/p.sock", PROXY_SOCKET]));
        assert!(has_seq(&v, &["--setenv", "HTTPS_PROXY", "http://127.0.0.1:3128"]));
        assert!(has_seq(&v, &[EXEC, "--fwd", PIPE_FDS, "--", "curl"]));
    }
}

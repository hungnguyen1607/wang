//! `jail`: the wang sandbox as a standalone tool.

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use wang_jail::policy::{expand, parse_duration, parse_size};
use wang_jail::{Io, Jail, Manifest, Policy, bench, selftest};

#[derive(Parser)]
#[command(name = "jail", version, about = "Run a command with only the files, domains and resources it declares")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a command in the jail
    Run {
        /// Manifest (TOML: [files] read/write/overlay/hide/cwd, [network] domains, [limits] memory/cpus/pids/timeout/tmp)
        #[arg(long, short)]
        manifest: Option<PathBuf>,
        /// Visible read-only (repeatable)
        #[arg(long = "ro", value_name = "PATH")]
        read: Vec<String>,
        /// Visible read-write (repeatable)
        #[arg(long = "rw", value_name = "PATH")]
        write: Vec<String>,
        /// Visible copy-on-write; changes are discarded (repeatable)
        #[arg(long, value_name = "PATH")]
        overlay: Vec<String>,
        /// Hide this path (repeatable)
        #[arg(long, value_name = "PATH")]
        hide: Vec<String>,
        /// Allow network access to this domain (repeatable; *.example.com, host:port, *)
        #[arg(long = "allow-net", value_name = "DOMAIN")]
        domains: Vec<String>,
        #[arg(long)]
        memory: Option<String>,
        #[arg(long)]
        cpus: Option<f64>,
        #[arg(long)]
        pids: Option<u32>,
        #[arg(long)]
        timeout: Option<String>,
        /// Working directory (must be visible)
        #[arg(long)]
        cwd: Option<String>,
        /// Print the sandbox report to stderr afterwards
        #[arg(long, short)]
        report: bool,
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Run the escape test suite
    Selftest {
        /// Skip the check that needs internet access
        #[arg(long)]
        offline: bool,
    },
    /// Measure the sandbox overhead per call
    Bench {
        #[arg(short, default_value_t = 30)]
        n: usize,
    },
    /// Check that the sandbox works on this machine
    Check,
}

fn main() {
    wang_jail::helper::dispatch();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    match rt.block_on(real_main()) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("jail: {e:#}");
            std::process::exit(125);
        }
    }
}

async fn real_main() -> Result<i32> {
    let cli = Cli::parse();
    let jail = Jail::new()?;
    match cli.cmd {
        Cmd::Check => {
            jail.check().await?;
            println!("ok: bwrap {} · cgroups at {}", jail.bwrap.display(), jail.cgroups.path.display());
            Ok(0)
        }
        Cmd::Selftest { offline } => {
            let checks = selftest::run_all(&jail, !offline).await?;
            let (text, all_ok) = selftest::report(&checks);
            print!("{text}");
            Ok(if all_ok { 0 } else { 1 })
        }
        Cmd::Bench { n } => {
            print!("{}", bench::run(&jail, n).await?);
            Ok(0)
        }
        Cmd::Run { manifest, read, write, overlay, hide, domains, memory, cpus, pids, timeout, cwd, report, command } => {
            let mut p = match manifest {
                Some(m) => Manifest::load(&m)?.into_policy()?,
                None => Policy::default(),
            };
            p.read.extend(read.iter().map(|x| expand(x)));
            p.write.extend(write.iter().map(|x| expand(x)));
            p.overlay.extend(overlay.iter().map(|x| expand(x)));
            p.hide.extend(hide.iter().map(|x| expand(x)));
            p.domains.extend(domains);
            if let Some(m) = memory {
                p.limits.memory_bytes = parse_size(&m)?;
            }
            if let Some(c) = cpus {
                p.limits.cpus = c;
            }
            if let Some(n) = pids {
                p.limits.pids = n;
            }
            if let Some(t) = timeout {
                p.limits.timeout = parse_duration(&t)?;
            }
            if let Some(c) = cwd {
                p.cwd = Some(expand(&c));
            } else if p.cwd.is_none() {
                p.cwd = std::env::current_dir().ok();
            }
            if command.is_empty() {
                bail!("no command");
            }
            let r = jail.run(&p, &command, Io::Inherit).await?;
            let notes = r.notes(&p);
            if report || !notes.is_empty() {
                for m in &r.missing_paths {
                    eprintln!("jail: skipped missing path {}", m.display());
                }
                for n in &notes {
                    eprintln!("jail: {n}");
                }
                if report {
                    eprintln!(
                        "jail: exit {} · {:.1} ms (setup {:.1} ms) · peak RAM {} KiB · CPU {:.1} ms",
                        r.exit_code,
                        r.wall_ms,
                        r.setup_ms.unwrap_or(0.0),
                        r.usage.peak_memory_bytes.unwrap_or(0) >> 10,
                        r.usage.cpu_usec.unwrap_or(0) as f64 / 1e3
                    );
                }
            }
            Ok(r.exit_code)
        }
    }
}

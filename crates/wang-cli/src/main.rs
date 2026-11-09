mod setup;

use std::io::{IsTerminal, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;
use tokio::io::AsyncBufReadExt;
use wang_daemon::config::{self, Config};
use wang_daemon::protocol::{ClientMsg, ServerMsg};
use wang_daemon::server::Client;
use wang_tokens::Ledger;
use wang_trace::Tracer;

#[derive(Parser)]
#[command(name = "wang", version, about = "Secure, lightweight, token-cheap laptop agent")]
struct Cli {
    /// Config file (default: ~/.config/wang/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// No command: first-run setup if needed, then chat (starting the daemon if it isn't running)
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Guided first run: checks, Claude Code hookup, API key, background service (asks before each)
    Setup,
    /// What works on this machine and how to fix what doesn't (changes nothing)
    Doctor,
    /// Create ~/.config/wang/config.toml and ~/.config/wang/.env
    Init,
    /// Run the daemon in the foreground (what the systemd service runs)
    Run,
    /// Talk to the daemon. With a message: one-shot. Without: interactive.
    Chat {
        /// Message to send (one-shot mode)
        message: Vec<String>,
        /// Conversation to use; the daemon keeps history per session while it runs
        #[arg(long, short, default_value = "cli")]
        session: String,
        /// Start a fresh conversation first
        #[arg(long)]
        new: bool,
    },
    /// Tokens and $ spent
    Cost {
        #[arg(value_enum, default_value = "today")]
        period: Period,
    },
    /// Agent health: success rate, cost per success, failing tools
    Stats {
        #[arg(value_enum, default_value = "week")]
        period: StatsPeriod,
    },
    /// Push notifications (ntfy)
    Notify {
        #[command(subcommand)]
        action: NotifyAction,
    },
    /// Step-by-step timeline of a task (default: the latest)
    Trace { task: Option<i64> },
    /// Rate a task 👍 / 👎 (default: the latest)
    Rate {
        #[arg(value_enum)]
        verdict: Verdict,
        task: Option<i64>,
    },
    /// Run the eval suite (default: replay recorded model calls, $0)
    Eval {
        /// Call the real model and (re)record cassettes
        #[arg(long)]
        live: bool,
        /// Route later steps to [routing] cheap_model (compare with a run without it)
        #[arg(long)]
        routing: bool,
        /// Run only this task id
        #[arg(long)]
        only: Option<String>,
        /// Suite folder (default ./evals)
        #[arg(long, default_value = "evals")]
        dir: PathBuf,
    },
    /// Scale bench: N agents at once against a fake model server ($0), ramped until something gives
    Bench {
        /// Concurrency levels to ramp through
        #[arg(long, value_delimiter = ',', default_value = "1,10,50,100,250,500,1000")]
        levels: Vec<usize>,
        /// Tasks each agent runs in a row per level
        #[arg(long, default_value_t = 2)]
        tasks: usize,
        /// Fake model delay before each reply (ms)
        #[arg(long, default_value_t = 100)]
        latency_ms: u64,
        /// Stop before system free memory drops below this (MB)
        #[arg(long, default_value_t = 1024)]
        min_free_mb: u64,
        /// Run tools without the sandbox (to measure what the sandbox costs at scale)
        #[arg(long)]
        no_sandbox: bool,
        /// Output folder (default ./bench/runs/<time>)
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Token proxy for other agents (Claude Code, opencode): savings + cost ledger. `wang proxy stats` for results
    Proxy(wang_proxy::Cli),
    /// Open pages in the sandboxed browser and print what the agent sees (the compact view)
    Browse {
        /// URLs to open, in order
        #[arg(required = true)]
        urls: Vec<String>,
        /// Which part of a long page to print
        #[arg(long, default_value_t = 1)]
        part: usize,
        /// Print a size/time/memory table instead of the pages
        #[arg(long)]
        stats: bool,
    },
    /// Tasks wang does on a schedule: `wang schedule add "every weekday at 8am" summarize hacker news`
    Schedule {
        #[command(subcommand)]
        action: ScheduleAction,
    },
    /// Serve wang's tools to Claude Code / Claude Desktop (MCP over stdio): `claude mcp add wang -- wang mcp`
    Mcp,
    /// Small local model (llama.cpp, in the jail) for history summaries at $0
    Local {
        #[command(subcommand)]
        action: LocalAction,
    },
    /// Firecracker micro-VMs: setup, run a command, try approaches in parallel, bench
    Vm {
        #[command(subcommand)]
        action: VmAction,
    },
    /// Approve a pending privileged action
    Approve { id: String },
    /// Deny a pending privileged action
    Deny { id: String },
    /// Long-term memory: list facts, or add one
    Memory {
        /// Fact to add (omit to list)
        fact: Vec<String>,
    },
    /// Manage the systemd user service
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Subcommand)]
enum ScheduleAction {
    /// Add one: WHEN in plain English ("every day at 8:00", "every 2 hours", "in 20 minutes"), then the task
    Add {
        when: String,
        #[arg(required = true)]
        task: Vec<String>,
    },
    /// Scheduled tasks, next run and last result
    List,
    /// Stop one
    Rm { id: i64 },
}

#[derive(Subcommand)]
enum LocalAction {
    /// Download llama.cpp's server and the model (checksums pinned)
    Setup,
    /// Ask the local model something
    Ask { prompt: Vec<String> },
    /// Start time, speed, memory, and summary quality on a sample conversation
    Bench,
}

#[derive(Subcommand)]
enum VmAction {
    /// Download Firecracker + kernel, build the image from a Docker image, save a base snapshot
    Setup {
        /// Docker image for the VM's filesystem
        #[arg(long, default_value = wang_vm::setup::DEFAULT_IMAGE)]
        image: String,
        /// Rebuild the image and base snapshot
        #[arg(long)]
        rebuild: bool,
    },
    /// Run one command in a fresh VM and throw the VM away
    Run {
        command: Vec<String>,
        /// Cold boot instead of restoring the base snapshot
        #[arg(long)]
        boot: bool,
    },
    /// Fork the VM once per approach, run each, score each, keep the best
    Try {
        /// Run first, once; the approaches start from its result
        #[arg(long)]
        setup: Option<String>,
        /// Run after each approach in its VM; last output line = score (number), else exit 0 = pass
        #[arg(long)]
        score: Option<String>,
        /// The approaches (one shell command each)
        #[arg(required = true)]
        approaches: Vec<String>,
    },
    /// Measure boot, snapshot, restore, fork and memory sharing
    Bench {
        /// VMs to fork at once
        #[arg(long, default_value_t = 8)]
        clones: usize,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Period {
    Today,
    Week,
    Month,
}

#[derive(Clone, Copy, ValueEnum)]
enum StatsPeriod {
    Today,
    Week,
    Month,
    All,
}

#[derive(Subcommand)]
enum NotifyAction {
    /// Send a test notification to the configured ntfy topic
    Test,
}

#[derive(Clone, Copy, ValueEnum)]
enum Verdict {
    Good,
    Bad,
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Write ~/.config/systemd/user/wang.service (doesn't start it)
    Install,
}

const LLAMA_URL: &str = "https://github.com/ggml-org/llama.cpp/releases/download/b12200/llama-b12200-bin-ubuntu-x64.tar.gz";
const LLAMA_SHA256: &str = "38f7018ee6b2f42dd5313fb5fd13a39fdddcd4d6dd0b8333cb4d3c0b85e48644";
const MODEL_URL: &str = "https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct-GGUF/resolve/main/qwen2.5-1.5b-instruct-q4_k_m.gguf";
const MODEL_SHA256: &str = "6a1a2eb6d15622bf3c96857206351ba97e1af16c30d7a74ee38970e434e9407e";

async fn local_cmd(cfg: &Config, action: LocalAction) -> Result<()> {
    use wang_daemon::model::local::{LocalModel, bench_transcript, fact_recall};
    let lcfg = cfg.local.to_cfg()?;
    let log = |m: &str| eprintln!("  {m}");
    if let LocalAction::Setup = action {
        std::fs::create_dir_all(&lcfg.dir)?;
        if lcfg.server().is_none() {
            let tgz = lcfg.dir.join("llama.tgz");
            wang_vm::setup::download(LLAMA_URL, &tgz, LLAMA_SHA256, &log).await?;
            let st = std::process::Command::new("tar").arg("xzf").arg(&tgz).arg("-C").arg(&lcfg.dir).status()?;
            if !st.success() {
                bail!("extracting llama.cpp failed");
            }
            std::fs::remove_file(&tgz).ok();
        }
        wang_vm::setup::download(MODEL_URL, &lcfg.model(), MODEL_SHA256, &log).await?;
        eprintln!("local model ready: {} + Qwen2.5-1.5B-Instruct (Q4_K_M). Try `wang local bench`.", lcfg.server().unwrap().display());
        return Ok(());
    }
    if !lcfg.ready() {
        bail!("local model not set up: run `wang local setup`");
    }
    let jail = std::sync::Arc::new(wang_jail::Jail::new()?);
    let m = LocalModel::new(jail, lcfg.clone());
    match action {
        LocalAction::Setup => unreachable!(),
        LocalAction::Ask { prompt } => {
            let r = m.chat("You are a concise assistant.", &prompt.join(" "), 400).await?;
            println!("{}", r.text);
            eprintln!("[{} → {} tokens · {} ms · {:.0} tok/s]", r.prompt_tokens, r.output_tokens, r.ms, r.gen_tok_s);
        }
        LocalAction::Bench => {
            let transcript = bench_transcript();
            let r = m.chat(wang_daemon::model::local::LOCAL_COMPACT_SYSTEM, &format!("Conversation:\n\n{transcript}"), 500).await?;
            let start_ms = *m.start_ms.lock().unwrap();
            let mem = m.memory_bytes().await.unwrap_or(0);
            let (recall, missing) = fact_recall(&r.text);
            // What the same summary would cost on the main model.
            let usage = wang_tokens::Usage { input_tokens: r.prompt_tokens, output_tokens: r.output_tokens, ..Default::default() };
            let main = cfg.model.model_id();
            let would = wang_tokens::cost_usd(&main, &usage);
            println!("local model bench (Qwen2.5-1.5B-Instruct Q4_K_M, {} threads, CPU)", lcfg.threads);
            println!("  cold start in the jail      {start_ms:.0} ms");
            println!("  summary of a {}-token conversation: {} ms ({:.0} prompt tok/s, {:.0} gen tok/s, {} tokens out)",
                r.prompt_tokens, r.ms, r.prompt_tok_s, r.gen_tok_s, r.output_tokens);
            println!("  memory (cgroup)             {} MiB", mem >> 20);
            println!("  facts kept                  {:.0}%{}", recall * 100.0,
                if missing.is_empty() { String::new() } else { format!(" (missing: {})", missing.join(", ")) });
            println!("  cost                        $0 (on {main}: ${would:.5})");
            let r2 = m.chat(wang_daemon::model::local::LOCAL_COMPACT_SYSTEM, &format!("Conversation:\n\n{transcript}"), 500).await?;
            println!("  warm repeat                 {} ms", r2.ms);
            println!("\n--- summary ---\n{}", r.text);
        }
    }
    m.stop().await;
    Ok(())
}

async fn vm_cmd(cfg: &Config, action: VmAction) -> Result<()> {
    use std::time::Duration;
    let jail = wang_jail::Jail::new()?;
    let vcfg = cfg.vm.to_cfg()?;
    let root = cfg.vm.root();
    match action {
        VmAction::Setup { image, rebuild } => {
            let opts = wang_vm::setup::SetupOpts { root, image, agent: None, cfg: vcfg, rebuild };
            wang_vm::setup::setup(&jail, &opts, &|m| eprintln!("  {m}")).await?;
            eprintln!("VMs ready: try `wang vm run python3 -c 'print(42)'`");
            Ok(())
        }
        VmAction::Run { command, boot } => {
            let assets = wang_vm::Assets::find(Some(&root))?;
            assets.clean_stale();
            let mut vm = if boot { wang_vm::Vm::boot(&jail, &assets, &vcfg).await? } else { wang_vm::start(&jail, &assets, &vcfg).await? };
            let r = vm.exec(&command.join(" "), Duration::from_secs(cfg.vm.timeout_secs)).await;
            let mem = vm.memory();
            vm.stop().await;
            let r = r?;
            print!("{}", r.stdout);
            eprint!("{}", r.stderr);
            eprintln!(
                "[vm ready in {:.0} ms ({}) · command {} ms · exit {} · {} MiB PSS]",
                vm.ready_ms,
                if boot { "cold boot" } else { "from snapshot" },
                r.ms,
                r.exit,
                mem.pss_bytes >> 20
            );
            if r.exit != 0 {
                std::process::exit(r.exit.clamp(1, 255));
            }
            Ok(())
        }
        VmAction::Try { setup, score, approaches } => {
            let assets = wang_vm::Assets::find(Some(&root))?;
            assets.clean_stale();
            let timeout = Duration::from_secs(cfg.vm.timeout_secs);
            let mut vm = wang_vm::start(&jail, &assets, &vcfg).await?;
            if let Some(s) = &setup {
                let r = vm.exec(s, timeout).await?;
                eprintln!("setup: exit {} in {} ms", r.exit, r.ms);
                if !r.ok() {
                    vm.stop().await;
                    bail!("setup failed:\n{}{}", r.stdout, r.stderr);
                }
            }
            let snap = vm.snapshot(&assets, true).await?;
            vm.stop().await;
            let tried = wang_vm::try_parallel(&jail, &assets, &snap, &vcfg, &approaches, score.as_deref(), timeout).await;
            snap.remove();
            let tried = tried?;
            for (i, a) in tried.attempts.iter().enumerate() {
                let mark = if Some(i) == tried.best { "★" } else { " " };
                match &a.run {
                    Ok(r) => println!("{mark} [{i}] score {:>6.2}  exit {:>3}  {:>6} ms  {}", a.score, r.exit, r.ms, a.command),
                    Err(e) => println!("{mark} [{i}] failed: {e}  {}", a.command),
                }
                if let Some(o) = &a.score_output {
                    for l in o.lines().rev().take(2).collect::<Vec<_>>().into_iter().rev() {
                        println!("        {l}");
                    }
                }
            }
            eprintln!("[{} VMs forked in {:.0} ms · all done in {:.0} ms]", approaches.len(), tried.fork_ms, tried.total_ms);
            if let Some(mut kept) = tried.kept {
                kept.stop().await;
            }
            Ok(())
        }
        VmAction::Bench { clones } => wang_vm::bench::run(&jail, &wang_vm::Assets::find(Some(&root))?, &vcfg, clones, &mut std::io::stdout()).await,
    }
}

async fn browse(cfg: &Config, urls: &[String], part: usize, stats: bool) -> Result<()> {
    let bcfg = cfg.browser.to_cfg()?;
    let jail = wang_jail::Jail::new()?;
    let mut b = wang_browser::Browser::launch(&jail, &bcfg).await?;
    let idle_mb = b.memory_bytes().unwrap_or(0) >> 20;
    if stats {
        eprintln!("browser {} · started in {:.0} ms · {} MiB idle · cap {} MiB", b.version, b.launch_ms, idle_mb, bcfg.memory_bytes >> 20);
        println!("{:>7} {:>9} {:>9} {:>8} {:>6} {:>5} {:>6}  url", "load ms", "html B", "text B", "view B", "×html", "refs", "mem MB");
    }
    for url in urls {
        let t = std::time::Instant::now();
        let opened = b.open(url).await;
        let load_ms = t.elapsed().as_millis();
        if let Err(e) = opened {
            if stats {
                println!("{load_ms:>7} {:>9} {:>9} {:>8} {:>6} {:>5} {:>6}  {url}  ({e:#})", "-", "-", "-", "-", "-", "-");
                continue;
            }
            b.close().await;
            bail!("{url}: {e:#}");
        }
        let v = b.view(200_000).await?;
        if stats {
            let ratio = if v.text.is_empty() { 0.0 } else { v.html_bytes as f64 / v.text.len() as f64 };
            println!(
                "{load_ms:>7} {:>9} {:>9} {:>8} {:>6.1} {:>5} {:>6}  {url}",
                v.html_bytes,
                v.text_bytes,
                v.text.len(),
                ratio,
                v.refs,
                b.memory_bytes().unwrap_or(0) >> 20
            );
        } else {
            let (text, parts) = wang_browser::view_part(&v.text, part, bcfg.view_chars);
            println!("{text}");
            eprintln!(
                "\n[part {}/{parts} · view {} chars (≈{} tokens) · page HTML {} bytes · {} elements]",
                part.min(parts),
                text.len(),
                text.len() / 4,
                v.html_bytes,
                v.refs
            );
        }
    }
    let st = b.close().await;
    if stats {
        eprintln!("peak memory {} MiB", st.usage.peak_memory_bytes.unwrap_or(0) >> 20);
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // When wang's binary is mounted into a jail as the helper, it runs that and exits here.
    wang_jail::helper::dispatch();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("WANG_LOG")
                .unwrap_or_else(|_| "wang_daemon=info,wang_cli=info,wang_proxy=info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
    let cli = Cli::parse();
    config::migrate_from_warp();
    let cfg_path = cli.config.clone().unwrap_or_else(config::config_path);

    let Some(cmd) = cli.cmd else {
        return bare(&cfg_path).await;
    };
    match cmd {
        Cmd::Setup => setup::setup(&cfg_path).await,
        Cmd::Doctor => {
            config::load_env();
            let cfg = Config::load(&cfg_path)?;
            println!("wang doctor\n");
            setup::print_doctor(&setup::doctor(&cfg).await);
            Ok(())
        }
        Cmd::Init => {
            init(&cfg_path)?;
            println!("\nNext: `wang setup` (guided), or put OPENROUTER_API_KEY=... in {} and run `wang`.", config::env_path().display());
            Ok(())
        }
        Cmd::Run => {
            config::load_env();
            let cfg = Config::load(&cfg_path)?;
            wang_daemon::daemon::run(&cfg).await
        }
        Cmd::Chat { message, session, new } => chat(message.join(" "), session, new).await,
        Cmd::Cost { period } => {
            let (days, label) = match period {
                Period::Today => (1, "today"),
                Period::Week => (7, "last 7 days"),
                Period::Month => (30, "last 30 days"),
            };
            print!("{}", Ledger::open(&config::db_path())?.summary(days, label)?);
            Ok(())
        }
        Cmd::Stats { period } => {
            let (days, label) = match period {
                StatsPeriod::Today => (Some(1), "today"),
                StatsPeriod::Week => (Some(7), "last 7 days"),
                StatsPeriod::Month => (Some(30), "last 30 days"),
                StatsPeriod::All => (None, "all time"),
            };
            print!("{}", Ledger::open(&config::db_path())?.stats(days, label)?);
            Ok(())
        }
        Cmd::Proxy(cli) => wang_proxy::main(cli).await,
        Cmd::Schedule { action } => {
            let store = wang_daemon::schedule::Store::open(&config::db_path())?;
            match action {
                ScheduleAction::Add { when, task } => {
                    let s = store.add(&when, &task.join(" "), "cli", chrono::Local::now())?;
                    println!(
                        "scheduled #{}: {} · next {}",
                        s.id,
                        s.rule.describe(),
                        s.next_run.map(|t| t.format("%a %b %-d %H:%M").to_string()).unwrap_or_default()
                    );
                    println!("Results go to your alerts (ntfy / Telegram) and `wang schedule list`. The daemon must be running.");
                }
                ScheduleAction::List => print!("{}", wang_daemon::schedule::render(&store.list(None)?)),
                ScheduleAction::Rm { id } => {
                    if !store.remove(id, None)? {
                        bail!("no schedule #{id}");
                    }
                    println!("removed #{id}");
                }
            }
            Ok(())
        }
        Cmd::Mcp => {
            config::load_env();
            let cfg = Config::load(&cfg_path)?;
            wang_daemon::daemon::run_mcp(&cfg).await
        }
        Cmd::Local { action } => {
            let cfg = Config::load(&cfg_path)?;
            local_cmd(&cfg, action).await
        }
        Cmd::Vm { action } => {
            let cfg = Config::load(&cfg_path)?;
            vm_cmd(&cfg, action).await
        }
        Cmd::Browse { urls, part, stats } => {
            let cfg = Config::load(&cfg_path)?;
            browse(&cfg, &urls, part, stats).await
        }
        Cmd::Notify { action: NotifyAction::Test } => {
            use wang_daemon::notify::{Notice, Notify, Ntfy};
            config::load_env();
            let cfg = Config::load(&cfg_path)?;
            let n = Ntfy::from_config(&cfg.ntfy)?
                .context("no ntfy topic configured: set [ntfy] topic in ~/.config/wang/config.toml")?;
            n.send(&Notice {
                title: "wang test".into(),
                message: "If you can read this, wang alerts work.".into(),
                priority: 3,
                tags: vec!["white_check_mark".into()],
                approval_id: None,
            })
            .await?;
            println!("sent a test notification to {}", n.describe());
            Ok(())
        }
        Cmd::Trace { task } => {
            let tracer = Tracer::open(&config::db_path())?;
            let id = match task {
                Some(id) => id,
                None => tracer.latest_task()?.context("no tasks traced yet")?,
            };
            print!("{}", wang_trace::render(&tracer.spans(id)?));
            Ok(())
        }
        Cmd::Rate { verdict, task } => rate(task, matches!(verdict, Verdict::Good)).map(|id| {
            println!("task #{id} rated {}", if matches!(verdict, Verdict::Good) { "👍" } else { "👎" });
        }),
        Cmd::Service { action: ServiceAction::Install } => service_install(),
        Cmd::Eval { live, routing, only, dir } => {
            config::load_env();
            let cfg = Config::load(&cfg_path)?;
            let cheap = routing.then(|| cfg.routing.cheap_model.clone());
            let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
            let out = dir.join("runs").join(format!("{stamp}-{}{}", if live { "live" } else { "replay" }, if routing { "-routed" } else { "" }));
            let opts = wang_evals::RunOpts {
                mode: if live { wang_evals::Mode::Live } else { wang_evals::Mode::Replay },
                suite: dir.join("tasks"),
                cassettes: dir.join("cassettes"),
                out: out.clone(),
                routing: cheap,
                only,
            };
            let report = wang_evals::run(cfg, &opts).await?;
            print!("{report}");
            println!("  saved {}", out.display());
            if let Some(prev) = wang_evals::latest_run(&dir.join("runs"), &out) {
                let c = wang_evals::compare(&wang_evals::Report::load(&prev.join("report.json"))?, &report);
                print!("\n{}", c.text);
                if !c.regressions.is_empty() {
                    std::process::exit(1);
                }
            }
            Ok(())
        }
        Cmd::Bench { levels, tasks, latency_ms, min_free_mb, no_sandbox, out } => {
            let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
            let out = out.unwrap_or_else(|| PathBuf::from("bench/runs").join(format!("{stamp}{}", if no_sandbox { "-nosandbox" } else { "" })));
            let opts = wang_bench::BenchOpts {
                levels,
                tasks_per_agent: tasks,
                latency: std::time::Duration::from_millis(latency_ms),
                min_free_mb,
                sandbox: !no_sandbox,
                out: out.clone(),
                ..Default::default()
            };
            eprintln!("{}", wang_bench::HEADER);
            let report = wang_bench::run(&opts, |l| eprintln!("{l}")).await?;
            println!("\n{report}");
            println!("  saved {}", out.join("report.json").display());
            Ok(())
        }
        Cmd::Approve { id } => send_approval(&id, true).await.map(|_| println!("approved")),
        Cmd::Deny { id } => send_approval(&id, false).await.map(|_| println!("denied")),
        Cmd::Memory { fact } => {
            let m = wang_daemon::memory::Memory::new(config::memory_path());
            if fact.is_empty() {
                let facts = m.facts();
                println!("{} ({} facts)", m.path().display(), facts.len());
                for f in facts {
                    println!("- {f}");
                }
            } else {
                let date = chrono::Local::now().format("%Y-%m-%d").to_string();
                println!("{}", m.add(&fact.join(" "), &date)?);
            }
            Ok(())
        }
    }
}

fn write_new(path: &std::path::Path, contents: &str, mode: u32) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(path, contents)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(true)
}

/// Bare `wang`: set up on first run, then chat with the daemon (started if needed).
async fn bare(cfg_path: &std::path::Path) -> Result<()> {
    config::load_env();
    if !setup::onboarded() {
        if !std::io::stdin().is_terminal() {
            println!("wang isn't set up yet: run `wang setup` (or `wang --help`).");
            return Ok(());
        }
        setup::setup(cfg_path).await?;
        config::load_env();
        println!();
    }
    if !setup::api_key_set() {
        println!("wang's own agent needs an API key (put OPENROUTER_API_KEY=... in {}).", config::env_path().display());
        if setup::claude_has_wang() {
            println!("Or use wang from Claude Code: it's plugged in, just open `claude` and ask it to use wang.");
        } else {
            println!("Or use wang from Claude Code with your Claude login: `wang setup`.");
        }
        return Ok(());
    }
    setup::ensure_daemon().await?;
    chat(String::new(), "cli".into(), false).await
}

fn init(cfg_path: &std::path::Path) -> Result<()> {
    for (path, contents, mode) in [
        (cfg_path.to_path_buf(), config::EXAMPLE_CONFIG, 0o644),
        (config::env_path(), config::EXAMPLE_ENV, 0o600),
    ] {
        if write_new(&path, contents, mode)? {
            println!("wrote {}", path.display());
        } else {
            println!("kept existing {}", path.display());
        }
    }
    Ok(())
}

fn rate(task: Option<i64>, good: bool) -> Result<i64> {
    let db = config::db_path();
    let ledger = Ledger::open(&db)?;
    let id = match task {
        Some(id) => id,
        None => ledger.latest_task()?.context("no tasks yet")?,
    };
    if !ledger.set_feedback(id, good)? {
        bail!("no task #{id}");
    }
    let tracer = Tracer::open(&db)?;
    if let Some(root) = tracer.task_root(id)? {
        tracer.annotate(root, json!({"wang.feedback": if good { "good" } else { "bad" }}))?;
    }
    Ok(id)
}

fn service_install() -> Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let path = std::env::var("PATH").unwrap_or_default();
    let unit = format!(
        "[Unit]
Description=wang agent daemon
After=network-online.target
Wants=network-online.target

[Service]
ExecStart={exe} run
Restart=on-failure
RestartSec=3
Environment=WANG_LOG=info
Environment=\"PATH={path}\"
# The daemon targets <100 MB; this is a hard ceiling, not the goal.
MemoryMax=256M
NoNewPrivileges=yes

[Install]
WantedBy=default.target
",
        exe = exe.display()
    );
    let dir = dirs_config().join("systemd/user");
    std::fs::create_dir_all(&dir)?;
    let unit_path = dir.join("wang.service");
    std::fs::write(&unit_path, unit)?;
    println!("wrote {}\n\nStart it with:\n  systemctl --user daemon-reload\n  systemctl --user enable --now wang\n\nLogs:\n  journalctl --user -u wang -f", unit_path.display());
    Ok(())
}

fn dirs_config() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
}

struct Printer {
    at_line_start: bool,
    color: bool,
}

impl Printer {
    fn dim(&self, s: &str) -> String {
        if self.color { format!("\x1b[2m{s}\x1b[0m") } else { s.to_string() }
    }
    fn text(&mut self, t: &str) {
        print!("{t}");
        let _ = std::io::stdout().flush();
        self.at_line_start = t.ends_with('\n');
    }
    fn newline(&mut self) {
        if !self.at_line_start {
            println!();
            self.at_line_start = true;
        }
    }
    fn side(&mut self, s: &str) {
        self.newline();
        eprintln!("{}", self.dim(s));
    }
}

/// Send one message and stream events until Done/Error. Returns the task id.
type StdinLines = tokio::io::Lines<tokio::io::BufReader<tokio::io::Stdin>>;

/// Answer an approval over a separate connection (the main one is busy streaming).
async fn send_approval(id: &str, allow: bool) -> Result<()> {
    let mut c = Client::connect(&config::socket_path()).await?;
    c.send(&ClientMsg::Approve { id: id.into(), allow }).await?;
    match c.recv().await? {
        ServerMsg::Ok => Ok(()),
        ServerMsg::Error { message } => bail!(message),
        other => bail!("unexpected reply {other:?}"),
    }
}

/// Send one message and stream events until Done/Error. Returns the task id.
/// Approval requests are answered from the terminal (y/N) or from the phone, whichever comes first.
async fn run_message(c: &mut Client, p: &mut Printer, lines: &mut StdinLines, session: &str, text: &str) -> Result<Option<i64>> {
    c.send(&ClientMsg::Message { session: session.into(), text: text.into() }).await?;
    let interactive = std::io::stdin().is_terminal();
    let mut pending: Option<String> = None;
    loop {
        let msg = tokio::select! {
            m = c.recv() => m?,
            line = lines.next_line(), if pending.is_some() && interactive => {
                let id = pending.take().unwrap();
                let allow = matches!(line?.as_deref().map(str::trim), Some("y" | "Y" | "yes"));
                if let Err(e) = send_approval(&id, allow).await {
                    eprintln!("{}", p.dim(&format!("  (couldn't answer: {e:#})")));
                }
                continue;
            }
        };
        match msg {
            ServerMsg::Text { delta } => p.text(&delta),
            ServerMsg::ToolStart { name, detail } => p.side(&format!("  → {name}: {detail}")),
            ServerMsg::ToolEnd { name, ok, ms, trimmed, repeated } => {
                let mut s = format!("  {} {name} {:.2}s", if ok { "✓" } else { "✗" }, ms as f64 / 1e3);
                if trimmed {
                    s += " (output trimmed)";
                }
                if repeated {
                    s += " (repeat)";
                }
                p.side(&s);
            }
            ServerMsg::ApprovalRequest { id, tool, detail } => {
                p.newline();
                eprintln!("  ? wang wants to run a privileged {tool} command:\n      {detail}");
                if interactive {
                    eprint!("    approve? [y/N] (or answer on your phone) ");
                    let _ = std::io::stderr().flush();
                } else {
                    eprintln!("    waiting for approval: phone, or `wang approve {id}` / `wang deny {id}`");
                }
                pending = Some(id);
            }
            ServerMsg::ApprovalResult { decision, .. } => {
                if pending.take().is_some() {
                    eprintln!();
                }
                p.side(&format!("  {decision}"));
            }
            ServerMsg::Done { task_id, footer } => {
                p.newline();
                eprintln!("{}", p.dim(&footer));
                return Ok(Some(task_id));
            }
            ServerMsg::Error { message } => {
                p.newline();
                eprintln!("error: {message}");
                return Ok(None);
            }
            ServerMsg::Ok => {}
        }
    }
}

async fn chat(message: String, session: String, new: bool) -> Result<()> {
    let sock = config::socket_path();
    let mut c = Client::connect(&sock).await.map_err(|_| {
        anyhow::anyhow!(
            "the wang daemon isn't running (no socket at {}).\nStart it with `wang run`, or `systemctl --user start wang` if you installed the service.",
            sock.display()
        )
    })?;
    let mut p = Printer { at_line_start: true, color: std::io::stderr().is_terminal() };
    if new {
        c.send(&ClientMsg::New { session: session.clone() }).await?;
        c.recv().await?;
    }

    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    if !message.trim().is_empty() {
        run_message(&mut c, &mut p, &mut lines, &session, &message).await?;
        return Ok(());
    }

    eprintln!("{}", p.dim("wang chat — /new  /good  /bad  /trace  /cost  /quit · force a workflow: /research /code /quick /general /plan <message>"));
    let mut last_task: Option<i64> = None;
    loop {
        eprint!("> ");
        std::io::stderr().flush()?;
        let Some(line) = lines.next_line().await? else { break };
        let line = line.trim();
        match line {
            "" => continue,
            "/quit" | "/exit" => break,
            "/new" => {
                c.send(&ClientMsg::New { session: session.clone() }).await?;
                c.recv().await?;
                eprintln!("{}", p.dim("(new conversation)"));
            }
            "/good" | "/bad" => match rate(last_task, line == "/good") {
                Ok(id) => eprintln!("{}", p.dim(&format!("task #{id} rated {}", if line == "/good" { "👍" } else { "👎" }))),
                Err(e) => eprintln!("error: {e:#}"),
            },
            "/trace" => match last_task {
                Some(id) => print!("{}", wang_trace::render(&Tracer::open(&config::db_path())?.spans(id)?)),
                None => eprintln!("no task yet"),
            },
            "/cost" => println!("{}", Ledger::open(&config::db_path())?.summary(1, "today")?.short()),
            _ if line.starts_with('/')
                && wang_daemon::workflow::Workflow::from_name(line[1..].split_whitespace().next().unwrap_or("")).is_none() =>
            {
                eprintln!("unknown command {line} (workflows: /research /code /quick /general /plan <message>)")
            }
            _ => {
                if let Some(id) = run_message(&mut c, &mut p, &mut lines, &session, line).await? {
                    last_task = Some(id);
                }
            }
        }
    }
    Ok(())
}

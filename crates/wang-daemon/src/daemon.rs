//! `wang run`: build the agent from config and serve every channel until SIGTERM/SIGINT.

use std::sync::Arc;

use anyhow::{Result, bail};
use wang_tokens::Ledger;
use wang_trace::Tracer;

use crate::agent::{Agent, Limits};
use crate::approvals::Approvals;
use crate::memory::Memory;
use crate::notify::{Fanout, Notify, Ntfy};
use crate::config::{self, Config};
use crate::model::{AnyModel, ModelClient};
use crate::server::{Channel, Hub, UnixSocketChannel};
use crate::telegram::{Bot, Telegram, TelegramChannel};
use crate::tools::{Sandbox, ToolCtx, WebPolicy};
use wang_jail::Jail;

/// Start the jail and prove it works; failures leave sandboxed tools disabled (fail closed).
pub async fn build_sandbox(cfg: &Config) -> Result<Sandbox> {
    if !cfg.sandbox.enabled {
        tracing::warn!("sandbox DISABLED in config: tools run with your full user permissions");
        return Ok(Sandbox::Off);
    }
    let policy = cfg.sandbox.policy()?;
    let jail = match Jail::new() {
        Ok(j) => j,
        Err(e) => {
            tracing::error!("sandbox unavailable: {e:#}. shell and read_file are disabled until this is fixed.");
            return Ok(Sandbox::Unavailable(format!("{e:#}")));
        }
    };
    if let Err(e) = jail.check().await {
        tracing::error!("sandbox check failed: {e:#}. shell and read_file are disabled until this is fixed.");
        return Ok(Sandbox::Unavailable(format!("{e:#}")));
    }
    tracing::info!(
        "sandbox ready: bwrap {} · cgroups {} · read {:?} · write {:?} · network {:?}",
        jail.bwrap.display(),
        jail.cgroups.path.display(),
        cfg.sandbox.read,
        cfg.sandbox.write,
        if cfg.sandbox.domains.is_empty() { vec!["none".to_string()] } else { cfg.sandbox.domains.clone() }
    );
    Ok(Sandbox::Jailed { jail: Arc::new(jail), policy })
}

/// `wang mcp`: serve wang's tools to Claude Code / Claude Desktop over stdio.
pub async fn run_mcp(cfg: &Config) -> Result<()> {
    let sandbox = build_sandbox(cfg).await?;
    let workdir = config::expand_tilde(&cfg.agent.workdir);
    let web = WebPolicy { domains: cfg.web_fetch.domains.clone(), allow_private: cfg.web_fetch.allow_private };
    let memory = cfg.memory.enabled.then(|| Arc::new(Memory::new(config::memory_path())));
    let ctx = tool_ctx(cfg, sandbox, workdir, web, memory)?;
    let ledger = Ledger::open(&config::db_path()).ok().map(Arc::new);
    let server = Arc::new(crate::mcp::McpServer {
        ctx,
        ledger,
        session: format!("mcp-{}", std::process::id()),
        allow_privileged: cfg.mcp.allow_privileged,
        output_cap: cfg.agent.tool_output_cap.max(20_000),
    });
    tracing::info!("wang mcp ready on stdio");
    // The client closes stdin, or sends SIGTERM, when it's done: clean up either way.
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let r = tokio::select! {
        r = server.clone().serve_stdio() => r,
        _ = term.recv() => Ok(()),
        _ = tokio::signal::ctrl_c() => Ok(()),
    };
    server.cleanup().await;
    r
}

/// The tools: sandboxed shell/read_file, web_fetch, memory, and the browser and VMs when available.
/// Shared by the daemon's agent and `wang mcp`.
pub fn tool_ctx(cfg: &Config, sandbox: Sandbox, workdir: std::path::PathBuf, web: WebPolicy, memory: Option<Arc<Memory>>) -> Result<ToolCtx> {
    let browser = browser_pool(cfg, &sandbox)?;
    let vms = vm_sessions(cfg, &sandbox)?;
    Ok(ToolCtx::with(workdir, sandbox, web).with_memory(memory).with_browser(browser).with_vm(vms))
}

/// The browser pool, when `[browser]` is on, the sandbox works and a Chrome is installed.
/// Browsers always run jailed: with the sandbox off or broken, there is no browser tool.
fn browser_pool(cfg: &Config, sandbox: &Sandbox) -> Result<Option<Arc<wang_browser::Pool>>> {
    if !cfg.browser.enabled {
        return Ok(None);
    }
    let Sandbox::Jailed { jail, .. } = sandbox else {
        tracing::warn!("browser off: it only runs inside the sandbox, which is not available");
        return Ok(None);
    };
    let bcfg = cfg.browser.to_cfg()?;
    let bin = match &bcfg.binary {
        Some(b) => b.clone(),
        None => match wang_browser::find_chrome() {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("browser off: {e}");
                return Ok(None);
            }
        },
    };
    tracing::info!(
        "browser ready: {} · up to {} at once · {} MiB each · domains {:?}",
        bin.display(),
        bcfg.max_browsers,
        bcfg.memory_bytes >> 20,
        bcfg.domains
    );
    let pool = wang_browser::Pool::new(jail.clone(), wang_browser::BrowserCfg { binary: Some(bin), ..bcfg });
    pool.spawn_reaper();
    Ok(Some(pool))
}

/// Micro-VMs, when `[vm]` is on, `wang vm setup` has been run and the jail works.
fn vm_sessions(cfg: &Config, sandbox: &Sandbox) -> Result<Option<Arc<wang_vm::Sessions>>> {
    if !cfg.vm.enabled {
        return Ok(None);
    }
    let Sandbox::Jailed { jail, .. } = sandbox else {
        tracing::warn!("vm off: Firecracker runs inside the sandbox, which is not available");
        return Ok(None);
    };
    let assets = match wang_vm::Assets::find(Some(&cfg.vm.root())) {
        Ok(a) => a,
        Err(e) => {
            tracing::info!("vm off: {e}");
            return Ok(None);
        }
    };
    let vcfg = cfg.vm.to_cfg()?;
    tracing::info!(
        "vm ready: {} vCPU · {} MiB · up to {} at once · base snapshot {} · network {:?}",
        vcfg.vcpus,
        vcfg.mem_mib,
        cfg.vm.max_vms,
        if assets.base(&vcfg).is_some() { "yes" } else { "no (cold boots)" },
        vcfg.domains
    );
    let s = wang_vm::Sessions::new(jail.clone(), assets, vcfg, cfg.vm.max_vms);
    s.spawn_reaper();
    Ok(Some(s))
}

/// The local summarizer, when `[local]` is on, `wang local setup` has been run and the jail works.
fn local_model(cfg: &Config, sandbox: &Sandbox) -> Result<Option<Arc<crate::model::local::LocalModel>>> {
    if !cfg.local.enabled {
        return Ok(None);
    }
    let lcfg = cfg.local.to_cfg()?;
    if !lcfg.ready() {
        tracing::info!("local model off (run `wang local setup` to summarize history for $0)");
        return Ok(None);
    }
    let Sandbox::Jailed { jail, .. } = sandbox else {
        tracing::warn!("local model off: it runs inside the sandbox, which is not available");
        return Ok(None);
    };
    tracing::info!("local model ready for history summaries ({} threads, {} token context; starts on first use)", lcfg.threads, lcfg.ctx);
    let m = crate::model::local::LocalModel::new(jail.clone(), lcfg);
    m.spawn_idle_stop();
    Ok(Some(m))
}

/// The Telegram bot, if TELEGRAM_BOT_TOKEN is set.
pub fn telegram_bot(cfg: &Config) -> Result<Option<Arc<Bot>>> {
    match std::env::var("TELEGRAM_BOT_TOKEN").ok().filter(|t| !t.trim().is_empty()) {
        Some(token) => Ok(Some(Arc::new(Bot::new(&cfg.telegram.api_base, &token)?))),
        None => Ok(None),
    }
}

pub fn build_agent(cfg: &Config, sandbox: Sandbox, tg: Option<&Arc<Bot>>) -> Result<Agent<AnyModel>> {
    let client = AnyModel::from_config_routed(&cfg.model, cfg.routing.cheap())?;
    let db = config::db_path();
    let ledger = Arc::new(Ledger::open(&db)?);
    let tracer = Arc::new(Tracer::open(&db)?);
    let workdir = config::expand_tilde(&cfg.agent.workdir);
    if !workdir.is_dir() {
        bail!("agent.workdir {} is not a directory", workdir.display());
    }
    let web = WebPolicy { domains: cfg.web_fetch.domains.clone(), allow_private: cfg.web_fetch.allow_private };
    let a = &cfg.agent;
    let limits = Limits {
        max_steps: a.max_steps,
        tool_output_cap: a.tool_output_cap,
        loop_fail_limit: a.loop_fail_limit.max(1),
        loop_repeat_limit: a.loop_repeat_limit.max(2),
        budget_usd: a.budget_usd.max(0.0),
        compact_at_tokens: a.compact_at_tokens,
        compact_keep_turns: a.compact_keep_turns.max(1),
    };
    let notifier = Ntfy::from_config(&cfg.ntfy)?;
    match &notifier {
        Some(n) => tracing::info!("alerts via ntfy: {}", n.describe()),
        None => tracing::info!("ntfy off (set [ntfy] topic in config to enable)"),
    }
    let ntfy = notifier.map(Arc::new);
    let mut targets: Vec<Arc<dyn Notify>> = vec![];
    if let Some(n) = &ntfy {
        targets.push(n.clone());
    }
    if let (Some(bot), true, false) = (tg, cfg.telegram.alerts, cfg.telegram.allowed_user_ids.is_empty()) {
        tracing::info!("alerts via telegram to {} user(s)", cfg.telegram.allowed_user_ids.len());
        targets.push(Arc::new(Telegram { bot: bot.clone(), users: cfg.telegram.allowed_user_ids.clone() }));
    }
    let notifier: Option<Arc<dyn Notify>> = match targets.len() {
        0 => None,
        1 => targets.pop(),
        _ => Some(Arc::new(Fanout(targets))),
    };
    let memory = cfg.memory.enabled.then(|| Arc::new(Memory::new(config::memory_path())));
    let approvals = if cfg.approvals.enabled {
        let timeout = wang_jail::policy::parse_duration(&cfg.approvals.timeout)?;
        let appr = Arc::new(Approvals::new(notifier.clone(), timeout));
        if let Some(n) = &ntfy {
            let (n, a) = (n.clone(), appr.clone());
            tokio::spawn(async move {
                n.listen(|msg| {
                    if let Some((id, allow)) = a.handle_reply(msg) {
                        tracing::info!("approval {id} {} from phone", if allow { "granted" } else { "denied" });
                    }
                })
                .await
            });
        }
        Some(appr)
    } else {
        None
    };
    let schedules = Arc::new(crate::schedule::Store::open(&db)?);
    let ctx = tool_ctx(cfg, sandbox, workdir, web, memory)?.with_schedules(Some(schedules));
    let summarizer = local_model(cfg, ctx.sandbox.as_ref())?;
    Ok(Agent::new(client, ledger, tracer, ctx, limits).with_notifier(notifier).with_approvals(approvals).with_summarizer(summarizer).with_orchestration(cfg.orchestration.to_cfg()))
}

pub async fn run(cfg: &Config) -> Result<()> {
    let sandbox = build_sandbox(cfg).await?;
    let tg = telegram_bot(cfg)?;
    let agent = build_agent(cfg, sandbox, tg.as_ref())?;
    tracing::info!(
        "wang daemon {} · provider {} · model {} · db {}",
        env!("CARGO_PKG_VERSION"),
        agent.client.provider(),
        agent.client.model(),
        config::db_path().display()
    );
    let hub = Arc::new(Hub::new(agent));
    let path = config::socket_path();
    let socket = UnixSocketChannel::bind(path.clone()).await?;

    // Scheduled tasks: results go to the Telegram chat they were made in, else to the alerts.
    let scheduler = {
        let store = hub.agent.ctx.schedules.clone();
        let delivery = Arc::new(crate::schedule::Delivery { bot: tg.clone(), notifier: hub.agent.notifier.clone() });
        let hub = hub.clone();
        async move {
            match store {
                Some(store) => {
                    let n = store.list(None).map(|l| l.len()).unwrap_or(0);
                    tracing::info!("scheduler on ({n} scheduled task{})", if n == 1 { "" } else { "s" });
                    crate::schedule::run(hub, store, delivery).await
                }
                None => std::future::pending().await,
            }
        }
    };

    let telegram = async {
        match tg {
            Some(bot) => {
                let via_notifier = cfg.telegram.alerts && !cfg.telegram.allowed_user_ids.is_empty();
                TelegramChannel::new(bot, &cfg.telegram, via_notifier).serve(hub.clone()).await
            }
            None => {
                tracing::info!("telegram off (set TELEGRAM_BOT_TOKEN in .env to enable)");
                std::future::pending().await
            }
        }
    };

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let result = tokio::select! {
        r = socket.serve(hub.clone()) => r,
        r = telegram => r,
        r = scheduler => r,
        _ = tokio::signal::ctrl_c() => { tracing::info!("interrupted, shutting down"); Ok(()) }
        _ = term.recv() => { tracing::info!("SIGTERM, shutting down"); Ok(()) }
    };
    let _ = std::fs::remove_file(&path);
    result
}

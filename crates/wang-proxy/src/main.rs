//! `wang-proxy`: the token proxy on its own (same as `wang proxy`).

use clap::Parser;

#[derive(Parser)]
#[command(name = "wang-proxy", version, about = "Local proxy: token savings + cost ledger for Claude Code, opencode and other agents")]
struct Args {
    #[command(flatten)]
    cli: wang_proxy::Cli,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("WANG_LOG").unwrap_or_else(|_| "wang_proxy=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    wang_proxy::main(Args::parse().cli).await
}

//! Operator-started broker. See `.codex/design/nas-lsp-broker.md`.
#[cfg(target_os = "linux")]
use clap::Parser;
#[cfg(target_os = "linux")]
use codex_nas_lsp::broker::Broker;
#[cfg(target_os = "linux")]
use codex_nas_lsp::config::BrokerConfig;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use tokio_util::sync::CancellationToken;

#[cfg(target_os = "linux")]
#[derive(Parser)]
struct Args {
    #[arg(long)]
    config: PathBuf,
}
#[cfg(target_os = "linux")]
#[tokio::main(worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let broker = Broker::new(BrokerConfig::load(&args.config)?)?;
    let shutdown = CancellationToken::new();
    let signal_shutdown = shutdown.clone();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let signals = tokio::spawn(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c()=>{},
            _ = terminate.recv()=>{},
        }
        signal_shutdown.cancel();
    });
    let result = broker.serve(shutdown).await;
    signals.abort();
    result?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("codex-nas-lsp requires Linux cgroup v2");
    std::process::exit(1);
}

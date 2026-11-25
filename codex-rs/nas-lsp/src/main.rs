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
    /// Publish broker metrics to this Prometheus textfile.
    #[arg(long)]
    metrics_file: Option<PathBuf>,
    /// Classify lookups in this registered fixture workspace as synthetic.
    #[arg(long, requires = "metrics_file")]
    synthetic_workspace: Vec<String>,
    /// Print the selected managed project's saved input manifest for the index producer.
    #[arg(long, value_name = "WORKSPACE_ID")]
    manifest: Option<String>,
}
#[cfg(target_os = "linux")]
#[tokio::main(worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let config = BrokerConfig::load(&args.config)?;
    if let Some(id) = args.manifest {
        let workspace = config
            .indexed_workspaces
            .iter()
            .find(|workspace| workspace.id == id)
            .ok_or("unknown indexed workspace")?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        let manifest = workspace.manifest(deadline)?;
        serde_json::to_writer(std::io::stdout().lock(), &manifest)?;
        return Ok(());
    }
    let mut broker = Broker::new(config)?;
    if let Some(path) = args.metrics_file {
        broker = broker.with_metrics(path, args.synthetic_workspace)?;
    }
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

//! Operator-owned broker configuration. See `.codex/design/nas-lsp-broker.md`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct WorkspaceConfig {
    pub id: String,
    pub root: PathBuf,
    pub language: String,
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub source_extensions: Vec<String>,
    pub exclude_dirs: Vec<String>,
    pub readiness: Readiness,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Readiness {
    PyreflyLazyBlocking,
}

#[derive(Clone, Debug)]
pub struct BrokerConfig {
    pub socket_path: PathBuf,
    pub workspaces: Vec<Arc<WorkspaceConfig>>,
    pub limits: PoolLimits,
}

#[derive(Clone, Debug)]
pub struct PoolLimits {
    pub max_workers: usize,
    pub max_queue_per_worker: usize,
    pub max_queries: usize,
    pub max_connections: usize,
    pub idle_ttl: Duration,
    pub memory_max_bytes: u64,
    pub pids_max: u64,
}

impl Default for PoolLimits {
    fn default() -> Self {
        Self {
            max_workers: 2,
            max_queue_per_worker: 4,
            max_queries: 8,
            max_connections: 32,
            idle_ttl: Duration::from_secs(60),
            memory_max_bytes: 1024 * 1024 * 1024,
            pids_max: 64,
        }
    }
}

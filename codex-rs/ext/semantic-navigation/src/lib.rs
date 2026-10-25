//! Native saved-file navigation. See the [design contract](../../../../.codex/design/nas-lsp-broker.md).

#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub use unix::install;
#[cfg(unix)]
pub use unix::install_with_socket;

#[cfg(not(unix))]
use std::path::PathBuf;

#[cfg(not(unix))]
use codex_extension_api::ExtensionRegistryBuilder;

/// No navigation tool is installed on platforms without Unix sockets.
#[cfg(not(unix))]
pub fn install<C: Sync>(_registry: &mut ExtensionRegistryBuilder<C>) {}

/// No navigation tool is installed on platforms without Unix sockets.
#[cfg(not(unix))]
pub fn install_with_socket<C: Sync>(_registry: &mut ExtensionRegistryBuilder<C>, _socket: PathBuf) {
}

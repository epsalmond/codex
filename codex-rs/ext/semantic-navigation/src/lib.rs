//! Native navigation extension. See `.codex/design/nas-lsp-broker.md`.

use codex_extension_api::ExtensionRegistryBuilder;

/// Scaffold only; the native tool lane installs its contributor here.
pub fn install<C>(_registry: &mut ExtensionRegistryBuilder<C>) {}

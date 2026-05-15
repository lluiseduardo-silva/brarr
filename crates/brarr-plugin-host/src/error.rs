//! Plugin-host error type.

use brarr_core::ProviderError;

/// Result alias for fallible plugin operations.
pub type PluginResult<T> = Result<T, PluginError>;

/// Errors raised while loading or invoking a `WASM` plugin.
///
/// Most variants wrap a foreign error. The host translates these to
/// [`ProviderError`] when surfacing them through the
/// [`brarr_core::TrackerProvider`] trait so the orchestrator does not
/// need to know about wasm-specific concepts.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// Wasmtime engine / module compile / runtime error.
    #[error("wasm runtime: {0}")]
    Wasm(String),

    /// A required export was missing from the plugin module.
    #[error("plugin missing required export: {name} ({signature})")]
    MissingExport {
        /// Symbol that was expected.
        name: &'static str,
        /// Human-readable expected signature, for debugging.
        signature: &'static str,
    },

    /// Plugin reported a version that the host does not support.
    #[error("unsupported plugin ABI version: {got} (supported: {supported})")]
    UnsupportedAbi {
        /// What the plugin returned from `plugin_abi_version`.
        got: i32,
        /// The version this host build understands.
        supported: i32,
    },

    /// Plugin returned a non-zero error code from a call.
    #[error("plugin returned error code {0}")]
    PluginCode(i32),

    /// Plugin produced output the host could not decode (bad UTF-8,
    /// malformed JSON, ptr/len out of bounds, etc.).
    #[error("plugin output invalid: {0}")]
    BadOutput(String),

    /// I/O error reading the .wasm file from disk.
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),

    /// Capability the plugin attempted to use was disabled by host config.
    #[error("plugin attempted disabled capability: {0}")]
    CapabilityDenied(&'static str),
}

impl PluginError {
    /// Convert into a [`ProviderError`] with the given plugin name as
    /// the source. Used at the [`brarr_core::TrackerProvider`] trait
    /// boundary.
    #[must_use]
    pub fn into_provider(self, plugin_name: &str) -> ProviderError {
        ProviderError::new(plugin_name, self.to_string())
    }
}

impl From<anyhow::Error> for PluginError {
    fn from(err: anyhow::Error) -> Self {
        Self::Wasm(format!("{err:#}"))
    }
}

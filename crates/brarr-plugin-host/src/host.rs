//! Host-side state held by every plugin instance.
//!
//! The [`HostState`] is carried in `Store<HostState>` by wasmtime. Host
//! functions imported by the plugin (e.g. `host_log`) receive a
//! [`Caller`] from which they unpack the [`HostState`] and the plugin
//! memory.

use tracing::{Level, event};
use wasmtime::{Caller, Extern, Linker};

use crate::PluginError;

/// State threaded through every plugin call.
pub struct HostState {
    /// Display name of the plugin (logged with every host_log call).
    pub plugin_name: String,
    /// Capability flags.
    pub caps: HostCapabilities,
}

/// What the host allows the plugin to do.
#[derive(Debug, Clone, Copy)]
pub struct HostCapabilities {
    /// Whether `host_log` is callable. Default true.
    pub log: bool,
}

impl Default for HostCapabilities {
    fn default() -> Self {
        Self { log: true }
    }
}

/// Install all host-side imports onto `linker` under the `env` module.
///
/// Currently registers:
/// - `env.host_log(level: i32, ptr: i32, len: i32)`
///
/// Capability checks happen *inside* each registered function, so a
/// plugin trying to call a disabled function gets a trap rather than a
/// silent no-op.
///
/// # Errors
///
/// Propagates [`wasmtime::Error`] if `linker.func_wrap` fails (should
/// only happen if you try to register the same name twice).
pub fn install_imports(linker: &mut Linker<HostState>) -> Result<(), PluginError> {
    linker
        .func_wrap(
            "env",
            "host_log",
            |mut caller: Caller<'_, HostState>, level: i32, ptr: i32, len: i32| {
                if !caller.data().caps.log {
                    return Err(wasmtime::Error::msg("host_log: capability disabled"));
                }
                let Some(Extern::Memory(mem)) = caller.get_export("memory") else {
                    return Err(wasmtime::Error::msg("plugin did not export `memory`"));
                };
                let data = mem.data(&caller);
                let msg = read_string(data, ptr, len)?;
                let plugin_name = caller.data().plugin_name.clone();
                emit_log(level, &plugin_name, &msg);
                Ok(())
            },
        )
        .map_err(PluginError::from)?;
    Ok(())
}

fn read_string(data: &[u8], ptr: i32, len: i32) -> Result<String, wasmtime::Error> {
    let ptr_usize =
        usize::try_from(ptr).map_err(|_| wasmtime::Error::msg("negative ptr from plugin"))?;
    let len_usize =
        usize::try_from(len).map_err(|_| wasmtime::Error::msg("negative len from plugin"))?;
    let end = ptr_usize
        .checked_add(len_usize)
        .ok_or_else(|| wasmtime::Error::msg("ptr+len overflows usize"))?;
    let slice = data
        .get(ptr_usize..end)
        .ok_or_else(|| wasmtime::Error::msg("ptr/len out of bounds in plugin memory"))?;
    std::str::from_utf8(slice)
        .map(str::to_owned)
        .map_err(|e| wasmtime::Error::msg(format!("invalid utf-8 from plugin: {e}")))
}

fn emit_log(level: i32, plugin: &str, msg: &str) {
    match level {
        0 => event!(target: "brarr_plugin_host", Level::TRACE, plugin, msg),
        1 => event!(target: "brarr_plugin_host", Level::DEBUG, plugin, msg),
        3 => event!(target: "brarr_plugin_host", Level::WARN, plugin, msg),
        4 => event!(target: "brarr_plugin_host", Level::ERROR, plugin, msg),
        // Treat unknown / 2 (info) and any out-of-range value as INFO.
        _ => event!(target: "brarr_plugin_host", Level::INFO, plugin, msg),
    }
}

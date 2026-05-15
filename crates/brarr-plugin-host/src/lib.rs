//! `brarr-plugin-host` — sandbox `WASM` para scrapers de trackers customizados.
//!
//! Define o contrato de plugin (uma versão `WASM` da trait
//! `TrackerProvider`) e hospeda runtimes (wasmtime/wasmer a decidir)
//! para carregar implementações de terceiros sem recompilar o binário
//! principal. Isolamento de capabilities: plugin só enxerga o que o
//! host expõe explicitamente (fetch `HTTP`, logging, `KV`).
//!
//! Status: stub. Não implementar até a Fase 6+.

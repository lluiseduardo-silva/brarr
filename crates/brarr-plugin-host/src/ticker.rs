//! Background epoch ticker for wasmtime sandbox enforcement.
//!
//! Wasmtime's epoch interruption works by:
//! 1. The `Engine` carries a monotonically increasing **epoch counter**.
//! 2. Each `Store` is given a *deadline* expressed as a future epoch
//!    value. When wasm code crosses a function entry / loop backedge
//!    after that deadline, the host can interrupt it.
//! 3. Something **outside** the wasm execution must advance the engine
//!    counter. That's this module's job.
//!
//! The ticker spawns a tokio task that calls
//! [`wasmtime::Engine::increment_epoch`] every `tick_interval`. It runs
//! until [`WasmEpochTicker`] is dropped (the abort handle drops with
//! the struct, cancelling the task on the next yield).
//!
//! The deadline applied to a particular `Store` is therefore expressed
//! as a *count of ticks*. With a 100 ms tick and a 5 s deadline, the
//! deadline is `5_000 / 100 = 50` ticks. See
//! [`WasmEpochTicker::ticks_for`].

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use wasmtime::Engine;

/// Default cadence of the background epoch increment. Lower numbers
/// give finer deadline granularity at the cost of more wake-ups.
pub const DEFAULT_TICK_INTERVAL: Duration = Duration::from_millis(100);

/// Owns the tokio task that advances `engine.increment_epoch()` on a
/// fixed cadence. Drop the value to stop the ticker.
pub struct WasmEpochTicker {
    handle: Option<JoinHandle<()>>,
    tick_interval: Duration,
}

impl WasmEpochTicker {
    /// Spawn the ticker against `engine`.
    #[must_use]
    pub fn spawn(engine: &Arc<Engine>, tick_interval: Duration) -> Self {
        let engine_for_task = Arc::clone(engine);
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(tick_interval);
            // `interval` fires once immediately; skip that tick so the
            // first real increment happens after `tick_interval`.
            interval.tick().await;
            loop {
                interval.tick().await;
                engine_for_task.increment_epoch();
            }
        });
        Self {
            handle: Some(handle),
            tick_interval,
        }
    }

    /// Number of epoch ticks that cover `deadline` of wall-clock time.
    /// Minimum 1 so any positive deadline still allows at least one
    /// trap check.
    #[must_use]
    pub fn ticks_for(&self, deadline: Duration) -> u64 {
        let ms = deadline.as_millis().max(1);
        let tick_ms = self.tick_interval.as_millis().max(1);
        let ratio = ms.div_ceil(tick_ms);
        u64::try_from(ratio).unwrap_or(u64::MAX).max(1)
    }
}

impl Drop for WasmEpochTicker {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

impl std::fmt::Debug for WasmEpochTicker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmEpochTicker")
            .field("tick_interval", &self.tick_interval)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ticks_for_rounds_up() {
        // 100ms ticker + 250ms deadline → 3 ticks (250/100 ceil).
        let engine = Arc::new(Engine::default());
        let ticker = WasmEpochTicker::spawn(&engine, Duration::from_millis(100));
        assert_eq!(ticker.ticks_for(Duration::from_millis(250)), 3);
        assert_eq!(ticker.ticks_for(Duration::from_millis(100)), 1);
        // Zero deadline still produces at least one tick so a
        // misconfigured `Duration::ZERO` doesn't disable enforcement.
        assert_eq!(ticker.ticks_for(Duration::ZERO), 1);
    }
}

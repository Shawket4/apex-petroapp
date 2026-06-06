use std::sync::Arc;

use tokio::time::{interval, MissedTickBehavior};

use crate::AppState;

/// Spawn the background sync loop. A failing cycle is recorded and alerted inside
/// `run_sync`; the loop itself simply continues to the next tick.
pub fn spawn(state: Arc<AppState>) {
    let period = state.cfg.sync_interval;
    tokio::spawn(async move {
        let mut tick = interval(period);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        tracing::info!(?period, "sync scheduler started");
        loop {
            tick.tick().await;
            let outcome = crate::sync::run_sync(&state, "schedule").await;
            tracing::info!(
                run_id = outcome.run_id,
                status = %outcome.status,
                inserted = outcome.events_inserted,
                unmapped = outcome.unmapped_count,
                "scheduled sync complete"
            );
        }
    });
}

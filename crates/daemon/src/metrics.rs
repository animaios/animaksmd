//! Metrics collection and reporting for the daemon.
//!
//! Periodically logs structured metrics via tracing that get routed
//! to systemd journal or JSON log files.

use std::time::Duration;

use tokio::sync::watch;
use tokio::time;
use tracing::info;
use zramdedup_common::ksm::KsmController;
use zramdedup_common::SharedGovernorState;

/// Run the metrics reporting loop.
pub async fn run_metrics_loop(
    ksm: &KsmController,
    state: SharedGovernorState,
    interval_secs: u64,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut interval = time::interval(Duration::from_secs(interval_secs));

    loop {
        tokio::select! {
            _ = interval.tick() => {
                report_metrics(ksm, &state).await;
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
        }
    }
}

async fn report_metrics(ksm: &KsmController, state: &SharedGovernorState) {
    let ksm_stats = match ksm.read_stats() {
        Ok(s) => s,
        Err(_) => return,
    };

    let s = state.read().await;

    // Read our own RSS
    let daemon_rss_kb = read_self_rss_kb();

    info!(
        target = "zramdedup::metrics",
        ksm_pages_shared = ksm_stats.pages_shared,
        ksm_pages_sharing = ksm_stats.pages_sharing,
        ksm_general_profit = ksm_stats.general_profit,
        ksm_full_scans = ksm_stats.full_scans,
        governor_level = s.ksm_level,
        pressure = %s.current_pressure,
        daemon_rss_kb,
        "Metrics report"
    );
}

/// Read our own RSS from /proc/self/status.
fn read_self_rss_kb() -> u64 {
    if let Ok(content) = std::fs::read_to_string("/proc/self/status") {
        for line in content.lines() {
            if let Some(value) = line.strip_prefix("VmRSS:") {
                let s = value.trim().trim_end_matches(" kB").trim();
                return s.parse().unwrap_or(0);
            }
        }
    }
    0
}

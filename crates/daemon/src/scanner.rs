//! Tier 2: Process-aware Memory Scanner
//!
//! Identifies processes with high duplicate-page potential and applies
//! MADV_MERGEABLE via process_madvise(2). Acts as a **KSM hint generator**,
//! not a forced deduplicator — it seeds KSM eligibility for stable,
//! cold anonymous regions.
//!
//! ## Multi-Level Filtering (avoids O(n×m) cost)
//!
//! 1. **Level 1 (cheap):** `/proc/PID/status` RSS threshold → filter to
//!    large processes only.
//! 2. **Level 2 (medium):** `/proc/PID/ksm_stat` → skip already-merged.
//! 3. **Level 3 (expensive):** `/proc/PID/maps` → only for top K candidates
//!    by RSS, then apply madvise.
//!
//! ## Stabilization
//!
//! Respects the global stabilization window to prevent feedback oscillation
//! with the governor tier.

use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio::time;
use tracing::{debug, info, warn};
use zramdedup_common::config::ScannerConfig;
use zramdedup_common::procfs;
use zramdedup_common::SharedGovernorState;

use crate::madvise;

/// Scanner statistics.
#[derive(Debug, Clone, Default)]
pub struct ScannerStats {
    pub processes_filtered_l1: u64,
    pub processes_filtered_l2: u64,
    pub processes_filtered_l25_profit: u64,
    pub processes_targeted: u64,
    pub madvise_calls: u64,
    pub total_bytes_marked: u64,
    pub scan_cycles: u64,
    pub skipped_stabilization: u64,
    pub thp_collapsed: u64,
}

/// A scored candidate process.
struct Candidate {
    pid: u32,
    name: String,
    anon_rss_mb: u64,
}

/// The process scanner.
pub struct Scanner {
    config: ScannerConfig,
    dry_run: bool,
    stats: ScannerStats,
    stabilization_secs: u64,
}

impl Scanner {
    pub fn new(config: ScannerConfig, dry_run: bool, stabilization_secs: u64) -> Self {
        Self {
            config,
            dry_run,
            stats: ScannerStats::default(),
            stabilization_secs,
        }
    }

    /// Run the scanner loop until shutdown is signaled.
    pub async fn run(
        mut self,
        state: SharedGovernorState,
        mut shutdown: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        let base_interval = Duration::from_secs(self.config.interval_secs);
        let mut interval = time::interval(base_interval);

        info!(
            interval_secs = self.config.interval_secs,
            min_anon_rss_mb = self.config.min_anon_rss_mb,
            max_candidates = self.config.max_candidates_per_cycle,
            stabilization_secs = self.stabilization_secs,
            "Process scanner started (KSM hint generator mode)"
        );

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    self.scan_cycle(&state).await;
                    self.stats.scan_cycles += 1;
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("Scanner received shutdown signal");
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    /// Execute one scan cycle with multi-level filtering.
    async fn scan_cycle(&mut self, state: &SharedGovernorState) {
        // Check global stabilization window
        let stabilization = Duration::from_secs(self.stabilization_secs);
        {
            let s = state.read().await;
            if s.last_global_action.elapsed() < stabilization {
                debug!("Scanner deferred (stabilization window active)");
                self.stats.skipped_stabilization += 1;
                return;
            }
        }

        // === Level 1: Cheap /proc/PID/status filter ===
        // Only read VmAnon — no maps parsing, no pagemap.
        let pids = match procfs::list_pids() {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "Failed to list PIDs");
                return;
            }
        };

        let mut l1_candidates: Vec<Candidate> = Vec::new();

        for pid in pids {
            if pid <= 2 {
                continue;
            }

            let status = match procfs::read_process_status(pid) {
                Ok(s) => s,
                Err(_) => continue,
            };

            if procfs::is_blocklisted(&status.name, &self.config.blocklist) {
                continue;
            }

            let anon_mb = status.vm_anon_kb / 1024;
            if anon_mb < self.config.min_anon_rss_mb {
                continue;
            }

            l1_candidates.push(Candidate {
                pid,
                name: status.name,
                anon_rss_mb: anon_mb,
            });
        }

        self.stats.processes_filtered_l1 += l1_candidates.len() as u64;

        if l1_candidates.is_empty() {
            debug!("No processes passed Level 1 RSS filter");
            return;
        }

        // === Level 2: Skip already-merged processes ===
        let mut l2_candidates: Vec<Candidate> = Vec::new();

        for candidate in l1_candidates {
            if let Ok(ksm_stat) = procfs::read_ksm_stat(candidate.pid) {
                if ksm_stat.merge_any {
                    continue; // Already KSM-enabled
                }
            }
            l2_candidates.push(candidate);
        }

        self.stats.processes_filtered_l2 += l2_candidates.len() as u64;

        if l2_candidates.is_empty() {
            debug!("All Level 1 candidates already KSM-enabled");
            return;
        }

        // === Level 2.5: Skip processes where KSM reports negative profit ===
        // If the kernel already determined that scanning this process wastes
        // more CPU than it saves in merged pages, don't add more madvise hints.
        let mut l25_candidates: Vec<Candidate> = Vec::new();

        for candidate in l2_candidates {
            if let Ok(ksm_stat) = procfs::read_ksm_stat(candidate.pid) {
                if ksm_stat.process_profit < 0 {
                    self.stats.processes_filtered_l25_profit += 1;
                    debug!(
                        pid = candidate.pid,
                        profit = ksm_stat.process_profit,
                        "Skipped: KSM reports negative process profit"
                    );
                    continue;
                }
            }
            l25_candidates.push(candidate);
        }

        if l25_candidates.is_empty() {
            debug!("All candidates filtered by negative KSM profit");
            return;
        }

        // Sort by RSS (largest first) and take top K
        l25_candidates.sort_by(|a, b| b.anon_rss_mb.cmp(&a.anon_rss_mb));
        l25_candidates.truncate(self.config.max_candidates_per_cycle);

        // === Level 3: Maps parsing + madvise for top K only ===
        let mut targets: Vec<(u32, Vec<procfs::MapsEntry>)> = Vec::new();

        for candidate in &l25_candidates {
            let maps = match procfs::read_process_maps(candidate.pid) {
                Ok(m) => m,
                Err(_) => continue,
            };

            let anon_rw_count = maps
                .iter()
                .filter(|m| m.is_anon_rw() && !m.has_exec() && !m.is_special())
                .count();

            if anon_rw_count == 0 {
                continue;
            }

            let total_anon_rw_mb: u64 = maps
                .iter()
                .filter(|m| m.is_anon_rw() && !m.has_exec() && !m.is_special())
                .map(|m| m.size())
                .sum::<u64>()
                / 1024
                / 1024;

            debug!(
                pid = candidate.pid,
                name = %candidate.name,
                anon_rss_mb = candidate.anon_rss_mb,
                anon_rw_regions = anon_rw_count,
                eligible_mb = total_anon_rw_mb,
                "Level 3 candidate — seeding KSM eligibility"
            );

            targets.push((candidate.pid, maps));
        }

        if targets.is_empty() {
            debug!("No Level 3 targets after maps filtering");
            return;
        }

        // Apply MADV_MERGEABLE to targets
        let max_bytes = self.config.max_mergeable_per_process_mb * 1024 * 1024;
        let results = madvise::batch_apply_mergeable(&targets, max_bytes, self.dry_run);

        // Aggregate statistics
        for (_pid, result) in &results {
            self.stats.processes_targeted += 1;
            self.stats.madvise_calls += result.regions_merged as u64;
            self.stats.total_bytes_marked += result.total_bytes_marked;

            if !result.errors.is_empty() {
                for (addr, err) in &result.errors {
                    debug!(
                        pid = result.pid,
                        addr = format!("0x{addr:x}"),
                        error = %err,
                        "madvise error for region"
                    );
                }
            }
        }

        // Opportunistic MADV_COLLAPSE: if KSM has been unmerging pages
        // (high pages_volatile), try to re-promote large regions to THPs
        // for TLB efficiency. Only on processes we already touched.
        if let Ok(ksm_stats) = zramdedup_common::ksm::KsmController::new(
            "/sys/kernel/mm/ksm",
        ) {
            if let Ok(stats) = ksm_stats.read_stats() {
                if stats.pages_volatile > 1000 {
                    for (pid, maps) in &targets {
                        if !self.dry_run {
                            let collapsed = madvise::collapse_regions(*pid, maps);
                            self.stats.thp_collapsed += collapsed as u64;
                        }
                    }
                }
            }
        }

        // Update global action timestamp if we actually did something
        if self.stats.madvise_calls > 0 {
            let mut s = state.write().await;
            s.last_global_action = Instant::now();
        }

        info!(
            scan_cycle = self.stats.scan_cycles,
            l1_passed = self.stats.processes_filtered_l1,
            l2_passed = self.stats.processes_filtered_l2,
            l25_profit_filtered = self.stats.processes_filtered_l25_profit,
            targeted = targets.len(),
            madvise_calls = self.stats.madvise_calls,
            marked_mb = self.stats.total_bytes_marked / 1024 / 1024,
            thp_collapsed = self.stats.thp_collapsed,
            skipped_stab = self.stats.skipped_stabilization,
            "Scanner cycle complete"
        );
    }

    /// Get current scanner statistics.
    #[allow(dead_code)]
    pub fn stats(&self) -> &ScannerStats {
        &self.stats
    }
}

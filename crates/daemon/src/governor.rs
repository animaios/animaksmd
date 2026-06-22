//! Tier 1: PSI-aware KSM Governor
//!
//! Monitors memory pressure and dynamically adjusts KSM scanning
//! aggressiveness to trade CPU cycles for effective RAM.
//!
//! ## Control Model (Option B — Advisor Primary)
//!
//! When `use_advisor = true`, the kernel advisor is the primary controller
//! for `pages_to_scan`. The governor acts as a **bias layer only**, adjusting:
//! - `max_page_sharing` (how many processes can share a page)
//! - `sleep_millisecs` bounds (pace of scanning)
//!
//! This avoids two controllers fighting the same parameters.
//!
//! ## Global Damping
//!
//! A stabilization window prevents coupled feedback oscillation between
//! the governor, scanner, and swap proxy tiers. No system-wide actuation
//! changes are made within this window.

use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio::time;
use tracing::{debug, info, warn};
use zramdedup_common::config::GovernorConfig;
use zramdedup_common::ksm::KsmController;
use zramdedup_common::psi::{PressureLevel, PsiStats};
use zramdedup_common::SharedGovernorState;

/// KSM profile for each aggressiveness level.
///
/// In advisor mode (Option B), only `max_page_sharing` and `sleep_millisecs`
/// are written. The advisor handles `pages_to_scan` internally.
#[derive(Debug, Clone)]
pub struct KsmProfile {
    #[allow(dead_code)]
    pub level: u8,
    pub run: u64,
    pub max_page_sharing: u32,
    pub sleep_millisecs: u64,
    pub pages_to_scan: u64, // only used when advisor is OFF
}

/// The governor profiles table.
const PROFILES: &[KsmProfile] = &[
    // Level 0: Idle — KSM stopped, no scanning
    KsmProfile {
        level: 0,
        run: 0,
        max_page_sharing: 256,
        sleep_millisecs: 500,
        pages_to_scan: 100,
    },
    // Level 1: Light — minimal scanning
    KsmProfile {
        level: 1,
        run: 1,
        max_page_sharing: 256,
        sleep_millisecs: 100,
        pages_to_scan: 500,
    },
    // Level 2: Moderate
    KsmProfile {
        level: 2,
        run: 1,
        max_page_sharing: 384,
        sleep_millisecs: 50,
        pages_to_scan: 2000,
    },
    // Level 3: Active
    KsmProfile {
        level: 3,
        run: 1,
        max_page_sharing: 512,
        sleep_millisecs: 20,
        pages_to_scan: 5000,
    },
    // Level 4: Aggressive
    KsmProfile {
        level: 4,
        run: 1,
        max_page_sharing: 768,
        sleep_millisecs: 10,
        pages_to_scan: 15000,
    },
    // Level 5: Emergency — maximum scanning
    KsmProfile {
        level: 5,
        run: 1,
        max_page_sharing: 1024,
        sleep_millisecs: 5,
        pages_to_scan: 30000,
    },
];

/// The governor state machine.
pub struct Governor {
    config: GovernorConfig,
    ksm: KsmController,
    current_level: u8,
    hysteresis_count: u32,
    level_entered_at: Instant,
    full_scans_at_level: u64,
    /// Last PSI reading (for fallback stability check).
    last_psi: Option<PsiStats>,
    /// Consecutive ticks with stable PSI (for drift detection).
    stable_psi_ticks: u32,
}

impl Governor {
    pub fn new(config: GovernorConfig, ksm: KsmController) -> Self {
        Self {
            config,
            ksm,
            current_level: 0,
            hysteresis_count: 0,
            level_entered_at: Instant::now(),
            full_scans_at_level: 0,
            last_psi: None,
            stable_psi_ticks: 0,
        }
    }

    /// Run the governor loop until shutdown is signaled.
    pub async fn run(
        mut self,
        state: SharedGovernorState,
        mut shutdown: watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        let poll_interval = Duration::from_millis(2000);
        let mut interval = time::interval(poll_interval);

        info!(
            use_advisor = self.config.use_advisor,
            stabilization_secs = self.config.stabilization_secs,
            "KSM governor started (Option B: bias layer over kernel advisor)"
        );

        // Enable KSM and advisor mode on startup
        if self.config.use_advisor {
            let _ = self.ksm.set_advisor_mode("scan-time");
            info!("KSM advisor mode set to scan-time (governor biases bounds only)");
        }
        let _ = self.ksm.set_run(1);

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    self.tick(&state).await;
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("Governor received shutdown signal");
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    /// Single tick of the governor loop.
    async fn tick(&mut self, state: &SharedGovernorState) {
        // Read current PSI (this IS the periodic fallback poll — always runs)
        let psi = match PsiStats::read_memory() {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "Failed to read PSI stats");
                return;
            }
        };

        // PSI stability tracking (for drift detection)
        if let Some(ref prev) = self.last_psi {
            let some_diff = (psi.some.avg10 - prev.some.avg10).abs();
            let full_diff = (psi.full.avg10 - prev.full.avg10).abs();
            if some_diff < 0.5 && full_diff < 0.5 {
                self.stable_psi_ticks += 1;
            } else {
                self.stable_psi_ticks = 0;
            }
        }
        self.last_psi = Some(psi.clone());

        // Read current KSM stats
        let ksm_stats = match self.ksm.read_stats() {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "Failed to read KSM stats");
                return;
            }
        };

        // Classify pressure
        let pressure =
            psi.classify(self.config.psi_some_threshold, self.config.psi_full_threshold);

        // Determine target level
        let target_level = self.pressure_to_level(pressure);

        // Apply transition logic with hysteresis
        let new_level = self.compute_transition(target_level);

        // Apply profile if level changed (with global damping check)
        if new_level != self.current_level {
            // Check global stabilization window
            let stabilization = Duration::from_secs(self.config.stabilization_secs);
            let time_since_action = {
                let s = state.read().await;
                s.last_global_action.elapsed()
            };

            if time_since_action >= stabilization {
                self.apply_profile(new_level, state).await;
            } else {
                debug!(
                    remaining_secs = (stabilization - time_since_action).as_secs(),
                    "Level change deferred (stabilization window)"
                );
            }
        }

        // Profit monitoring: if KSM is running but profit is negative after many scans
        if self.current_level > 0
            && ksm_stats.general_profit < 0
            && ksm_stats.full_scans > self.full_scans_at_level + 10
        {
            warn!(
                level = self.current_level,
                profit = ksm_stats.general_profit,
                "KSM general_profit is negative after sustained scanning, reducing aggressiveness"
            );
            if self.current_level > 1 {
                self.apply_profile(self.current_level - 1, state).await;
            }
        }

        // Update shared state
        {
            let mut s = state.write().await;
            s.current_pressure = pressure;
            s.ksm_level = self.current_level;
            s.pages_sharing = ksm_stats.pages_sharing;
            s.general_profit = ksm_stats.general_profit;
            s.last_adjustment = Instant::now();
        }

        debug!(
            psi_some_avg10 = psi.some.avg10,
            psi_full_avg10 = psi.full.avg10,
            pressure = %pressure,
            ksm_level = self.current_level,
            pages_shared = ksm_stats.pages_shared,
            pages_sharing = ksm_stats.pages_sharing,
            general_profit = ksm_stats.general_profit,
            stable_psi_ticks = self.stable_psi_ticks,
            "Governor tick"
        );
    }

    /// Map pressure level to KSM aggressiveness level (0-5).
    fn pressure_to_level(&self, pressure: PressureLevel) -> u8 {
        match pressure {
            PressureLevel::Idle => 0,
            PressureLevel::Low => 1,
            PressureLevel::Medium => 3,
            PressureLevel::High => 4,
            PressureLevel::Critical => 5,
        }
    }

    /// Compute the actual transition with hysteresis and cooldown.
    fn compute_transition(&mut self, target_level: u8) -> u8 {
        if target_level > self.current_level {
            // Ramp up immediately
            self.hysteresis_count = 0;
            self.level_entered_at = Instant::now();
            self.full_scans_at_level = 0;
            target_level
        } else if target_level < self.current_level {
            // Ramp down with hysteresis and cooldown
            let cooldown = Duration::from_secs(self.config.min_level_duration_secs);
            if self.level_entered_at.elapsed() < cooldown {
                return self.current_level; // Still in cooldown
            }

            self.hysteresis_count += 1;
            if self.hysteresis_count >= self.config.hysteresis_readings {
                self.hysteresis_count = 0;
                self.level_entered_at = Instant::now();
                self.full_scans_at_level = 0;
                // Step down by one level at a time
                self.current_level - 1
            } else {
                self.current_level
            }
        } else {
            // Same level, reset hysteresis
            self.hysteresis_count = 0;
            self.current_level
        }
    }

    /// Apply a KSM profile by writing sysfs parameters.
    ///
    /// In advisor mode (Option B), only writes `max_page_sharing` and
    /// `sleep_millisecs` — the kernel advisor controls `pages_to_scan`.
    /// In manual mode, writes all parameters directly.
    async fn apply_profile(&mut self, level: u8, state: &SharedGovernorState) {
        let profile = &PROFILES[level as usize];
        let prev_level = self.current_level;

        info!(
            prev_level,
            new_level = level,
            advisor_mode = self.config.use_advisor,
            max_page_sharing = profile.max_page_sharing,
            sleep_millisecs = profile.sleep_millisecs,
            "Applying KSM profile"
        );

        // Write order matters: set run=0 first if going to idle, then params, then run=1
        if profile.run == 0 {
            let _ = self.ksm.set_run(0);
        }

        // Always write these — they are the governor's "bias knobs"
        let _ = self.ksm.set_sleep_millisecs(profile.sleep_millisecs);
        let _ = self.ksm.set_max_page_sharing(profile.max_page_sharing as u64);

        if self.config.use_advisor {
            // Option B: DON'T write pages_to_scan or advisor_target_scan_time.
            // Let the kernel advisor decide scan rate. We only bias the bounds.
            debug!("Advisor mode: skipping pages_to_scan (kernel controls scan rate)");
        } else {
            // Full manual control
            let _ = self.ksm.set_pages_to_scan(profile.pages_to_scan);
        }

        if profile.run != 0 {
            let _ = self.ksm.set_run(1);
        }

        self.current_level = level;

        // Update global action timestamp for stabilization damping
        {
            let mut s = state.write().await;
            s.last_global_action = Instant::now();
        }
    }
}

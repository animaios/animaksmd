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
//! ## Stabilization
//!
//! A stabilization window damps repeated governor profile changes. Other
//! tiers maintain their own stabilization timestamps so a KSM profile write
//! cannot starve the process scanner.

use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio::time;
use tracing::{debug, info, warn};
use zramdedup_common::config::GovernorConfig;
use zramdedup_common::ksm::{KsmController, KsmStats};
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

        // Read current KSM stats
        let ksm_stats = match self.ksm.read_stats() {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "Failed to read KSM stats");
                return;
            }
        };

        self.tick_with_readings(state, psi, ksm_stats).await;
    }

    async fn tick_with_readings(
        &mut self,
        state: &SharedGovernorState,
        psi: PsiStats,
        ksm_stats: KsmStats,
    ) {
        self.observe_psi_stability(&psi);

        // Classify pressure
        let pressure = psi.classify(
            self.config.psi_some_threshold,
            self.config.psi_full_threshold,
        );

        // Determine target level
        let target_level = self.pressure_to_level(pressure);

        // Apply transition logic with hysteresis
        let new_level = self.compute_transition(target_level);

        // Apply profile if level changed (with per-governor damping check)
        if new_level != self.current_level {
            // Check governor stabilization window
            let stabilization = Duration::from_secs(self.config.stabilization_secs);
            let time_since_action = {
                let s = state.read().await;
                s.last_governor_action.elapsed()
            };

            if time_since_action >= stabilization {
                self.apply_profile(new_level, state).await;
            } else {
                debug!(
                    remaining_secs = (stabilization - time_since_action).as_secs(),
                    "Level change deferred (governor stabilization window)"
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

    fn observe_psi_stability(&mut self, psi: &PsiStats) {
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
        let _ = self
            .ksm
            .set_max_page_sharing(profile.max_page_sharing as u64);

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

        // Update governor stabilization timestamp. Keep the global timestamp
        // as observability for "anything acted" without using it as a gate.
        {
            let mut s = state.write().await;
            s.last_governor_action = Instant::now();
            s.last_global_action = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zramdedup_common::new_shared_state;

    /// Create a Governor with default config and a dry-run KSM controller.
    fn make_gov() -> Governor {
        let mut ksm = KsmController::new("/sys/kernel/mm/ksm").unwrap();
        ksm.set_dry_run(true);
        Governor::new(GovernorConfig::default(), ksm)
    }

    /// Create a Governor with a modified config.
    fn make_gov_with(f: impl FnOnce(&mut GovernorConfig)) -> Governor {
        let mut config = GovernorConfig::default();
        f(&mut config);
        let mut ksm = KsmController::new("/sys/kernel/mm/ksm").unwrap();
        ksm.set_dry_run(true);
        Governor::new(config, ksm)
    }

    fn make_temp_gov_with(f: impl FnOnce(&mut GovernorConfig)) -> Governor {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.keep();
        let mut config = GovernorConfig::default();
        config.ksm_path = path.to_string_lossy().to_string();
        f(&mut config);
        let mut ksm = KsmController::new(&config.ksm_path).unwrap();
        ksm.set_dry_run(true);
        Governor::new(config, ksm)
    }

    fn psi(some_avg10: f32, full_avg10: f32) -> PsiStats {
        PsiStats {
            some: zramdedup_common::psi::PsiLine {
                avg10: some_avg10,
                ..Default::default()
            },
            full: zramdedup_common::psi::PsiLine {
                avg10: full_avg10,
                ..Default::default()
            },
        }
    }

    fn ksm_stats(pages_sharing: u64, general_profit: i64, full_scans: u64) -> KsmStats {
        KsmStats {
            pages_shared: pages_sharing / 2,
            pages_sharing,
            general_profit,
            full_scans,
            ..Default::default()
        }
    }

    // ── Initial state ────────────────────────────────────────────────

    #[test]
    fn test_new_initial_state() {
        let gov = make_gov();
        assert_eq!(gov.current_level, 0, "should start at Idle level");
        assert_eq!(gov.hysteresis_count, 0);
        assert_eq!(gov.stable_psi_ticks, 0);
        assert!(gov.last_psi.is_none());
    }

    // ── PROFILES table integrity ─────────────────────────────────────

    #[test]
    fn test_profiles_len() {
        assert_eq!(PROFILES.len(), 6, "must have exactly 6 profiles (0..=5)");
    }

    #[test]
    fn test_profiles_sequential() {
        for (i, p) in PROFILES.iter().enumerate() {
            assert_eq!(p.level as usize, i, "PROFILES[{i}] has mismatched level");
        }
    }

    #[test]
    fn test_profiles_idle_stops_ksm() {
        assert_eq!(PROFILES[0].run, 0, "Idle profile must set run=0");
        for i in 1..=5 {
            assert!(PROFILES[i].run > 0, "Profile {i} should have run>0");
        }
    }

    #[test]
    fn test_profiles_monotonic() {
        // Higher levels = strictly more aggressive or equal
        for i in 1..=5 {
            let prev = &PROFILES[i - 1];
            let cur = &PROFILES[i];
            assert!(
                cur.max_page_sharing >= prev.max_page_sharing,
                "max_page_sharing regressed at level {i}: {} < {}",
                cur.max_page_sharing,
                prev.max_page_sharing
            );
            assert!(
                cur.sleep_millisecs <= prev.sleep_millisecs,
                "sleep_millisecs increased at level {i}: {} > {}",
                cur.sleep_millisecs,
                prev.sleep_millisecs
            );
            assert!(
                cur.pages_to_scan >= prev.pages_to_scan,
                "pages_to_scan regressed at level {i}: {} < {}",
                cur.pages_to_scan,
                prev.pages_to_scan
            );
        }
    }

    // ── pressure_to_level ────────────────────────────────────────────

    #[test]
    fn test_pressure_to_level_idle() {
        let gov = make_gov();
        assert_eq!(gov.pressure_to_level(PressureLevel::Idle), 0);
    }

    #[test]
    fn test_pressure_to_level_low() {
        let gov = make_gov();
        assert_eq!(gov.pressure_to_level(PressureLevel::Low), 1);
    }

    #[test]
    fn test_pressure_to_level_medium_skips_2() {
        let gov = make_gov();
        // Medium maps to 3 (jumps over profile level 2, which is only
        // reached during ramp-down from Active/Higher)
        assert_eq!(gov.pressure_to_level(PressureLevel::Medium), 3);
    }

    #[test]
    fn test_pressure_to_level_high() {
        let gov = make_gov();
        assert_eq!(gov.pressure_to_level(PressureLevel::High), 4);
    }

    #[test]
    fn test_pressure_to_level_critical() {
        let gov = make_gov();
        assert_eq!(gov.pressure_to_level(PressureLevel::Critical), 5);
    }

    // ── compute_transition (ramp-up) ─────────────────────────────────

    #[test]
    fn test_ramp_up_immediate() {
        let mut gov = make_gov();
        gov.current_level = 0;
        let result = gov.compute_transition(3);
        assert_eq!(result, 3, "ramp-up must be immediate");
        assert_eq!(gov.hysteresis_count, 0, "hysteresis reset on ramp-up");
    }

    #[test]
    fn test_ramp_up_from_any_level() {
        let mut gov = make_gov();
        for start in 0..=4u8 {
            gov.current_level = start;
            let target = start + 1;
            let result = gov.compute_transition(target);
            assert_eq!(
                result, target,
                "ramp-up from {start} to {target} should be immediate"
            );
        }
    }

    #[test]
    fn test_ramp_up_resets_full_scans_tracking() {
        let mut gov = make_gov();
        gov.current_level = 0;
        gov.full_scans_at_level = 999;
        let _ = gov.compute_transition(4);
        assert_eq!(
            gov.full_scans_at_level, 0,
            "full_scans_at_level reset on ramp-up"
        );
    }

    // ── compute_transition (same level) ──────────────────────────────

    #[test]
    fn test_same_level_resets_hysteresis() {
        let mut gov = make_gov();
        gov.current_level = 3;
        gov.hysteresis_count = 42; // some pending count
        let result = gov.compute_transition(3);
        assert_eq!(result, 3);
        assert_eq!(
            gov.hysteresis_count, 0,
            "hysteresis reset on same-level tick"
        );
    }

    // ── compute_transition (ramp-down with cooldown) ─────────────────

    #[test]
    fn test_ramp_down_cooldown_blocks() {
        // min_level_duration_secs=10, and level_entered_at was just set
        // (the Governor was constructed moments ago). Elapsed < 10s → blocked.
        let mut gov = make_gov_with(|c| c.min_level_duration_secs = 10);
        gov.current_level = 4;

        let result = gov.compute_transition(1);
        assert_eq!(result, 4, "cooldown should block ramp-down");
        assert_eq!(gov.hysteresis_count, 0, "cooldown: no hysteresis increment");
    }

    #[test]
    fn test_ramp_down_cooldown_bypassed_after_duration() {
        // min_level_duration_secs=0 → cooldown never blocks
        let mut gov = make_gov_with(|c| {
            c.min_level_duration_secs = 0;
            c.hysteresis_readings = 1;
        });
        gov.current_level = 3;

        let result = gov.compute_transition(1);
        // hysteresis_readings=1, so immediately steps down
        assert_eq!(result, 2, "should step down by 1");
    }

    // ── compute_transition (ramp-down with hysteresis) ───────────────

    #[test]
    fn test_ramp_down_hysteresis_counting() {
        let mut gov = make_gov_with(|c| {
            c.min_level_duration_secs = 0;
            c.hysteresis_readings = 3;
        });
        gov.current_level = 4;

        // Call 1: starts counting
        assert_eq!(gov.compute_transition(1), 4);
        assert_eq!(gov.hysteresis_count, 1);

        // Call 2: counting
        assert_eq!(gov.compute_transition(1), 4);
        assert_eq!(gov.hysteresis_count, 2);

        // Call 3: threshold reached → step down
        assert_eq!(gov.compute_transition(1), 3);
        assert_eq!(gov.hysteresis_count, 0);
    }

    #[test]
    fn test_ramp_down_steps_one_at_a_time() {
        let mut gov = make_gov_with(|c| {
            c.min_level_duration_secs = 0;
            c.hysteresis_readings = 1;
        });
        gov.current_level = 5;

        // Each call steps down by exactly 1, regardless of how far the
        // target is.
        let r1 = gov.compute_transition(0);
        assert_eq!(r1, 4);
        gov.current_level = r1;

        let r2 = gov.compute_transition(0);
        assert_eq!(r2, 3);
        gov.current_level = r2;

        let r3 = gov.compute_transition(0);
        assert_eq!(r3, 2);
    }

    #[test]
    fn test_ramp_down_resets_on_ramp_up_midway() {
        let mut gov = make_gov_with(|c| {
            c.min_level_duration_secs = 0;
            c.hysteresis_readings = 5; // lots of readings needed
        });
        gov.current_level = 4;

        // Start ramping down (2 hysteresis ticks)
        let _ = gov.compute_transition(1);
        let _ = gov.compute_transition(1);
        assert_eq!(gov.hysteresis_count, 2);

        // PSI spikes back up before we finished stepping down
        let r = gov.compute_transition(5); // target > current → ramp-up
        assert_eq!(r, 5, "ramp-up must override pending ramp-down");
        assert_eq!(gov.hysteresis_count, 0, "hysteresis reset by ramp-up");
    }

    // ── apply_profile (dry-run) ──────────────────────────────────────

    #[tokio::test]
    async fn test_apply_profile_dry_run_changes_level() {
        let mut ksm = KsmController::new("/sys/kernel/mm/ksm").unwrap();
        ksm.set_dry_run(true);
        let mut gov = Governor::new(GovernorConfig::default(), ksm);
        let state = new_shared_state();

        assert_eq!(gov.current_level, 0);
        gov.apply_profile(3, &state).await;
        assert_eq!(gov.current_level, 3);
    }

    #[tokio::test]
    async fn test_apply_profile_updates_governor_action() {
        let mut ksm = KsmController::new("/sys/kernel/mm/ksm").unwrap();
        ksm.set_dry_run(true);
        let mut gov = Governor::new(GovernorConfig::default(), ksm);
        let state = new_shared_state();

        gov.apply_profile(2, &state).await;

        let s = state.read().await;
        assert!(
            s.last_governor_action.elapsed().as_secs() < 5,
            "last_governor_action should have been updated very recently"
        );
        assert!(
            s.last_global_action.elapsed().as_secs() < 5,
            "last_global_action telemetry should have been updated very recently"
        );
    }

    #[tokio::test]
    async fn test_apply_profile_idle_stops_ksm() {
        let mut ksm = KsmController::new("/sys/kernel/mm/ksm").unwrap();
        ksm.set_dry_run(true);
        let mut gov = Governor::new(GovernorConfig::default(), ksm);
        let state = new_shared_state();

        // Going to level 0 (Idle) should be fine
        gov.apply_profile(0, &state).await;
        assert_eq!(gov.current_level, 0);
    }

    #[test]
    fn test_observe_psi_stability_counts_stable_and_resets_on_jump() {
        let mut gov = make_temp_gov_with(|_| {});

        gov.observe_psi_stability(&psi(1.0, 0.5));
        assert_eq!(gov.stable_psi_ticks, 0);

        gov.observe_psi_stability(&psi(1.2, 0.7));
        assert_eq!(gov.stable_psi_ticks, 1);

        gov.observe_psi_stability(&psi(3.0, 0.7));
        assert_eq!(gov.stable_psi_ticks, 0);
    }

    #[tokio::test]
    async fn test_tick_with_readings_applies_profile_and_updates_shared_state() {
        let mut gov = make_temp_gov_with(|c| {
            c.stabilization_secs = 30;
            c.min_level_duration_secs = 0;
        });
        let state = new_shared_state();
        {
            let mut s = state.write().await;
            s.last_governor_action = Instant::now() - Duration::from_secs(31);
        }

        gov.tick_with_readings(&state, psi(30.0, 25.0), ksm_stats(1234, 5678, 9))
            .await;

        assert_eq!(gov.current_level, 5);
        let s = state.read().await;
        assert_eq!(s.current_pressure, PressureLevel::Critical);
        assert_eq!(s.ksm_level, 5);
        assert_eq!(s.pages_sharing, 1234);
        assert_eq!(s.general_profit, 5678);
        assert!(s.last_governor_action.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn test_tick_with_readings_defers_profile_inside_stabilization_window() {
        let mut gov = make_temp_gov_with(|c| c.stabilization_secs = 30);
        let state = new_shared_state();
        {
            let mut s = state.write().await;
            s.last_governor_action = Instant::now();
        }

        gov.tick_with_readings(&state, psi(30.0, 25.0), ksm_stats(10, 20, 1))
            .await;

        assert_eq!(gov.current_level, 0);
        let s = state.read().await;
        assert_eq!(s.current_pressure, PressureLevel::Critical);
        assert_eq!(s.ksm_level, 0);
        assert_eq!(s.pages_sharing, 10);
        assert_eq!(s.general_profit, 20);
    }

    #[tokio::test]
    async fn test_tick_with_readings_reduces_level_on_sustained_negative_profit() {
        let mut gov = make_temp_gov_with(|c| {
            c.stabilization_secs = 0;
            c.min_level_duration_secs = 0;
        });
        let state = new_shared_state();
        gov.current_level = 4;
        gov.full_scans_at_level = 1;

        gov.tick_with_readings(&state, psi(12.0, 8.0), ksm_stats(10, -50, 20))
            .await;

        assert_eq!(gov.current_level, 3);
        let s = state.read().await;
        assert_eq!(s.ksm_level, 3);
        assert_eq!(s.general_profit, -50);
    }
}

//! zramdedup-common: Shared library for zramdedup components.
//!
//! Provides PSI monitoring, KSM control, process analysis, and
//! configuration management for the zramdedup memory optimization system.

pub mod config;
pub mod error;
pub mod ksm;
pub mod procfs;
pub mod psi;

use psi::PressureLevel;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

/// Shared state between governor and scanner tasks.
#[derive(Debug)]
pub struct GovernorState {
    pub current_pressure: PressureLevel,
    pub ksm_level: u8,
    pub pages_sharing: u64,
    pub general_profit: i64,
    pub last_adjustment: Instant,
    /// Timestamp of the last global actuation change (KSM param write,
    /// madvise burst, or dedup table expansion). Used to enforce the
    /// stabilization window and prevent coupled feedback oscillation.
    pub last_global_action: Instant,
    pub scanner_enabled: bool,
}

impl Default for GovernorState {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            current_pressure: PressureLevel::Idle,
            ksm_level: 0,
            pages_sharing: 0,
            general_profit: 0,
            last_adjustment: now,
            last_global_action: now,
            scanner_enabled: false,
        }
    }
}

/// Shared handle for governor state, passed between tasks.
pub type SharedGovernorState = Arc<RwLock<GovernorState>>;

/// Create a new shared governor state.
pub fn new_shared_state() -> SharedGovernorState {
    Arc::new(RwLock::new(GovernorState::default()))
}

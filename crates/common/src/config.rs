//! Configuration schema for zramdedup, deserialized from TOML.

use serde::Deserialize;
use std::path::Path;

/// Top-level configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ZramdedupConfig {
    pub general: GeneralConfig,
    pub governor: GovernorConfig,
    pub scanner: ScannerConfig,
    pub swap_proxy: SwapProxyConfig,
}

impl ZramdedupConfig {
    /// Load configuration from a TOML file path.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    /// Validate configuration values are within safe ranges.
    pub fn validate(&self) -> anyhow::Result<()> {
        // Governor validation
        if self.governor.psi_some_threshold < 0.0 || self.governor.psi_some_threshold > 100.0 {
            anyhow::bail!("governor.psi_some_threshold must be between 0 and 100");
        }
        if self.governor.psi_full_threshold < 0.0 || self.governor.psi_full_threshold > 100.0 {
            anyhow::bail!("governor.psi_full_threshold must be between 0 and 100");
        }
        if self.governor.advisor_scan_time_range.0 >= self.governor.advisor_scan_time_range.1 {
            anyhow::bail!("governor.advisor_scan_time_range: min must be < max");
        }
        if self.governor.max_page_sharing_range.0 >= self.governor.max_page_sharing_range.1 {
            anyhow::bail!("governor.max_page_sharing_range: min must be < max");
        }

        // Scanner validation
        if self.scanner.min_anon_rss_mb == 0 {
            anyhow::bail!("scanner.min_anon_rss_mb must be > 0");
        }
        if self.scanner.duplicate_ratio_threshold <= 0.0
            || self.scanner.duplicate_ratio_threshold > 1.0
        {
            anyhow::bail!("scanner.duplicate_ratio_threshold must be in (0, 1]");
        }

        // Swap proxy validation
        if self.swap_proxy.device_size_gb == 0 {
            anyhow::bail!("swap_proxy.device_size_gb must be > 0");
        }
        if self.swap_proxy.dedup_table_max_entries == 0 {
            anyhow::bail!("swap_proxy.dedup_table_max_entries must be > 0");
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    pub log_level: String,
    pub poll_interval_ms: u64,
    pub state_dir: String,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            log_level: "info".to_string(),
            poll_interval_ms: 2000,
            state_dir: "/var/lib/zramdedup".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GovernorConfig {
    pub enabled: bool,
    /// When true, kernel advisor controls scan rate; governor only biases
    /// max_page_sharing and sleep bounds (Option B — avoids fighting the advisor).
    /// When false, governor has full manual control over all KSM parameters.
    pub use_advisor: bool,
    pub psi_some_threshold: f32,
    pub psi_full_threshold: f32,
    pub advisor_scan_time_range: (u64, u64),
    pub max_page_sharing_range: (u32, u32),
    pub hysteresis_readings: u32,
    pub min_level_duration_secs: u64,
    /// Stabilization window (seconds): minimum time between repeated actions
    /// within the same tier. Governor and scanner keep separate timestamps so
    /// one tier cannot starve another.
    pub stabilization_secs: u64,
    /// Path to KSM sysfs directory.
    pub ksm_path: String,
}

impl Default for GovernorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            use_advisor: true,
            psi_some_threshold: 5.0,
            psi_full_threshold: 2.0,
            advisor_scan_time_range: (30, 600),
            max_page_sharing_range: (256, 1024),
            hysteresis_readings: 4,
            min_level_duration_secs: 30,
            stabilization_secs: 30,
            ksm_path: "/sys/kernel/mm/ksm".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ScannerConfig {
    pub enabled: bool,
    pub interval_secs: u64,
    pub min_anon_rss_mb: u64,
    pub duplicate_ratio_threshold: f32,
    pub max_mergeable_per_process_mb: u64,
    pub blocklist: Vec<String>,
    pub target_cgroups: Vec<String>,
    /// Maximum number of processes to apply madvise to per scan cycle.
    /// Limits CPU overhead from syscall bursts.
    pub max_candidates_per_cycle: usize,
}

impl Default for ScannerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: 30,
            min_anon_rss_mb: 100,
            duplicate_ratio_threshold: 0.15,
            max_mergeable_per_process_mb: 512,
            blocklist: vec![
                "zramdedup".to_string(),
                "ksmd".to_string(),
                "systemd".to_string(),
                "sshd".to_string(),
            ],
            target_cgroups: vec![],
            max_candidates_per_cycle: 5,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SwapProxyConfig {
    pub enabled: bool,
    pub device_size_gb: u64,
    pub fingerprint: String,
    pub dedup_table_max_entries: u64,
    pub zram_backend: String,
    pub bloom_capacity: usize,
    pub bloom_false_positive_rate: f64,
    pub page_store_path: String,
}

impl Default for SwapProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            device_size_gb: 8,
            fingerprint: "xxh3-128".to_string(),
            dedup_table_max_entries: 1_000_000,
            zram_backend: "/dev/zram0".to_string(),
            bloom_capacity: 1_000_000,
            bloom_false_positive_rate: 0.01,
            page_store_path: "/var/lib/zramdedup/pagestore.dat".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_validates() {
        let cfg = ZramdedupConfig::default();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_parse_toml() {
        let toml_str = r#"
[general]
log_level = "debug"
poll_interval_ms = 1000

[governor]
enabled = true
use_advisor = true

[scanner]
enabled = false

[swap_proxy]
enabled = false
"#;
        let cfg: ZramdedupConfig = toml::from_str(toml_str).unwrap();
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.general.log_level, "debug");
        assert_eq!(cfg.general.poll_interval_ms, 1000);
        assert!(!cfg.scanner.enabled);
    }

    #[test]
    fn test_validate_rejects_invalid_governor_thresholds() {
        let mut cfg = ZramdedupConfig::default();
        cfg.governor.psi_some_threshold = -0.1;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("psi_some_threshold"));

        cfg = ZramdedupConfig::default();
        cfg.governor.psi_full_threshold = 101.0;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("psi_full_threshold"));
    }

    #[test]
    fn test_validate_rejects_invalid_governor_ranges() {
        let mut cfg = ZramdedupConfig::default();
        cfg.governor.advisor_scan_time_range = (10, 10);
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("advisor_scan_time_range"));

        cfg = ZramdedupConfig::default();
        cfg.governor.max_page_sharing_range = (1024, 256);
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("max_page_sharing_range"));
    }

    #[test]
    fn test_validate_rejects_invalid_scanner_values() {
        let mut cfg = ZramdedupConfig::default();
        cfg.scanner.min_anon_rss_mb = 0;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("min_anon_rss_mb"));

        cfg = ZramdedupConfig::default();
        cfg.scanner.duplicate_ratio_threshold = 0.0;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("duplicate_ratio_threshold"));
    }

    #[test]
    fn test_validate_rejects_invalid_swap_proxy_values() {
        let mut cfg = ZramdedupConfig::default();
        cfg.swap_proxy.device_size_gb = 0;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("device_size_gb"));

        cfg = ZramdedupConfig::default();
        cfg.swap_proxy.dedup_table_max_entries = 0;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("dedup_table_max_entries"));
    }
}

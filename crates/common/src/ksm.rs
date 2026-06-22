//! KSM (Kernel Same-page Merging) sysfs controller.
//!
//! Provides a safe abstraction over /sys/kernel/mm/ksm/ with
//! validated writes, snapshot/restore, and rate limiting.

use crate::error::{Result, ZramdedupError};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Minimum interval between KSM parameter updates.
const MIN_UPDATE_INTERVAL_MS: u64 = 500;

/// Snapshot file name within state directory.
const SNAPSHOT_FILENAME: &str = "ksm-snapshot.json";

/// Safe parameter ranges for KSM tunables.
const PARAM_RANGES: &[(&str, u64, u64)] = &[
    ("run", 0, 2),
    ("pages_to_scan", 100, 30000),
    ("sleep_millisecs", 5, 10000),
    ("max_page_sharing", 16, 4096),
    ("advisor_target_scan_time", 10, 3600),
    ("advisor_max_cpu", 10, 100),
    ("advisor_min_pages_to_scan", 100, 5000),
    ("advisor_max_pages_to_scan", 1000, 100000),
];

/// Current KSM runtime statistics read from sysfs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KsmStats {
    pub pages_shared: u64,
    pub pages_sharing: u64,
    pub pages_unshared: u64,
    pub pages_volatile: u64,
    pub pages_scanned: u64,
    pub pages_skipped: u64,
    pub full_scans: u64,
    pub general_profit: i64,
    pub ksm_zero_pages: u64,
    pub stable_node_chains: u64,
    pub stable_node_dups: u64,
}

/// KSM configuration state (the writable parameters).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KsmConfig {
    pub run: u64,
    pub pages_to_scan: u64,
    pub sleep_millisecs: u64,
    pub max_page_sharing: u64,
    pub smart_scan: u64,
    pub advisor_mode: String,
    pub advisor_target_scan_time: u64,
    pub advisor_max_cpu: u64,
    pub advisor_min_pages_to_scan: u64,
    pub advisor_max_pages_to_scan: u64,
}

/// Snapshot of KSM state for restore on shutdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KsmSnapshot {
    pub config: KsmConfig,
    pub timestamp: String,
}

/// KSM sysfs controller with validated writes and rate limiting.
pub struct KsmController {
    base_path: PathBuf,
    last_update: Instant,
    dry_run: bool,
}

impl KsmController {
    /// Create a new controller pointing at the KSM sysfs directory.
    pub fn new(ksm_path: &str) -> Result<Self> {
        let base_path = PathBuf::from(ksm_path);
        if !base_path.exists() {
            return Err(ZramdedupError::Sysfs {
                path: base_path,
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "KSM path not found"),
            });
        }
        Ok(Self {
            base_path,
            last_update: Instant::now() - std::time::Duration::from_millis(MIN_UPDATE_INTERVAL_MS),
            dry_run: false,
        })
    }

    /// Enable dry-run mode (log writes without executing).
    pub fn set_dry_run(&mut self, dry_run: bool) {
        self.dry_run = dry_run;
    }

    /// Read all KSM statistics.
    pub fn read_stats(&self) -> Result<KsmStats> {
        Ok(KsmStats {
            pages_shared: self.read_u64("pages_shared")?,
            pages_sharing: self.read_u64("pages_sharing")?,
            pages_unshared: self.read_u64("pages_unshared")?,
            pages_volatile: self.read_u64("pages_volatile")?,
            pages_scanned: self.read_u64("pages_scanned")?,
            pages_skipped: self.read_u64("pages_skipped")?,
            full_scans: self.read_u64("full_scans")?,
            general_profit: self.read_i64("general_profit")?,
            ksm_zero_pages: self.read_u64("ksm_zero_pages").unwrap_or(0),
            stable_node_chains: self.read_u64("stable_node_chains").unwrap_or(0),
            stable_node_dups: self.read_u64("stable_node_dups").unwrap_or(0),
        })
    }

    /// Read all KSM configuration parameters.
    pub fn read_config(&self) -> Result<KsmConfig> {
        Ok(KsmConfig {
            run: self.read_u64("run")?,
            pages_to_scan: self.read_u64("pages_to_scan")?,
            sleep_millisecs: self.read_u64("sleep_millisecs")?,
            max_page_sharing: self.read_u64("max_page_sharing")?,
            smart_scan: self.read_u64("smart_scan").unwrap_or(1),
            advisor_mode: self.read_string("advisor_mode").unwrap_or_default(),
            advisor_target_scan_time: self
                .read_u64("advisor_target_scan_time")
                .unwrap_or(200),
            advisor_max_cpu: self.read_u64("advisor_max_cpu").unwrap_or(70),
            advisor_min_pages_to_scan: self
                .read_u64("advisor_min_pages_to_scan")
                .unwrap_or(500),
            advisor_max_pages_to_scan: self
                .read_u64("advisor_max_pages_to_scan")
                .unwrap_or(30000),
        })
    }

    /// Snapshot current KSM config to a JSON file for later restore.
    ///
    /// Uses atomic write (temp file + rename) to prevent crash corruption.
    pub fn snapshot(&self, state_dir: &Path) -> Result<()> {
        let config = self.read_config()?;
        let snapshot = KsmSnapshot {
            config,
            timestamp: chrono_now(),
        };
        let json = serde_json::to_string_pretty(&snapshot)
            .map_err(|e| ZramdedupError::Snapshot(e.to_string()))?;

        fs::create_dir_all(state_dir)?;
        let path = state_dir.join(SNAPSHOT_FILENAME);
        let tmp_path = state_dir.join(format!("{SNAPSHOT_FILENAME}.tmp"));

        // Write to temp file first, then atomic rename.
        // This prevents a crash mid-write from corrupting the snapshot.
        fs::write(&tmp_path, json)?;
        fs::rename(&tmp_path, &path)?;

        tracing::info!(path = %path.display(), "KSM state snapshotted (atomic write)");
        Ok(())
    }

    /// Restore KSM config from a previously saved snapshot.
    pub fn restore(&mut self, state_dir: &Path) -> Result<()> {
        let path = state_dir.join(SNAPSHOT_FILENAME);
        if !path.exists() {
            tracing::warn!(path = %path.display(), "No KSM snapshot found, skipping restore");
            return Ok(());
        }

        let json = fs::read_to_string(&path)?;
        let snapshot: KsmSnapshot =
            serde_json::from_str(&json).map_err(|e| ZramdedupError::Snapshot(e.to_string()))?;

        tracing::info!(
            timestamp = %snapshot.timestamp,
            "Restoring KSM state from snapshot"
        );

        // Restore advisor mode FIRST so we can write pages_to_scan (kernel rejects
        // direct writes to pages_to_scan while a scan-time advisor is active).
        if snapshot.config.run == 0 || snapshot.config.advisor_mode != "scan-time" {
            // Safe to set advisor mode to the snapshot value before writing pages_to_scan
            let _ = self.set_advisor_mode(&snapshot.config.advisor_mode);
        } else {
            // We were running with an advisor; disable it temporarily so we can restore
            // pages_to_scan, then re-enable the advisor mode.
            let _ = self.set_advisor_mode("none");
        }

        self.set_run(snapshot.config.run)?;
        self.set_pages_to_scan(snapshot.config.pages_to_scan)?;
        self.set_sleep_millisecs(snapshot.config.sleep_millisecs)?;
        self.set_max_page_sharing(snapshot.config.max_page_sharing)?;

        // Restore advisor settings if available (re-enable if we disabled it above)
        let _ = self.set_advisor_mode(&snapshot.config.advisor_mode);
        let _ = self.set_advisor_target_scan_time(snapshot.config.advisor_target_scan_time);
        let _ = self.set_advisor_max_cpu(snapshot.config.advisor_max_cpu);

        Ok(())
    }

    /// Set KSM run mode (0=stop, 1=run, 2=unmerge).
    pub fn set_run(&self, value: u64) -> Result<()> {
        self.write_validated("run", value)
    }

    pub fn set_pages_to_scan(&self, value: u64) -> Result<()> {
        self.write_validated("pages_to_scan", value)
    }

    pub fn set_sleep_millisecs(&self, value: u64) -> Result<()> {
        self.write_validated("sleep_millisecs", value)
    }

    pub fn set_max_page_sharing(&self, value: u64) -> Result<()> {
        self.write_validated("max_page_sharing", value)
    }

    pub fn set_advisor_mode(&self, mode: &str) -> Result<()> {
        self.write_string("advisor_mode", mode)
    }

    pub fn set_advisor_target_scan_time(&self, value: u64) -> Result<()> {
        self.write_validated("advisor_target_scan_time", value)
    }

    pub fn set_advisor_max_cpu(&self, value: u64) -> Result<()> {
        self.write_validated("advisor_max_cpu", value)
    }

    /// Write a validated integer parameter to sysfs.
    fn write_validated(&self, param: &str, value: u64) -> Result<()> {
        let clamped = self.clamp_value(param, value);
        if clamped != value {
            tracing::warn!(
                param,
                requested = value,
                clamped_to = clamped,
                "KSM parameter clamped to safe range"
            );
        }

        self.write_param(param, &clamped.to_string())
    }

    /// Write a string parameter to sysfs.
    fn write_string(&self, param: &str, value: &str) -> Result<()> {
        self.write_param(param, value)
    }

    fn write_param(&self, param: &str, value: &str) -> Result<()> {
        // Rate limit
        let elapsed = self.last_update.elapsed();
        if elapsed.as_millis() < MIN_UPDATE_INTERVAL_MS as u128 {
            tracing::trace!(
                param,
                "Skipping KSM write (rate limited, {:.0}ms since last update)",
                elapsed.as_millis()
            );
            return Ok(());
        }

        let path = self.base_path.join(param);

        if self.dry_run {
            tracing::info!(param, value, "[DRY RUN] Would write KSM parameter");
            return Ok(());
        }

        fs::write(&path, value).map_err(|e| ZramdedupError::Sysfs {
            path: path.clone(),
            source: e,
        })?;

        // Verify write
        let readback = fs::read_to_string(&path)
            .unwrap_or_default()
            .trim()
            .to_string();

        // For advisor_mode the kernel may add brackets around the active mode
        if !readback.contains(value) {
            tracing::warn!(
                param,
                expected = value,
                actual = readback,
                "KSM parameter write verification mismatch"
            );
        }

        tracing::debug!(param, value, readback = %readback, "KSM parameter written");
        Ok(())
    }

    fn clamp_value(&self, param: &str, value: u64) -> u64 {
        for &(name, min, max) in PARAM_RANGES {
            if name == param {
                return value.clamp(min, max);
            }
        }
        value // Unknown param, pass through
    }

    fn read_u64(&self, param: &str) -> Result<u64> {
        let path = self.base_path.join(param);
        let content = fs::read_to_string(&path).map_err(|e| ZramdedupError::Sysfs {
            path,
            source: e,
        })?;
        // Handle advisor_mode style values like "[none] scan-time"
        let trimmed = content.trim();
        // Strip brackets if present (for enum-style values)
        let cleaned = trimmed.trim_start_matches('[').split(']').next().unwrap_or(trimmed);
        cleaned.parse::<u64>().map_err(|_| ZramdedupError::Sysfs {
            path: self.base_path.join(param),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("cannot parse '{trimmed}' as u64"),
            ),
        })
    }

    fn read_i64(&self, param: &str) -> Result<i64> {
        let path = self.base_path.join(param);
        let content = fs::read_to_string(&path).map_err(|e| ZramdedupError::Sysfs {
            path,
            source: e,
        })?;
        content
            .trim()
            .parse::<i64>()
            .map_err(|_| ZramdedupError::Sysfs {
                path: self.base_path.join(param),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("cannot parse '{}' as i64", content.trim()),
                ),
            })
    }

    fn read_string(&self, param: &str) -> Result<String> {
        let path = self.base_path.join(param);
        let content = fs::read_to_string(&path).map_err(|e| ZramdedupError::Sysfs {
            path,
            source: e,
        })?;
        // Extract the active value from bracket notation like "[none] scan-time"
        let trimmed = content.trim();
        if trimmed.starts_with('[') {
            if let Some(active) = trimmed.strip_prefix('[').and_then(|s| s.split(']').next()) {
                return Ok(active.to_string());
            }
        }
        Ok(trimmed.to_string())
    }

    /// Read all sysfs files in the KSM directory into a map (for diagnostics).
    pub fn read_all_raw(&self) -> HashMap<String, String> {
        let mut map = HashMap::new();
        if let Ok(entries) = fs::read_dir(&self.base_path) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Ok(content) = fs::read_to_string(entry.path()) {
                    map.insert(name, content.trim().to_string());
                }
            }
        }
        map
    }
}

/// Simple ISO-like timestamp without pulling in chrono.
fn chrono_now() -> String {
    use std::time::SystemTime;
    let duration = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clamp_value() {
        let ctrl = KsmController {
            base_path: PathBuf::from("/tmp"),
            last_update: Instant::now(),
            dry_run: true,
        };
        assert_eq!(ctrl.clamp_value("run", 5), 2);
        assert_eq!(ctrl.clamp_value("pages_to_scan", 50), 100);
        assert_eq!(ctrl.clamp_value("pages_to_scan", 50000), 30000);
        assert_eq!(ctrl.clamp_value("sleep_millisecs", 3), 5);
        assert_eq!(ctrl.clamp_value("unknown_param", 42), 42);
    }
}

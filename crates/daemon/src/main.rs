//! zramdedup daemon: PSI-aware KSM governor + process scanner.
//!
//! Trades CPU cycles for effective RAM by dynamically tuning KSM
//! scanning aggressiveness based on memory pressure, and proactively
//! marking duplicate-heavy processes for KSM merging.

mod governor;
mod madvise;
mod metrics;
mod scanner;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tokio::signal;
use tokio::sync::watch;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use zramdedup_common::config::ZramdedupConfig;
use zramdedup_common::ksm::KsmController;
use zramdedup_common::new_shared_state;

#[derive(Parser)]
#[command(name = "zramdedup", about = "CPU-for-RAM memory optimization daemon")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the zramdedup daemon
    Run {
        /// Path to configuration file
        #[arg(short, long, default_value = "/etc/zramdedup.toml")]
        config: PathBuf,

        /// Dry-run mode: log actions without executing sysfs writes or madvise calls
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Show current status
    Status {
        /// Path to configuration file
        #[arg(short, long, default_value = "/etc/zramdedup.toml")]
        config: PathBuf,
    },
    /// Restore KSM parameters from snapshot
    RestoreKsm {
        /// Path to configuration file
        #[arg(short, long, default_value = "/etc/zramdedup.toml")]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run { config, dry_run } => run_daemon(config, dry_run).await,
        Commands::Status { config } => show_status(config).await,
        Commands::RestoreKsm { config } => restore_ksm(config).await,
    }
}

async fn run_daemon(config_path: PathBuf, dry_run: bool) -> anyhow::Result<()> {
    // Load configuration
    let config = load_config_or_default(&config_path)?;

    // Initialize tracing
    init_tracing(&config.general.log_level);

    info!(
        version = env!("CARGO_PKG_VERSION"),
        dry_run,
        governor_enabled = config.governor.enabled,
        scanner_enabled = config.scanner.enabled,
        "zramdedup starting"
    );

    // Initialize KSM controller
    let mut ksm = KsmController::new(&config.governor.ksm_path)?;
    if dry_run {
        ksm.set_dry_run(true);
    }

    // Snapshot current KSM state for restore on shutdown
    let state_dir = PathBuf::from(&config.general.state_dir);
    if !dry_run {
        ksm.snapshot(&state_dir)?;
    }

    // Shared state between governor and scanner
    let shared_state = new_shared_state();

    // Shutdown channel
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Spawn governor task
    let governor_handle = if config.governor.enabled {
        let gov_config = config.governor.clone();
        let gov_ksm = KsmController::new(&gov_config.ksm_path)?;
        if dry_run {
            // Create a new controller for the governor with dry_run
            let mut gov_ksm = KsmController::new(&gov_config.ksm_path)?;
            gov_ksm.set_dry_run(true);
            let gov = governor::Governor::new(gov_config, gov_ksm);
            let state = shared_state.clone();
            let rx = shutdown_rx.clone();
            Some(tokio::spawn(async move {
                if let Err(e) = gov.run(state, rx).await {
                    error!(error = %e, "Governor task failed");
                }
            }))
        } else {
            let gov = governor::Governor::new(gov_config, gov_ksm);
            let state = shared_state.clone();
            let rx = shutdown_rx.clone();
            Some(tokio::spawn(async move {
                if let Err(e) = gov.run(state, rx).await {
                    error!(error = %e, "Governor task failed");
                }
            }))
        }
    } else {
        None
    };

    // Spawn scanner task
    let scanner_handle = if config.scanner.enabled {
        let scan_config = config.scanner.clone();
        let stabilization = config.governor.stabilization_secs;
        let state = shared_state.clone();
        let rx = shutdown_rx.clone();
        Some(tokio::spawn(async move {
            let scanner = scanner::Scanner::new(scan_config, dry_run, stabilization);
            if let Err(e) = scanner.run(state, rx).await {
                error!(error = %e, "Scanner task failed");
            }
        }))
    } else {
        None
    };

    // Spawn metrics task
    let metrics_state = shared_state.clone();
    let metrics_rx = shutdown_rx.clone();
    let metrics_config_path = config.governor.ksm_path.clone();
    let metrics_handle = tokio::spawn(async move {
        if let Ok(ksm) = KsmController::new(&metrics_config_path) {
            metrics::run_metrics_loop(&ksm, metrics_state, 60, metrics_rx).await;
        }
    });

    info!("All tasks spawned, waiting for shutdown signal");

    // Wait for SIGTERM or SIGINT
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())?;
    let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt())?;

    tokio::select! {
        _ = sigterm.recv() => {
            info!("Received SIGTERM");
        }
        _ = sigint.recv() => {
            info!("Received SIGINT");
        }
    }

    // Signal all tasks to shut down
    info!("Initiating graceful shutdown");
    let _ = shutdown_tx.send(true);

    // Wait for tasks to complete (with timeout)
    let shutdown_timeout = tokio::time::Duration::from_secs(5);
    let _ = tokio::time::timeout(shutdown_timeout, async {
        if let Some(h) = governor_handle {
            let _ = h.await;
        }
        if let Some(h) = scanner_handle {
            let _ = h.await;
        }
        let _ = metrics_handle.await;
    })
    .await;

    // Restore KSM state
    if !dry_run {
        let mut ksm = KsmController::new(&config.governor.ksm_path)?;
        if let Err(e) = ksm.restore(&state_dir) {
            error!(error = %e, "Failed to restore KSM state");
        } else {
            info!("KSM state restored to pre-daemon values");
        }
    }

    info!("zramdedup shutdown complete");
    Ok(())
}

async fn show_status(config_path: PathBuf) -> anyhow::Result<()> {
    let config = load_config_or_default(&config_path)?;

    init_tracing("info");

    let ksm = KsmController::new(&config.governor.ksm_path)?;
    let stats = ksm.read_stats()?;
    let cfg = ksm.read_config()?;

    let psi = zramdedup_common::psi::PsiStats::read_memory()?;

    println!("=== zramdedup status ===");
    println!();
    println!("KSM Configuration:");
    println!("  run:                    {}", cfg.run);
    println!("  pages_to_scan:          {}", cfg.pages_to_scan);
    println!("  sleep_millisecs:        {}", cfg.sleep_millisecs);
    println!("  max_page_sharing:       {}", cfg.max_page_sharing);
    println!("  advisor_mode:           {}", cfg.advisor_mode);
    println!("  smart_scan:             {}", cfg.smart_scan);
    println!();
    println!("KSM Statistics:");
    println!("  pages_shared:           {}", stats.pages_shared);
    println!("  pages_sharing:          {}", stats.pages_sharing);
    println!("  pages_unshared:         {}", stats.pages_unshared);
    println!("  general_profit:         {}", stats.general_profit);
    println!("  full_scans:             {}", stats.full_scans);
    println!(
        "  sharing_savings_mb:     {:.1}",
        stats.pages_sharing as f64 * 4096.0 / 1024.0 / 1024.0
    );
    println!();
    println!("Memory Pressure (PSI):");
    println!("  some avg10:             {:.2}%", psi.some.avg10);
    println!("  some avg60:             {:.2}%", psi.some.avg60);
    println!("  full avg10:             {:.2}%", psi.full.avg10);
    println!("  full avg60:             {:.2}%", psi.full.avg60);

    Ok(())
}

async fn restore_ksm(config_path: PathBuf) -> anyhow::Result<()> {
    let config = load_config_or_default(&config_path)?;

    init_tracing("info");

    let mut ksm = KsmController::new(&config.governor.ksm_path)?;
    let state_dir = PathBuf::from(&config.general.state_dir);
    ksm.restore(&state_dir)?;
    info!("KSM state restored successfully");

    Ok(())
}

fn init_tracing(log_level: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level));

    // Try journald first, fall back to stderr
    match tracing_journald::layer() {
        Ok(_layer) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .init();
            // Note: journald layer is created but we use stderr writer for simplicity
            // In production, use tracing_subscriber::registry().with(layer).init()
        }
        Err(_) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .init();
        }
    }
}

/// Load config from path or return defaults (with info log if file missing).
fn load_config_or_default(config_path: &PathBuf) -> anyhow::Result<ZramdedupConfig> {
    if config_path.exists() {
        Ok(ZramdedupConfig::load(config_path)?)
    } else {
        info!(
            path = %config_path.display(),
            "Config file not found, using defaults"
        );
        Ok(ZramdedupConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_config_or_default_nonexistent_returns_default() {
        let path = PathBuf::from("/nonexistent/path/zramdedup.toml");
        let config = load_config_or_default(&path).unwrap();
        assert_eq!(config.governor.stabilization_secs, 30);
        assert_eq!(config.general.log_level, "info");
    }
}

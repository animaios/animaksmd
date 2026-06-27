//! animaksm daemon: PSI-aware KSM governor + process scanner.
//!
//! Trades CPU cycles for effective RAM by dynamically tuning KSM
//! scanning aggressiveness based on memory pressure, and proactively
//! marking duplicate-heavy processes for KSM merging.

mod governor;
mod madvise;
mod metrics;
mod scanner;

use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use tokio::signal;
use tokio::sync::watch;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use animaksm_common::config::AnimaksmConfig;
use animaksm_common::ksm::{KsmConfig, KsmController, KsmStats};
use animaksm_common::new_shared_state;
use animaksm_common::psi::PsiStats;

#[derive(Parser)]
#[command(name = "animaksm", about = "CPU-for-RAM memory optimization daemon")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the animaksm daemon
    Run {
        /// Path to configuration file
        #[arg(short, long, default_value = "/etc/animaksm.toml")]
        config: PathBuf,

        /// Dry-run mode: log actions without executing sysfs writes or madvise calls
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Show current status
    Status {
        /// Path to configuration file
        #[arg(short, long, default_value = "/etc/animaksm.toml")]
        config: PathBuf,
    },
    /// Show detailed KSM statistics (like uksmdstats)
    Stats {
        /// Path to configuration file
        #[arg(short, long, default_value = "/etc/animaksm.toml")]
        config: PathBuf,
    },
    /// Restore KSM parameters from snapshot
    RestoreKsm {
        /// Path to configuration file
        #[arg(short, long, default_value = "/etc/animaksm.toml")]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run { config, dry_run } => run_daemon(config, dry_run).await,
        Commands::Status { config } => show_status(config).await,
        Commands::Stats { config } => show_stats(config).await,
        Commands::RestoreKsm { config } => restore_ksm(config).await,
    }
}

async fn run_daemon(config_path: PathBuf, dry_run: bool) -> anyhow::Result<()> {
    run_daemon_with_shutdown(config_path, dry_run, wait_for_shutdown_signal()).await
}

async fn run_daemon_with_shutdown<S>(
    config_path: PathBuf,
    dry_run: bool,
    shutdown_signal: S,
) -> anyhow::Result<()>
where
    S: Future<Output = anyhow::Result<&'static str>>,
{
    // Load configuration
    let config = load_config_or_default(&config_path)?;

    // Initialize tracing
    init_tracing(&config.general.log_level);

    info!(
        version = env!("CARGO_PKG_VERSION"),
        dry_run,
        governor_enabled = config.governor.enabled,
        scanner_enabled = config.scanner.enabled,
        "animaksm starting"
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

    let signal_name = shutdown_signal.await?;
    match signal_name {
        "SIGTERM" => info!("Received SIGTERM"),
        "SIGINT" => info!("Received SIGINT"),
        other => info!(signal = other, "Received shutdown signal"),
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

    info!("animaksm shutdown complete");
    Ok(())
}

async fn wait_for_shutdown_signal() -> anyhow::Result<&'static str> {
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())?;
    let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt())?;

    tokio::select! {
        _ = sigterm.recv() => {
            Ok("SIGTERM")
        }
        _ = sigint.recv() => {
            Ok("SIGINT")
        }
    }
}

async fn show_status(config_path: PathBuf) -> anyhow::Result<()> {
    init_tracing("info");
    print!(
        "{}",
        build_status_output(&config_path, Path::new("/proc/pressure/memory"))?
    );

    Ok(())
}

async fn show_stats(config_path: PathBuf) -> anyhow::Result<()> {
    init_tracing("info");

    let config = load_config_or_default(&config_path)?;
    let ksm = KsmController::new(&config.governor.ksm_path)?;
    let stats = ksm.read_stats()?;
    let _cfg = ksm.read_config()?;

    println!("======================================================");
    println!("animaksm with KSM statistics support");
    println!("======================================================");

    // Read additional KSM stats from sysfs
    let ksm_path = Path::new(&config.governor.ksm_path);

    let full_scans = fs::read_to_string(ksm_path.join("full_scans"))
        .unwrap_or_default()
        .trim()
        .parse::<u64>()
        .unwrap_or(0);
    let sleep_millisecs = fs::read_to_string(ksm_path.join("sleep_millisecs"))
        .unwrap_or_default()
        .trim()
        .parse::<u64>()
        .unwrap_or(0);
    let max_page_sharing = fs::read_to_string(ksm_path.join("max_page_sharing"))
        .unwrap_or_default()
        .trim()
        .parse::<u64>()
        .unwrap_or(0);
    let pages_to_scan = fs::read_to_string(ksm_path.join("pages_to_scan"))
        .unwrap_or_default()
        .trim()
        .parse::<u64>()
        .unwrap_or(0);
    let stable_node_chains = fs::read_to_string(ksm_path.join("stable_node_chains"))
        .unwrap_or_default()
        .trim()
        .parse::<u64>()
        .unwrap_or(0);
    let stable_node_dups = fs::read_to_string(ksm_path.join("stable_node_dups"))
        .unwrap_or_default()
        .trim()
        .parse::<u64>()
        .unwrap_or(0);
    let use_zero_pages = fs::read_to_string(ksm_path.join("use_zero_pages"))
        .unwrap_or_default()
        .trim()
        .parse::<u64>()
        .unwrap_or(0);
    let pages_shared = stats.pages_shared;
    let pages_sharing = stats.pages_sharing;
    let pages_unshared = stats.pages_unshared;
    let general_profit = stats.general_profit;

    println!("Full scans:                 {}", full_scans);
    println!("Interval:                   {} ms", sleep_millisecs);
    println!("Max page sharing ratio:     {}", max_page_sharing);
    println!("Pages to scan:              {}", pages_to_scan);
    println!("Pages over ratio:           {}", stable_node_chains);
    println!("Duplicated pages:           {}", stable_node_dups);
    println!("Use zero pages:             {}", use_zero_pages);
    println!();

    if pages_shared > 0 && pages_sharing > 0 {
        let sharing_shared_ratio = pages_sharing as f64 / pages_shared as f64;
        let unshared_sharing_ratio = pages_unshared as f64 / pages_sharing as f64;
        println!("Sharing/shared ratio:       {:.4}", sharing_shared_ratio);
        println!("Unshared/sharing ratio:     {:.4}", unshared_sharing_ratio);
    } else {
        println!("Sharing/shared ratio:       0");
        println!("Unshared/sharing ratio:     0");
    }
    println!();

    // formula MiB: pages * page_size / (1024 * 1024) = pages / 256
    println!(
        "Pages sharing:              {:.1} MiB",
        pages_sharing as f64 / 256.0
    );
    println!(
        "Pages shared:               {:.1} MiB",
        pages_shared as f64 / 256.0
    );
    println!(
        "Pages unshared:             {:.1} MiB",
        pages_unshared as f64 / 256.0
    );
    println!();

    // general_profit is in bytes, convert to MiB
    println!(
        "General profit:             {:.1} MiB",
        general_profit as f64 / 1024.0 / 1024.0
    );

    Ok(())
}

fn build_status_output(config_path: &Path, psi_path: &Path) -> anyhow::Result<String> {
    let config = load_config_or_default(config_path)?;
    let ksm = KsmController::new(&config.governor.ksm_path)?;
    let stats = ksm.read_stats()?;
    let cfg = ksm.read_config()?;
    let psi = PsiStats::read_from(psi_path)?;

    Ok(format_status(&cfg, &stats, &psi))
}

fn format_status(cfg: &KsmConfig, stats: &KsmStats, psi: &PsiStats) -> String {
    format!(
        "=== animaksm status ===\n\
         \n\
         KSM Configuration:\n\
           run:                    {}\n\
           pages_to_scan:          {}\n\
           sleep_millisecs:        {}\n\
           max_page_sharing:       {}\n\
           advisor_mode:           {}\n\
           smart_scan:             {}\n\
         \n\
         KSM Statistics:\n\
           pages_shared:           {}\n\
           pages_sharing:          {}\n\
           pages_unshared:         {}\n\
           general_profit:         {}\n\
           full_scans:             {}\n\
           sharing_savings_mb:     {:.1}\n\
         \n\
         Memory Pressure (PSI):\n\
           some avg10:             {:.2}%\n\
           some avg60:             {:.2}%\n\
           full avg10:             {:.2}%\n\
           full avg60:             {:.2}%\n",
        cfg.run,
        cfg.pages_to_scan,
        cfg.sleep_millisecs,
        cfg.max_page_sharing,
        cfg.advisor_mode,
        cfg.smart_scan,
        stats.pages_shared,
        stats.pages_sharing,
        stats.pages_unshared,
        stats.general_profit,
        stats.full_scans,
        stats.pages_sharing as f64 * 4096.0 / 1024.0 / 1024.0,
        psi.some.avg10,
        psi.some.avg60,
        psi.full.avg10,
        psi.full.avg60,
    )
}

async fn restore_ksm(config_path: PathBuf) -> anyhow::Result<()> {
    let config = load_config_or_default(&config_path)?;

    init_tracing("info");
    restore_ksm_from_config(&config)?;
    info!("KSM state restored successfully");

    Ok(())
}

fn restore_ksm_from_config(config: &AnimaksmConfig) -> anyhow::Result<()> {
    let mut ksm = KsmController::new(&config.governor.ksm_path)?;
    let state_dir = PathBuf::from(&config.general.state_dir);
    ksm.restore(&state_dir)?;
    Ok(())
}

fn init_tracing(log_level: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level));

    // Try journald first, fall back to stderr
    match tracing_journald::layer() {
        Ok(_layer) => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .try_init();
            // Note: journald layer is created but we use stderr writer for simplicity
            // In production, use tracing_subscriber::registry().with(layer).init()
        }
        Err(_) => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .try_init();
        }
    }
}

/// Load config from path or return defaults (with info log if file missing).
fn load_config_or_default(config_path: &Path) -> anyhow::Result<AnimaksmConfig> {
    if config_path.exists() {
        Ok(AnimaksmConfig::load(config_path)?)
    } else {
        info!(
            path = %config_path.display(),
            "Config file not found, using defaults"
        );
        Ok(AnimaksmConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_ksm_dir(dir: &Path) {
        for (name, value) in [
            ("run", "1"),
            ("pages_to_scan", "500"),
            ("sleep_millisecs", "20"),
            ("max_page_sharing", "256"),
            ("smart_scan", "1"),
            ("advisor_mode", "none"),
            ("advisor_target_scan_time", "200"),
            ("advisor_max_cpu", "70"),
            ("advisor_min_pages_to_scan", "500"),
            ("advisor_max_pages_to_scan", "30000"),
            ("pages_shared", "10"),
            ("pages_sharing", "512"),
            ("pages_unshared", "3"),
            ("pages_volatile", "0"),
            ("pages_scanned", "1000"),
            ("pages_skipped", "2"),
            ("full_scans", "7"),
            ("general_profit", "12345"),
        ] {
            std::fs::write(dir.join(name), value).unwrap();
        }
    }

    fn write_config(path: &Path, ksm_path: &Path, state_dir: &Path) {
        std::fs::write(
            path,
            format!(
                "[general]\nstate_dir = \"{}\"\n\n[governor]\nksm_path = \"{}\"\n",
                state_dir.display(),
                ksm_path.display()
            ),
        )
        .unwrap();
    }

    fn write_daemon_config(path: &Path, ksm_path: &Path, state_dir: &Path) {
        std::fs::write(
            path,
            format!(
                "[general]\nstate_dir = \"{}\"\n\n\
                 [governor]\nenabled = false\nksm_path = \"{}\"\n\n\
                 [scanner]\nenabled = false\n",
                state_dir.display(),
                ksm_path.display()
            ),
        )
        .unwrap();
    }

    /// Config with governor and scanner enabled (for coverage of task-spawning paths).
    fn write_daemon_config_all_enabled(path: &Path, ksm_path: &Path, state_dir: &Path) {
        std::fs::write(
            path,
            format!(
                "[general]\nstate_dir = \"{}\"\n\n\
                 [governor]\nenabled = true\nksm_path = \"{}\"\n\n\
                 [scanner]\nenabled = true\ninterval_secs = 3600\nmin_anon_rss_mb = 999999\n",
                state_dir.display(),
                ksm_path.display()
            ),
        )
        .unwrap();
    }

    #[test]
    fn test_load_config_or_default_nonexistent_returns_default() {
        let path = PathBuf::from("/nonexistent/path/animaksm.toml");
        let config = load_config_or_default(&path).unwrap();
        assert_eq!(config.governor.stabilization_secs, 30);
        assert_eq!(config.general.log_level, "info");
    }

    #[test]
    fn test_load_config_or_default_reads_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("animaksm.toml");
        std::fs::write(
            &path,
            "[general]\nlog_level = \"debug\"\n\n[scanner]\nenabled = false\n",
        )
        .unwrap();

        let config = load_config_or_default(&path).unwrap();
        assert_eq!(config.general.log_level, "debug");
        assert!(!config.scanner.enabled);
    }

    #[test]
    fn test_format_status_contains_config_stats_and_psi() {
        let cfg = KsmConfig {
            run: 1,
            pages_to_scan: 2000,
            sleep_millisecs: 50,
            max_page_sharing: 384,
            smart_scan: 1,
            advisor_mode: "scan-time".into(),
            ..Default::default()
        };
        let stats = KsmStats {
            pages_shared: 10,
            pages_sharing: 512,
            pages_unshared: 3,
            general_profit: 12345,
            full_scans: 7,
            ..Default::default()
        };
        let psi = PsiStats {
            some: animaksm_common::psi::PsiLine {
                avg10: 1.25,
                avg60: 2.5,
                ..Default::default()
            },
            full: animaksm_common::psi::PsiLine {
                avg10: 0.5,
                avg60: 0.75,
                ..Default::default()
            },
        };

        let output = format_status(&cfg, &stats, &psi);
        assert!(output.contains("advisor_mode:           scan-time"));
        assert!(output.contains("pages_sharing:          512"));
        assert!(output.contains("sharing_savings_mb:     2.0"));
        assert!(output.contains("some avg10:             1.25%"));
        assert!(output.contains("full avg60:             0.75%"));
    }

    #[test]
    fn test_build_status_output_reads_configured_ksm_and_psi_paths() {
        let dir = tempfile::tempdir().unwrap();
        let ksm_dir = dir.path().join("ksm");
        let state_dir = dir.path().join("state");
        std::fs::create_dir(&ksm_dir).unwrap();
        seed_ksm_dir(&ksm_dir);

        let psi_path = dir.path().join("memory.pressure");
        std::fs::write(
            &psi_path,
            "some avg10=1.25 avg60=2.50 avg300=3.00 total=10\n\
             full avg10=0.50 avg60=0.75 avg300=1.00 total=5\n",
        )
        .unwrap();

        let config_path = dir.path().join("animaksm.toml");
        write_config(&config_path, &ksm_dir, &state_dir);

        let output = build_status_output(&config_path, &psi_path).unwrap();
        assert!(output.contains("pages_to_scan:          500"));
        assert!(output.contains("pages_sharing:          512"));
        assert!(output.contains("some avg60:             2.50%"));
        assert!(output.contains("full avg10:             0.50%"));
    }

    #[tokio::test]
    async fn test_show_status_reads_configured_ksm_when_proc_psi_exists() {
        if !Path::new("/proc/pressure/memory").exists() {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let ksm_dir = dir.path().join("ksm");
        let state_dir = dir.path().join("state");
        std::fs::create_dir(&ksm_dir).unwrap();
        seed_ksm_dir(&ksm_dir);

        let config_path = dir.path().join("animaksm.toml");
        write_config(&config_path, &ksm_dir, &state_dir);

        show_status(config_path).await.unwrap();
    }

    #[tokio::test]
    async fn test_restore_ksm_restores_snapshot_from_config() {
        let dir = tempfile::tempdir().unwrap();
        let ksm_dir = dir.path().join("ksm");
        let state_dir = dir.path().join("state");
        std::fs::create_dir(&ksm_dir).unwrap();
        seed_ksm_dir(&ksm_dir);

        let ctrl = KsmController::new(ksm_dir.to_str().unwrap()).unwrap();
        ctrl.snapshot(&state_dir).unwrap();
        std::fs::write(ksm_dir.join("run"), "0").unwrap();
        std::fs::write(ksm_dir.join("pages_to_scan"), "100").unwrap();

        let config_path = dir.path().join("animaksm.toml");
        write_config(&config_path, &ksm_dir, &state_dir);

        restore_ksm(config_path).await.unwrap();

        assert_eq!(std::fs::read_to_string(ksm_dir.join("run")).unwrap(), "1");
        assert_eq!(
            std::fs::read_to_string(ksm_dir.join("pages_to_scan")).unwrap(),
            "500"
        );
    }

    #[test]
    fn test_restore_ksm_from_config_errors_for_missing_ksm_path() {
        let mut config = AnimaksmConfig::default();
        config.governor.ksm_path = "/nonexistent/ksm/path".into();
        let err = restore_ksm_from_config(&config).unwrap_err();
        assert!(err.to_string().contains("KSM path not found"));
    }

    #[tokio::test]
    async fn test_run_daemon_with_injected_shutdown_dry_run_skips_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let ksm_dir = dir.path().join("ksm");
        let state_dir = dir.path().join("state");
        std::fs::create_dir(&ksm_dir).unwrap();
        seed_ksm_dir(&ksm_dir);

        let config_path = dir.path().join("animaksm.toml");
        write_daemon_config(&config_path, &ksm_dir, &state_dir);

        run_daemon_with_shutdown(config_path, true, async { Ok("test") })
            .await
            .unwrap();

        assert!(!state_dir.join("ksm-snapshot.json").exists());
    }

    #[tokio::test]
    async fn test_run_daemon_with_injected_shutdown_snapshots_and_restores() {
        let dir = tempfile::tempdir().unwrap();
        let ksm_dir = dir.path().join("ksm");
        let state_dir = dir.path().join("state");
        std::fs::create_dir(&ksm_dir).unwrap();
        seed_ksm_dir(&ksm_dir);

        let config_path = dir.path().join("animaksm.toml");
        write_daemon_config(&config_path, &ksm_dir, &state_dir);

        run_daemon_with_shutdown(config_path, false, async { Ok("SIGTERM") })
            .await
            .unwrap();

        assert!(state_dir.join("ksm-snapshot.json").exists());
        assert_eq!(std::fs::read_to_string(ksm_dir.join("run")).unwrap(), "1");
    }

    #[tokio::test]
    async fn test_run_daemon_with_governor_and_scanner_enabled_dry_run() {
        let dir = tempfile::tempdir().unwrap();
        let ksm_dir = dir.path().join("ksm");
        let state_dir = dir.path().join("state");
        std::fs::create_dir(&ksm_dir).unwrap();
        seed_ksm_dir(&ksm_dir);

        let config_path = dir.path().join("animaksm.toml");
        write_daemon_config_all_enabled(&config_path, &ksm_dir, &state_dir);

        run_daemon_with_shutdown(config_path.clone(), true, async { Ok("SIGINT") })
            .await
            .unwrap();

        // Dry-run: snapshot should NOT exist
        assert!(!state_dir.join("ksm-snapshot.json").exists());
    }

    #[tokio::test]
    async fn test_run_daemon_with_governor_and_scanner_enabled_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let ksm_dir = dir.path().join("ksm");
        let state_dir = dir.path().join("state");
        std::fs::create_dir(&ksm_dir).unwrap();
        seed_ksm_dir(&ksm_dir);

        let config_path = dir.path().join("animaksm.toml");
        write_daemon_config_all_enabled(&config_path, &ksm_dir, &state_dir);

        run_daemon_with_shutdown(config_path, false, async { Ok("SIGTERM") })
            .await
            .unwrap();

        // Non-dry-run: snapshot + restore should have happened
        assert!(state_dir.join("ksm-snapshot.json").exists());
        assert_eq!(std::fs::read_to_string(ksm_dir.join("run")).unwrap(), "1");
    }

    #[test]
    fn test_init_tracing_is_idempotent() {
        // init_tracing calls tracing_journald::layer() which fails in CI,
        // so this hits the Err branch for the second call (already initialized).
        // The first call may hit either branch depending on environment.
        init_tracing("debug");
        init_tracing("info");
    }

    #[test]
    fn test_cli_parses_run_status_and_restore_commands() {
        let run = Cli::try_parse_from([
            "animaksm",
            "run",
            "--config",
            "/tmp/animaksm.toml",
            "--dry-run",
        ])
        .unwrap();
        match run.command {
            Commands::Run { config, dry_run } => {
                assert_eq!(config, PathBuf::from("/tmp/animaksm.toml"));
                assert!(dry_run);
            }
            _ => panic!("expected run command"),
        }

        let status =
            Cli::try_parse_from(["animaksm", "status", "--config", "/tmp/animaksm.toml"]).unwrap();
        assert!(matches!(status.command, Commands::Status { .. }));

        let restore =
            Cli::try_parse_from(["animaksm", "restore-ksm", "--config", "/tmp/animaksm.toml"])
                .unwrap();
        assert!(matches!(restore.command, Commands::RestoreKsm { .. }));
    }
}

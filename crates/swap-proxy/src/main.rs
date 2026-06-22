//! zramdedup-swap-proxy: Experimental deduplicating swap proxy.
//!
//! Sits between the kernel swap subsystem and zram, deduplicating
//! identical pages before they reach compressed storage. Uses
//! xxh3-128 fingerprinting with a Bloom filter + concurrent hash map.
//!
//! NOTE: This is an experimental component. It requires the ublk_drv
//! kernel module (not available in all environments). The proxy is
//! designed to fail-open: if it crashes, zram0 remains as direct swap.

mod backend;
mod dedup;
mod fingerprint;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use parking_lot::Mutex;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use fingerprint::{fingerprint_page, PAGE_SIZE};

#[derive(Parser)]
#[command(
    name = "zramdedup-swap-proxy",
    about = "Experimental deduplicating swap proxy"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the swap proxy
    Run {
        /// Path to page store file (or block device)
        #[arg(long, default_value = "/var/lib/zramdedup/pagestore.dat")]
        store_path: PathBuf,

        /// Device size in GB
        #[arg(long, default_value_t = 8)]
        size_gb: u64,

        /// Maximum dedup table entries
        #[arg(long, default_value_t = 1_000_000)]
        max_entries: u64,

        /// Dry-run mode: simulate without actual block device
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Show proxy statistics
    Stats {
        /// Path to page store file
        #[arg(long, default_value = "/var/lib/zramdedup/pagestore.dat")]
        store_path: PathBuf,
    },
    /// Clean up after a proxy crash
    Cleanup,
}

/// Shared statistics for the proxy.
struct ProxyStats {
    total_writes: AtomicU64,
    unique_writes: AtomicU64,
    duplicate_writes: AtomicU64,
    total_reads: AtomicU64,
    discards: AtomicU64,
}

impl ProxyStats {
    fn new() -> Self {
        Self {
            total_writes: AtomicU64::new(0),
            unique_writes: AtomicU64::new(0),
            duplicate_writes: AtomicU64::new(0),
            total_reads: AtomicU64::new(0),
            discards: AtomicU64::new(0),
        }
    }
}

/// The swap proxy engine (without actual block device integration).
struct ProxyEngine {
    dedup_table: dedup::DedupTable,
    store: Mutex<backend::PageStore>,
    translation: Mutex<backend::TranslationTable>,
    stats: Arc<ProxyStats>,
    passthrough: AtomicBool,
}

impl ProxyEngine {
    fn new(store_path: &Path, size_gb: u64, max_entries: u64) -> anyhow::Result<Self> {
        let total_slots = (size_gb * 1024 * 1024 * 1024) / PAGE_SIZE as u64;
        let bloom_capacity = max_entries as usize;

        let store = if store_path.exists() {
            backend::PageStore::open(store_path, total_slots)?
        } else {
            // Create new store file
            std::fs::create_dir_all(store_path.parent().unwrap_or(Path::new(".")))?;
            backend::PageStore::create_file(store_path, total_slots)?
        };

        info!(
            store_path = %store_path.display(),
            total_slots,
            size_gb,
            max_entries,
            "Page store initialized"
        );

        Ok(Self {
            dedup_table: dedup::DedupTable::new(max_entries, bloom_capacity, total_slots),
            store: Mutex::new(store),
            translation: Mutex::new(backend::TranslationTable::new()),
            stats: Arc::new(ProxyStats::new()),
            // DEFAULT: passthrough ON — dedup is opportunistic only under low pressure.
            // This prevents latency variance from killing swap performance when
            // the system is already stressed.
            passthrough: AtomicBool::new(true),
        })
    }

    /// Process a page write: fingerprint, check dedup, store or deduplicate.
    fn handle_write(&self, virtual_offset: u64, data: &[u8]) -> anyhow::Result<()> {
        self.stats.total_writes.fetch_add(1, Ordering::Relaxed);

        // Passthrough mode (when under extreme pressure)
        if self.passthrough.load(Ordering::Relaxed) {
            let slot = virtual_offset / PAGE_SIZE as u64;
            let mut store = self.store.lock();
            store.write_page(slot, data)?;
            self.stats.unique_writes.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }

        let fp = fingerprint_page(data);

        // Check dedup table
        match self.dedup_table.lookup(&fp, data) {
            dedup::LookupResult::Duplicate { backend_offset } => {
                // Duplicate page! Just add a reference
                self.dedup_table.add_reference(&fp);
                self.stats.duplicate_writes.fetch_add(1, Ordering::Relaxed);

                // Update translation table
                let mut xlat = self.translation.lock();
                // Remove old mapping if any
                if let Some(old_entry) = xlat.remove(virtual_offset) {
                    self.dedup_table.remove_reference(&old_entry.fingerprint);
                }
                xlat.insert(virtual_offset, fp, backend_offset);
            }
            dedup::LookupResult::Miss => {
                // Unique page, store it
                match self.dedup_table.insert(fp, data) {
                    Some(slot) => {
                        let mut store = self.store.lock();
                        store.write_page(slot, data)?;

                        self.stats.unique_writes.fetch_add(1, Ordering::Relaxed);

                        let mut xlat = self.translation.lock();
                        if let Some(old_entry) = xlat.remove(virtual_offset) {
                            self.dedup_table.remove_reference(&old_entry.fingerprint);
                        }
                        xlat.insert(virtual_offset, fp, slot);
                    }
                    None => {
                        warn!("Dedup table full, writing directly");
                        let slot = virtual_offset / PAGE_SIZE as u64;
                        let mut store = self.store.lock();
                        store.write_page(slot, data)?;
                        self.stats.unique_writes.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }

        Ok(())
    }

    /// Process a page read.
    fn handle_read(&self, virtual_offset: u64) -> anyhow::Result<Vec<u8>> {
        self.stats.total_reads.fetch_add(1, Ordering::Relaxed);

        let xlat = self.translation.lock();
        let slot = if let Some(entry) = xlat.lookup(virtual_offset) {
            entry.backend_slot
        } else {
            // No translation — read directly (unwritten page = zeros)
            virtual_offset / PAGE_SIZE as u64
        };
        drop(xlat);

        let mut store = self.store.lock();
        store.read_page(slot)
    }

    /// Process a discard (trim) for a virtual offset.
    fn handle_discard(&self, virtual_offset: u64) {
        self.stats.discards.fetch_add(1, Ordering::Relaxed);

        let mut xlat = self.translation.lock();
        if let Some(entry) = xlat.remove(virtual_offset) {
            self.dedup_table.remove_reference(&entry.fingerprint);
        }
    }

    /// Get current proxy statistics.
    fn print_stats(&self) {
        let total_w = self.stats.total_writes.load(Ordering::Relaxed);
        let unique_w = self.stats.unique_writes.load(Ordering::Relaxed);
        let dup_w = self.stats.duplicate_writes.load(Ordering::Relaxed);
        let total_r = self.stats.total_reads.load(Ordering::Relaxed);
        let discards = self.stats.discards.load(Ordering::Relaxed);

        let dedup_table_stats = self.dedup_table.stats();

        let dedup_ratio = if total_w > 0 {
            (dup_w as f64 / total_w as f64) * 100.0
        } else {
            0.0
        };

        println!("=== zramdedup swap proxy statistics ===");
        println!();
        println!("Write Operations:");
        println!("  Total writes:         {total_w}");
        println!("  Unique writes:        {unique_w}");
        println!("  Duplicate (deduped):  {dup_w}");
        println!("  Dedup ratio:          {dedup_ratio:.1}%");
        println!();
        println!("Read Operations:");
        println!("  Total reads:          {total_r}");
        println!();
        println!("Discards:               {discards}");
        println!();
        println!("Dedup Table:");
        println!(
            "  Unique pages tracked: {}",
            dedup_table_stats.table_entries
        );
        println!("  Total unique seen:    {}", dedup_table_stats.unique_pages);
        println!(
            "  Duplicate hits:       {}",
            dedup_table_stats.duplicate_hits
        );
        println!("  Evictions:            {}", dedup_table_stats.evictions);
        println!(
            "  Bloom false positives: {}",
            dedup_table_stats.bloom_false_positives
        );
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run {
            store_path,
            size_gb,
            max_entries,
            dry_run,
        } => run_proxy(store_path, size_gb, max_entries, dry_run),
        Commands::Stats { store_path } => {
            // Open store and show stats
            println!("Note: Stats require the proxy to be running.");
            println!("Store path: {}", store_path.display());
            Ok(())
        }
        Commands::Cleanup => {
            info!("Cleaning up after proxy crash");
            // Ensure zram0 is still active as swap
            let output = std::process::Command::new("swapon").arg("--show").output();
            match output {
                Ok(o) => {
                    let stdout = String::from_utf8_lossy(&o.stdout);
                    if stdout.contains("zram0") {
                        println!("zram0 is still active as swap. No cleanup needed.");
                    } else {
                        println!("WARNING: zram0 is not active as swap!");
                        println!("Run: swapon /dev/zram0");
                    }
                }
                Err(e) => {
                    println!("Could not check swap status: {e}");
                }
            }
            Ok(())
        }
    }
}

fn run_proxy(
    store_path: PathBuf,
    size_gb: u64,
    max_entries: u64,
    dry_run: bool,
) -> anyhow::Result<()> {
    // Initialize tracing
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();

    info!(
        version = env!("CARGO_PKG_VERSION"),
        store = %store_path.display(),
        size_gb,
        max_entries,
        dry_run,
        "zramdedup-swap-proxy starting"
    );

    if dry_run {
        info!("DRY RUN mode: simulating proxy without actual block device");
        run_dry_run_simulation(store_path, size_gb, max_entries)?;
    } else {
        // Real mode: would set up ublk device here
        // For now, demonstrate the engine works with synthetic data
        info!("Real mode requires ublk_drv kernel module");
        info!("Use --dry-run to test the dedup engine with synthetic data");

        let engine = ProxyEngine::new(&store_path, size_gb, max_entries)?;

        // Demonstrate with a few synthetic pages
        info!("Running synthetic dedup demonstration...");

        // Write some duplicate pages
        let page_a = vec![0xAAu8; PAGE_SIZE];
        let page_b = vec![0xBBu8; PAGE_SIZE];

        for i in 0..100u64 {
            let data = if i % 3 == 0 {
                &page_a
            } else if i % 3 == 1 {
                &page_b
            } else {
                &page_a
            };
            let offset = i * PAGE_SIZE as u64;
            engine.handle_write(offset, data)?;
        }

        engine.print_stats();

        info!("Demonstration complete. In production, ublk device would be registered here.");
    }

    Ok(())
}

fn run_dry_run_simulation(
    store_path: PathBuf,
    size_gb: u64,
    max_entries: u64,
) -> anyhow::Result<()> {
    let engine = ProxyEngine::new(&store_path, size_gb, max_entries)?;

    info!("Generating synthetic workload (opportunistic mode demo)...");
    info!("Phase 1: Passthrough mode (simulating high memory pressure)");

    // Phase 1: Start in passthrough mode (high pressure)
    // Pages go straight through without dedup overhead
    let num_unique = 600u64;
    let num_total = 5_000u64;

    let mut unique_pages: Vec<Vec<u8>> = Vec::new();
    for i in 0..num_unique {
        let mut page = vec![0u8; PAGE_SIZE];
        for (j, byte) in page.iter_mut().enumerate() {
            *byte = ((i * 7 + j as u64 * 13) % 256) as u8;
        }
        unique_pages.push(page);
    }

    // Write pages in passthrough (no dedup)
    for i in 0..num_total {
        let page_idx = (i * 37 % num_unique) as usize;
        let offset = i * PAGE_SIZE as u64;
        engine.handle_write(offset, &unique_pages[page_idx])?;
    }

    info!(
        total = engine
            .stats
            .total_writes
            .load(std::sync::atomic::Ordering::Relaxed),
        deduped = engine
            .stats
            .duplicate_writes
            .load(std::sync::atomic::Ordering::Relaxed),
        "Phase 1 complete (passthrough — zero dedup overhead)"
    );

    // Phase 2: Pressure drops — enable opportunistic dedup
    info!("Phase 2: Enabling dedup mode (pressure dropped, opportunistic)");
    engine.passthrough.store(false, Ordering::Relaxed);

    for i in num_total..(num_total * 2) {
        let page_idx = (i * 37 % num_unique) as usize;
        let offset = i * PAGE_SIZE as u64;
        engine.handle_write(offset, &unique_pages[page_idx])?;
    }

    // Read back some pages
    for i in 0..1000u64 {
        let offset = i * PAGE_SIZE as u64;
        let data = engine.handle_read(offset)?;
        assert_eq!(data.len(), PAGE_SIZE);
    }

    // Discard some
    for i in 5000..6000u64 {
        let offset = i * PAGE_SIZE as u64;
        engine.handle_discard(offset);
    }

    engine.print_stats();

    info!("Dry-run simulation complete (opportunistic mode demonstrated)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("pagestore.dat");
        (dir, path)
    }

    #[test]
    fn test_proxy_stats_new_zeroes() {
        let stats = ProxyStats::new();
        assert_eq!(stats.total_writes.load(Ordering::Relaxed), 0);
        assert_eq!(stats.unique_writes.load(Ordering::Relaxed), 0);
        assert_eq!(stats.duplicate_writes.load(Ordering::Relaxed), 0);
        assert_eq!(stats.total_reads.load(Ordering::Relaxed), 0);
        assert_eq!(stats.discards.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_proxy_engine_new_creates_store_parent_directory() {
        let (_dir, path) = engine_path();

        let engine = ProxyEngine::new(&path, 1, 128).unwrap();

        assert!(path.exists());
        assert!(engine.passthrough.load(Ordering::Relaxed));
        assert_eq!(
            engine.store.lock().total_slots(),
            (1024 * 1024 * 1024) / PAGE_SIZE as u64
        );
    }

    #[test]
    fn test_cli_parses_run_stats_and_cleanup_commands() {
        let run = Cli::try_parse_from([
            "zramdedup-swap-proxy",
            "run",
            "--store-path",
            "/tmp/store.dat",
            "--size-gb",
            "2",
            "--max-entries",
            "42",
            "--dry-run",
        ])
        .unwrap();
        match run.command {
            Commands::Run {
                store_path,
                size_gb,
                max_entries,
                dry_run,
            } => {
                assert_eq!(store_path, PathBuf::from("/tmp/store.dat"));
                assert_eq!(size_gb, 2);
                assert_eq!(max_entries, 42);
                assert!(dry_run);
            }
            _ => panic!("expected run command"),
        }

        let stats = Cli::try_parse_from([
            "zramdedup-swap-proxy",
            "stats",
            "--store-path",
            "/tmp/store.dat",
        ])
        .unwrap();
        assert!(matches!(stats.command, Commands::Stats { .. }));

        let cleanup = Cli::try_parse_from(["zramdedup-swap-proxy", "cleanup"]).unwrap();
        assert!(matches!(cleanup.command, Commands::Cleanup));
    }

    #[test]
    fn test_handle_write_passthrough_writes_direct_slot_and_counts_unique() {
        let (_dir, path) = engine_path();
        let engine = ProxyEngine::new(&path, 1, 128).unwrap();
        let page = vec![0xAA; PAGE_SIZE];

        engine.handle_write(PAGE_SIZE as u64, &page).unwrap();
        let read = engine.handle_read(PAGE_SIZE as u64).unwrap();

        assert_eq!(read, page);
        assert_eq!(engine.stats.total_writes.load(Ordering::Relaxed), 1);
        assert_eq!(engine.stats.unique_writes.load(Ordering::Relaxed), 1);
        assert_eq!(engine.stats.duplicate_writes.load(Ordering::Relaxed), 0);
        assert_eq!(engine.stats.total_reads.load(Ordering::Relaxed), 1);
        assert_eq!(engine.translation.lock().len(), 0);
    }

    #[test]
    fn test_handle_write_dedup_mode_records_duplicate_translation() {
        let (_dir, path) = engine_path();
        let engine = ProxyEngine::new(&path, 1, 128).unwrap();
        engine.passthrough.store(false, Ordering::Relaxed);
        let page = vec![0xAB; PAGE_SIZE];

        engine.handle_write(0, &page).unwrap();
        engine.handle_write(PAGE_SIZE as u64, &page).unwrap();

        assert_eq!(engine.stats.total_writes.load(Ordering::Relaxed), 2);
        assert_eq!(engine.stats.unique_writes.load(Ordering::Relaxed), 1);
        assert_eq!(engine.stats.duplicate_writes.load(Ordering::Relaxed), 1);
        assert_eq!(engine.translation.lock().len(), 2);

        let read = engine.handle_read(PAGE_SIZE as u64).unwrap();
        assert_eq!(read, page);

        let dedup_stats = engine.dedup_table.stats();
        assert_eq!(dedup_stats.table_entries, 1);
        assert_eq!(dedup_stats.duplicate_hits, 1);
    }

    #[test]
    fn test_handle_write_replaces_old_translation_and_removes_reference() {
        let (_dir, path) = engine_path();
        let engine = ProxyEngine::new(&path, 1, 128).unwrap();
        engine.passthrough.store(false, Ordering::Relaxed);
        let page_a = vec![0xAA; PAGE_SIZE];
        let page_b = vec![0xBB; PAGE_SIZE];

        engine.handle_write(0, &page_a).unwrap();
        engine.handle_write(0, &page_b).unwrap();

        let read = engine.handle_read(0).unwrap();
        assert_eq!(read, page_b);
        assert_eq!(engine.translation.lock().len(), 1);
        assert_eq!(engine.dedup_table.stats().table_entries, 1);
    }

    #[test]
    fn test_handle_discard_removes_translation_and_counts_discard() {
        let (_dir, path) = engine_path();
        let engine = ProxyEngine::new(&path, 1, 128).unwrap();
        engine.passthrough.store(false, Ordering::Relaxed);
        let page = vec![0xCC; PAGE_SIZE];

        engine.handle_write(0, &page).unwrap();
        assert_eq!(engine.translation.lock().len(), 1);

        engine.handle_discard(0);

        assert_eq!(engine.stats.discards.load(Ordering::Relaxed), 1);
        assert_eq!(engine.translation.lock().len(), 0);
        assert_eq!(engine.dedup_table.stats().table_entries, 0);
    }

    #[test]
    fn test_run_proxy_dry_run_simulation_completes() {
        let (_dir, path) = engine_path();
        run_proxy(path, 1, 2048, true).unwrap();
    }
}

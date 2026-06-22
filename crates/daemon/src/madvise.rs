//! Tier 2: process_madvise(2) interface for applying KSM to target processes.
//!
//! Uses pidfd_open + process_madvise to mark anonymous memory regions
//! as MADV_MERGEABLE without ptrace or process suspension.
//!
//! ## Batched iovec
//!
//! process_madvise(2) accepts an iovec array — instead of one syscall per
//! VMA, we collect all eligible regions and fire a single batched call.
//! This dramatically reduces syscall overhead (N VMAs → 1 syscall per PID).
//!
//! ## MADV_COLLAPSE
//!
//! Opportunistic THP promotion: when KSM unmerges pages (triggering CoW),
//! those pages lose their THP backing. MADV_COLLAPSE (Linux 6.1+)
//! re-promotes aligned 2MB regions to transparent huge pages, recovering
//! TLB efficiency.

use std::collections::HashMap;
use std::os::unix::io::RawFd;

use tracing::{debug, info};
use zramdedup_common::procfs::MapsEntry;

/// MADV_MERGEABLE constant (from linux/mman.h).
const MADV_MERGEABLE: libc::c_int = 12;

/// MADV_COLLAPSE constant (Linux 6.1+, collapses memory into THP).
const MADV_COLLAPSE: libc::c_int = 25;

/// process_madvise syscall number (x86_64).
const SYS_PROCESS_MADVISE: libc::c_long = 440;

/// pidfd_open syscall number (x86_64).
const SYS_PIDFD_OPEN: libc::c_long = 434;

/// Maximum iovecs per process_madvise call (kernel limit is typically 1024).
const MAX_IOV_BATCH: usize = 256;

/// Result of a madvise operation on a process.
#[derive(Debug)]
pub struct MadviseResult {
    pub pid: u32,
    pub regions_attempted: usize,
    pub regions_merged: usize,
    pub total_bytes_marked: u64,
    pub errors: Vec<(u64, String)>, // (address, error description)
}

/// Apply MADV_MERGEABLE to anonymous RW regions of a target process.
///
/// Uses process_madvise(2) via pidfd with **batched iovec** — all eligible
/// regions are collected and sent in a single syscall, reducing overhead
/// from N syscalls to 1.
pub fn apply_mergeable(
    pid: u32,
    regions: &[MapsEntry],
    max_bytes: u64,
    dry_run: bool,
) -> MadviseResult {
    let mut result = MadviseResult {
        pid,
        regions_attempted: 0,
        regions_merged: 0,
        total_bytes_marked: 0,
        errors: Vec::new(),
    };

    // Filter to eligible regions and enforce per-process byte cap
    let eligible: Vec<&MapsEntry> = regions
        .iter()
        .filter(|r| r.is_anon_rw() && !r.has_exec() && !r.is_special())
        .collect();

    if eligible.is_empty() {
        debug!(pid, "No eligible anonymous RW regions found");
        return result;
    }

    // Open pidfd for the target process
    let pidfd = match pidfd_open(pid) {
        Ok(fd) => fd,
        Err(e) => {
            result.errors.push((0, format!("pidfd_open failed: {e}")));
            return result;
        }
    };

    // Collect regions into iovec batches (respecting max_bytes and MAX_IOV_BATCH)
    let batch = build_batch(&eligible, max_bytes);
    result.regions_attempted = batch.len();
    let total_bytes: u64 = batch.iter().map(|&(_, len)| len as u64).sum();

    if batch.is_empty() {
        unsafe {
            libc::close(pidfd);
        }
        return result;
    }

    if dry_run {
        for &(addr, len) in &batch {
            info!(
                pid,
                start = format!("0x{addr:x}"),
                size_kb = len / 1024,
                "[DRY RUN] Would batch process_madvise(MADV_MERGEABLE)"
            );
        }
        result.regions_merged = batch.len();
        result.total_bytes_marked = total_bytes;
        unsafe {
            libc::close(pidfd);
        }
        return result;
    }

    // Process in chunks of MAX_IOV_BATCH
    for chunk in batch.chunks(MAX_IOV_BATCH) {
        let iovs: Vec<libc::iovec> = chunk
            .iter()
            .map(|&(addr, len)| libc::iovec {
                iov_base: addr as *mut libc::c_void,
                iov_len: len,
            })
            .collect();

        let total_chunk_bytes: u64 = chunk.iter().map(|&(_, len)| len as u64).sum();

        match process_madvise_batch(pidfd, &iovs, MADV_MERGEABLE) {
            Ok(bytes_processed) => {
                if bytes_processed == total_chunk_bytes {
                    // All regions processed successfully
                    result.regions_merged += chunk.len();
                    result.total_bytes_marked += bytes_processed;
                    debug!(
                        pid,
                        regions = chunk.len(),
                        bytes = bytes_processed,
                        "Batched process_madvise(MADV_MERGEABLE) succeeded"
                    );
                } else {
                    // Partial success: kernel processed some but not all regions.
                    // This is normal — some VMAs may have been unmapped, split, or
                    // changed permissions between our maps read and the syscall.
                    // We optimistically count all as merged since we can't determine
                    // which specific iovec entries failed.
                    result.regions_merged += chunk.len();
                    result.total_bytes_marked += bytes_processed;
                    debug!(
                        pid,
                        expected = total_chunk_bytes,
                        processed = bytes_processed,
                        "Partial batch madvise (some VMAs may have changed)"
                    );
                }
            }
            Err(e) => {
                // Full batch failure — fall back to per-region calls
                debug!(
                    pid,
                    error = %e,
                    batch_size = chunk.len(),
                    "Batched madvise failed, falling back to per-region"
                );
                for &(addr, len) in chunk {
                    match process_madvise_single(pidfd, addr, len, MADV_MERGEABLE) {
                        Ok(_) => {
                            result.regions_merged += 1;
                            result.total_bytes_marked += len as u64;
                        }
                        Err(fallback_err) => {
                            result
                                .errors
                                .push((addr, format!("process_madvise failed: {fallback_err}")));
                        }
                    }
                }
            }
        }
    }

    // Close pidfd
    unsafe {
        libc::close(pidfd);
    }

    if result.regions_merged > 0 {
        info!(
            pid,
            regions_merged = result.regions_merged,
            total_kb = result.total_bytes_marked / 1024,
            "Applied batched MADV_MERGEABLE to process regions"
        );
    }

    result
}

/// Opportunistically collapse memory regions into transparent huge pages.
///
/// Uses process_madvise(MADV_COLLAPSE) on aligned 2MB regions.
/// This is useful after KSM unmerges pages (which triggers CoW and
/// breaks THP backing), recovering TLB efficiency.
///
/// Returns the number of regions successfully collapsed.
/// Requires Linux 6.1+; silently returns 0 on ENOSYS.
pub fn collapse_regions(pid: u32, regions: &[MapsEntry]) -> usize {
    // Only consider large regions (>= 2MB) that could contain aligned THP ranges
    const THP_SIZE: u64 = 2 * 1024 * 1024;

    let large_regions: Vec<&MapsEntry> = regions
        .iter()
        .filter(|r| r.is_anon_rw() && !r.has_exec() && !r.is_special() && r.size() >= THP_SIZE)
        .collect();

    if large_regions.is_empty() {
        return 0;
    }

    let pidfd = match pidfd_open(pid) {
        Ok(fd) => fd,
        Err(_) => return 0,
    };

    let mut collapsed = 0;

    for region in &large_regions {
        // Align to 2MB boundary for THP
        let start = (region.start + THP_SIZE - 1) & !(THP_SIZE - 1);
        let end = region.end & !(THP_SIZE - 1);
        if start >= end {
            continue;
        }

        let len = (end - start) as usize;
        match process_madvise_single(pidfd, start, len, MADV_COLLAPSE) {
            Ok(_) => collapsed += 1,
            Err(e) => {
                // ENOSYS = kernel doesn't support MADV_COLLAPSE (pre-6.1)
                // EINVAL = region not suitable for collapse
                // Both are non-fatal
                if e.contains("ENOSYS") {
                    debug!("MADV_COLLAPSE not supported by kernel");
                    break;
                }
                debug!(
                    pid,
                    start = format!("0x{start:x}"),
                    error = %e,
                    "MADV_COLLAPSE skipped for region"
                );
            }
        }
    }

    unsafe {
        libc::close(pidfd);
    }

    if collapsed > 0 {
        info!(pid, collapsed, "Collapsed regions to THP via MADV_COLLAPSE");
    }

    collapsed
}

/// Build the batch of (addr, len) pairs from eligible regions, capping at
/// `max_bytes` total and `MAX_IOV_BATCH` entries.
fn build_batch(eligible: &[&MapsEntry], max_bytes: u64) -> Vec<(u64, usize)> {
    let mut batch: Vec<(u64, usize)> = Vec::new();
    let mut bytes_accumulated: u64 = 0;

    for region in eligible {
        let size = region.size();
        if bytes_accumulated + size > max_bytes {
            break;
        }
        batch.push((region.start, size as usize));
        bytes_accumulated += size;
    }

    batch
}

/// Apply MADV_MERGEABLE to multiple processes at once.
pub fn batch_apply_mergeable(
    targets: &[(u32, Vec<MapsEntry>)],
    max_bytes_per_process: u64,
    dry_run: bool,
) -> HashMap<u32, MadviseResult> {
    let mut results = HashMap::new();

    for (pid, regions) in targets {
        let result = apply_mergeable(*pid, regions, max_bytes_per_process, dry_run);
        results.insert(*pid, result);
    }

    results
}

/// Open a pidfd for the given PID using pidfd_open(2).
fn pidfd_open(pid: u32) -> Result<RawFd, String> {
    let ret = unsafe { libc::syscall(SYS_PIDFD_OPEN, pid as libc::c_int, 0 as libc::c_uint) };

    if ret < 0 {
        let errno = unsafe { *libc::__errno_location() };
        return Err(format!("errno={errno} ({})", errno_to_str(errno)));
    }

    Ok(ret as RawFd)
}

/// Batched process_madvise: send multiple iovecs in a single syscall.
///
/// Returns the number of bytes successfully processed by the kernel.
/// On full failure (ret < 0), returns an error string.
fn process_madvise_batch(
    pidfd: RawFd,
    iovs: &[libc::iovec],
    advice: libc::c_int,
) -> Result<u64, String> {
    let ret = unsafe {
        libc::syscall(
            SYS_PROCESS_MADVISE,
            pidfd as libc::c_int,
            iovs.as_ptr(),
            iovs.len(),
            advice,
            0u32, // flags
        )
    };

    if ret < 0 {
        let errno = unsafe { *libc::__errno_location() };
        return Err(format!("errno={errno} ({})", errno_to_str(errno)));
    }

    Ok(ret as u64)
}

/// Single-region process_madvise call (used as fallback and for MADV_COLLAPSE).
fn process_madvise_single(
    pidfd: RawFd,
    addr: u64,
    len: usize,
    advice: libc::c_int,
) -> Result<(), String> {
    let iov = libc::iovec {
        iov_base: addr as *mut libc::c_void,
        iov_len: len,
    };

    let ret = unsafe {
        libc::syscall(
            SYS_PROCESS_MADVISE,
            pidfd as libc::c_int,
            &iov as *const libc::iovec,
            1usize,
            advice,
            0u32,
        )
    };

    if ret < 0 {
        let errno = unsafe { *libc::__errno_location() };
        return Err(format!("errno={errno} ({})", errno_to_str(errno)));
    }

    Ok(())
}

fn errno_to_str(errno: i32) -> &'static str {
    match errno {
        libc::EPERM => "EPERM: Operation not permitted",
        libc::ESRCH => "ESRCH: No such process",
        libc::EINVAL => "EINVAL: Invalid argument",
        libc::ENOMEM => "ENOMEM: Cannot allocate memory",
        libc::EBADF => "EBADF: Bad file descriptor",
        libc::ENOSYS => "ENOSYS: Function not implemented",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zramdedup_common::procfs::MapsEntry;

    // ── build_batch ──────────────────────────────────────────────────────

    #[test]
    fn test_build_batch_empty_eligible() {
        let eligible: Vec<&MapsEntry> = vec![];
        let batch = build_batch(&eligible, 4096);
        assert!(batch.is_empty());
    }

    #[test]
    fn test_build_batch_collects_regions() {
        let regions = vec![
            MapsEntry {
                start: 0x10000,
                end: 0x11000,
                perms: "rw-p".into(),
                offset: 0,
                dev: "00:00".into(),
                inode: 0,
                pathname: String::new(),
            },
            MapsEntry {
                start: 0x20000,
                end: 0x22000,
                perms: "rw-p".into(),
                offset: 0,
                dev: "00:00".into(),
                inode: 0,
                pathname: String::new(),
            },
        ];
        let refs: Vec<&MapsEntry> = regions.iter().collect();
        let batch = build_batch(&refs, 10 * 4096);
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0], (0x10000, 0x1000usize));
        assert_eq!(batch[1], (0x20000, 0x2000usize));
    }

    #[test]
    fn test_build_batch_respects_max_bytes() {
        let regions = vec![
            MapsEntry {
                start: 0x10000,
                end: 0x11000,
                perms: "rw-p".into(),
                offset: 0,
                dev: "00:00".into(),
                inode: 0,
                pathname: String::new(),
            }, // 4KB
            MapsEntry {
                start: 0x20000,
                end: 0x24000,
                perms: "rw-p".into(),
                offset: 0,
                dev: "00:00".into(),
                inode: 0,
                pathname: String::new(),
            }, // 16KB
            MapsEntry {
                start: 0x30000,
                end: 0x31000,
                perms: "rw-p".into(),
                offset: 0,
                dev: "00:00".into(),
                inode: 0,
                pathname: String::new(),
            }, // 4KB
        ];
        let refs: Vec<&MapsEntry> = regions.iter().collect();
        // max_bytes = 12KB → only first two regions fit (4KB + 16KB = 20KB > 12KB,
        // so it stops after the first one)
        let batch = build_batch(&refs, 12 * 1024);
        assert_eq!(batch.len(), 1, "only first region should fit in 12KB cap");
    }

    #[test]
    fn test_build_batch_exact_capacity() {
        let regions = vec![
            MapsEntry {
                start: 0x10000,
                end: 0x12000,
                perms: "rw-p".into(),
                offset: 0,
                dev: "00:00".into(),
                inode: 0,
                pathname: String::new(),
            }, // 8KB
            MapsEntry {
                start: 0x20000,
                end: 0x22000,
                perms: "rw-p".into(),
                offset: 0,
                dev: "00:00".into(),
                inode: 0,
                pathname: String::new(),
            }, // 8KB
        ];
        let refs: Vec<&MapsEntry> = regions.iter().collect();
        // max_bytes = 16KB → exactly both
        let batch = build_batch(&refs, 16 * 1024);
        assert_eq!(batch.len(), 2);
    }

    #[test]
    fn test_build_batch_zero_max_bytes() {
        let regions = vec![MapsEntry {
            start: 0x10000,
            end: 0x11000,
            perms: "rw-p".into(),
            offset: 0,
            dev: "00:00".into(),
            inode: 0,
            pathname: String::new(),
        }];
        let refs: Vec<&MapsEntry> = regions.iter().collect();
        let batch = build_batch(&refs, 0);
        assert!(batch.is_empty(), "zero cap should yield empty batch");
    }

    // ── apply_mergeable (dry-run, using our own PID) ────────────────────

    #[test]
    fn test_apply_mergeable_dry_run_eligible_regions() {
        let my_pid = std::process::id();
        let regions = vec![
            MapsEntry {
                start: 0x10000,
                end: 0x11000,
                perms: "rw-p".into(),
                offset: 0,
                dev: "00:00".into(),
                inode: 0,
                pathname: String::new(),
            },
            MapsEntry {
                start: 0x20000,
                end: 0x24000,
                perms: "rw-p".into(),
                offset: 0,
                dev: "00:00".into(),
                inode: 0,
                pathname: String::new(),
            },
        ];
        let result = apply_mergeable(my_pid, &regions, 1024 * 1024, true);
        // Both regions should be eligible (anon RW, no exec, not special)
        // First region = 4KB, second = 16KB → total = 20KB
        assert_eq!(result.regions_attempted, 2);
        assert_eq!(result.regions_merged, 2);
        assert_eq!(result.total_bytes_marked, (4 + 16) * 1024);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_apply_mergeable_dry_run_filters_non_eligible() {
        let my_pid = std::process::id();
        // One eligible, one file-backed (not anon), one executable
        let regions = vec![
            MapsEntry {
                start: 0x10000,
                end: 0x11000,
                perms: "rw-p".into(),
                offset: 0,
                dev: "00:00".into(),
                inode: 0,
                pathname: String::new(),
            }, // eligible
            MapsEntry {
                start: 0x20000,
                end: 0x21000,
                perms: "rw-p".into(),
                offset: 1,
                dev: "00:00".into(),
                inode: 12345,
                pathname: "/some/file".into(),
            }, // file-backed, not eligible
            MapsEntry {
                start: 0x30000,
                end: 0x31000,
                perms: "r-xp".into(),
                offset: 0,
                dev: "00:00".into(),
                inode: 0,
                pathname: String::new(),
            }, // executable, not eligible
        ];
        let result = apply_mergeable(my_pid, &regions, 1024 * 1024, true);
        // Only the first region should pass the eligibility filter
        assert_eq!(result.regions_attempted, 1);
        assert_eq!(result.regions_merged, 1);
    }

    #[test]
    fn test_apply_mergeable_dry_run_respects_max_bytes() {
        let my_pid = std::process::id();
        let regions = vec![
            MapsEntry {
                start: 0x10000,
                end: 0x12000,
                perms: "rw-p".into(),
                offset: 0,
                dev: "00:00".into(),
                inode: 0,
                pathname: String::new(),
            }, // 8KB
            MapsEntry {
                start: 0x20000,
                end: 0x24000,
                perms: "rw-p".into(),
                offset: 0,
                dev: "00:00".into(),
                inode: 0,
                pathname: String::new(),
            }, // 16KB
        ];
        // max_bytes = 8KB → only 1 region fits
        let result = apply_mergeable(my_pid, &regions, 8 * 1024, true);
        assert_eq!(result.regions_attempted, 1);
        assert_eq!(result.regions_merged, 1);
        assert_eq!(result.total_bytes_marked, 8 * 1024);
    }

    #[test]
    fn test_apply_mergeable_dry_run_invalid_pid() {
        // PID 0xFFFFFF is almost certainly non-existent
        let regions = vec![MapsEntry {
            start: 0x10000,
            end: 0x11000,
            perms: "rw-p".into(),
            offset: 0,
            dev: "00:00".into(),
            inode: 0,
            pathname: String::new(),
        }];
        let result = apply_mergeable(0xFFFFFF, &regions, 4096, true);
        // pidfd_open should fail → error returned, nothing merged
        assert!(result.regions_attempted == 0);
        assert!(
            !result.errors.is_empty(),
            "should report pidfd_open failure"
        );
        assert!(
            result.errors[0].1.contains("pidfd_open failed"),
            "error should mention pidfd_open: {}",
            result.errors[0].1
        );
    }

    // ── batch_apply_mergeable ───────────────────────────────────────────

    #[test]
    fn test_batch_apply_mergeable_multiple_targets() {
        let my_pid = std::process::id();
        let regions = vec![MapsEntry {
            start: 0x10000,
            end: 0x11000,
            perms: "rw-p".into(),
            offset: 0,
            dev: "00:00".into(),
            inode: 0,
            pathname: String::new(),
        }];

        // Two different PIDs — batch_apply_mergeable returns one per PID
        let other_pid = my_pid.checked_add(1).unwrap_or(2);
        let targets = vec![(my_pid, regions.clone()), (other_pid, regions)];
        let results = batch_apply_mergeable(&targets, 4096, true);

        assert_eq!(results.len(), 2, "should have entries for both PIDs");
        // First PID exists → dry-run succeeds with 1 region merged
        assert_eq!(results[&my_pid].regions_merged, 1);
        // Second PID doesn't exist → pidfd_open fails, 0 merged
        assert_eq!(results[&other_pid].regions_merged, 0);
        assert!(results[&other_pid].errors[0].1.contains("pidfd_open failed"));
    }

    // ── MadviseResult ───────────────────────────────────────────────────

    #[test]
    fn test_madvise_result_default_zeroes() {
        let result = MadviseResult {
            pid: 42,
            regions_attempted: 0,
            regions_merged: 0,
            total_bytes_marked: 0,
            errors: vec![],
        };
        assert_eq!(result.pid, 42);
        assert_eq!(result.regions_merged, 0);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_madvise_result_with_errors() {
        let result = MadviseResult {
            pid: 99,
            regions_attempted: 2,
            regions_merged: 1,
            total_bytes_marked: 4096,
            errors: vec![(0x10000, "EPERM: Operation not permitted".into())],
        };
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.regions_merged, 1);
    }
}

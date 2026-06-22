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
    let mut batch: Vec<(u64, usize)> = Vec::new(); // (start_addr, length)
    let mut bytes_accumulated: u64 = 0;

    for region in &eligible {
        let size = region.size();
        if bytes_accumulated + size > max_bytes {
            break;
        }
        batch.push((region.start, size as usize));
        bytes_accumulated += size;
        result.regions_attempted += 1;
    }

    if batch.is_empty() {
        unsafe { libc::close(pidfd); }
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
        result.total_bytes_marked = bytes_accumulated;
        unsafe { libc::close(pidfd); }
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
                            result.errors.push((
                                addr,
                                format!("process_madvise failed: {fallback_err}"),
                            ));
                        }
                    }
                }
            }
        }
    }

    // Close pidfd
    unsafe { libc::close(pidfd); }

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

    unsafe { libc::close(pidfd); }

    if collapsed > 0 {
        info!(pid, collapsed, "Collapsed regions to THP via MADV_COLLAPSE");
    }

    collapsed
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

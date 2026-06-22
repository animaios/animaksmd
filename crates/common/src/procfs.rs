//! Procfs utilities for process discovery and memory analysis.
//!
//! Provides functions to enumerate processes, read their memory maps,
//! and extract KSM-relevant statistics from /proc.

use crate::error::{Result, ZramdedupError};
use std::fs;

/// Process memory status from /proc/PID/status.
#[derive(Debug, Clone, Default)]
pub struct ProcessStatus {
    pub pid: u32,
    pub name: String,
    pub vm_rss_kb: u64,
    pub vm_anon_kb: u64,
    pub vm_swap_kb: u64,
}

/// A single entry from /proc/PID/maps.
#[derive(Debug, Clone)]
pub struct MapsEntry {
    pub start: u64,
    pub end: u64,
    pub perms: String,
    pub offset: u64,
    pub dev: String,
    pub inode: u64,
    pub pathname: String,
}

impl MapsEntry {
    /// Size of this mapping in bytes.
    pub fn size(&self) -> u64 {
        self.end - self.start
    }

    /// Number of pages in this mapping.
    pub fn page_count(&self) -> u64 {
        self.size() / 4096
    }

    /// Whether this is an anonymous read-write private mapping (KSM candidate).
    pub fn is_anon_rw(&self) -> bool {
        self.perms.contains("rw")
            && !self.perms.contains('s') // not shared
            && self.pathname.is_empty()
            && self.inode == 0
    }

    /// Whether this mapping has execute permission (should be skipped).
    pub fn has_exec(&self) -> bool {
        self.perms.contains('x')
    }

    /// Whether this is a special kernel mapping (vdso, vvar, vsyscall).
    pub fn is_special(&self) -> bool {
        self.pathname.starts_with("[vdso]")
            || self.pathname.starts_with("[vvar")
            || self.pathname.starts_with("[vsyscall]")
            || self.pathname.starts_with("[stack")
            || self.pathname.starts_with("[heap]")
    }
}

/// Per-process KSM statistics from /proc/PID/ksm_stat.
#[derive(Debug, Clone, Default)]
pub struct KsmProcStat {
    pub rmap_items: u64,
    pub merging_pages: u64,
    pub process_profit: i64,
    pub merge_any: bool,
    pub mergeable: bool,
}

/// An anonymous memory region scored for KSM merge potential.
#[derive(Debug, Clone)]
pub struct MergeCandidate {
    pub pid: u32,
    pub process_name: String,
    pub start: u64,
    pub end: u64,
    pub size_bytes: u64,
    pub anon_rss_kb: u64,
}

/// List all PIDs in /proc (numeric directory entries).
pub fn list_pids() -> Result<Vec<u32>> {
    let mut pids = Vec::new();
    let entries = fs::read_dir("/proc").map_err(|e| ZramdedupError::Procfs {
        pid: 0,
        detail: format!("cannot read /proc: {e}"),
    })?;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if let Ok(pid) = name_str.parse::<u32>() {
            pids.push(pid);
        }
    }

    Ok(pids)
}

/// Read process status (VmRSS, VmAnon, etc.) from /proc/PID/status.
pub fn read_process_status(pid: u32) -> Result<ProcessStatus> {
    let path = format!("/proc/{pid}/status");
    let content = fs::read_to_string(&path).map_err(|e| ZramdedupError::Procfs {
        pid,
        detail: format!("cannot read {path}: {e}"),
    })?;

    let mut status = ProcessStatus {
        pid,
        ..Default::default()
    };

    for line in content.lines() {
        if let Some(value) = line.strip_prefix("Name:") {
            status.name = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("VmRSS:") {
            status.vm_rss_kb = parse_kb_value(value);
        } else if let Some(value) = line.strip_prefix("VmAnon:") {
            status.vm_anon_kb = parse_kb_value(value);
        } else if let Some(value) = line.strip_prefix("VmSwap:") {
            status.vm_swap_kb = parse_kb_value(value);
        }
    }

    Ok(status)
}

/// Read and parse /proc/PID/maps.
pub fn read_process_maps(pid: u32) -> Result<Vec<MapsEntry>> {
    let path = format!("/proc/{pid}/maps");
    let content = fs::read_to_string(&path).map_err(|e| ZramdedupError::Procfs {
        pid,
        detail: format!("cannot read {path}: {e}"),
    })?;

    let mut entries = Vec::new();
    for line in content.lines() {
        if let Some(entry) = parse_maps_line(line) {
            entries.push(entry);
        }
    }

    Ok(entries)
}

/// Read per-process KSM statistics from /proc/PID/ksm_stat.
pub fn read_ksm_stat(pid: u32) -> Result<KsmProcStat> {
    let path = format!("/proc/{pid}/ksm_stat");
    let content = fs::read_to_string(&path).map_err(|e| ZramdedupError::Procfs {
        pid,
        detail: format!("cannot read {path}: {e}"),
    })?;

    let mut stat = KsmProcStat::default();

    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            match parts[0] {
                "ksm_rmap_items" => stat.rmap_items = parts[1].parse().unwrap_or(0),
                "ksm_merging_pages" => stat.merging_pages = parts[1].parse().unwrap_or(0),
                "ksm_process_profit" => stat.process_profit = parts[1].parse().unwrap_or(0),
                "ksm_merge_any" => stat.merge_any = parts[1] == "1" || parts[1] == "yes",
                "ksm_mergeable" => stat.mergeable = parts[1] == "1" || parts[1] == "yes",
                _ => {}
            }
        }
    }

    Ok(stat)
}

/// Read PIDs belonging to a specific cgroup (cgroup v2).
pub fn read_cgroup_procs(cgroup_path: &str) -> Result<Vec<u32>> {
    let procs_path = format!("{cgroup_path}/cgroup.procs");
    let content = fs::read_to_string(&procs_path).map_err(|e| ZramdedupError::Procfs {
        pid: 0,
        detail: format!("cannot read {procs_path}: {e}"),
    })?;

    let pids: Vec<u32> = content
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();

    Ok(pids)
}

/// Read a pagemap entry for a given virtual address.
/// Returns the PFN (page frame number) if the page is present.
pub fn read_pagemap_pfn(pid: u32, vaddr: u64) -> Result<Option<u64>> {
    let path = format!("/proc/{pid}/pagemap");
    let offset = (vaddr / 4096) * 8; // 8 bytes per page

    use std::io::{Read, Seek, SeekFrom};
    let mut file = fs::File::open(&path).map_err(|e| ZramdedupError::Procfs {
        pid,
        detail: format!("cannot open {path}: {e}"),
    })?;

    file.seek(SeekFrom::Start(offset))
        .map_err(|e| ZramdedupError::Procfs {
            pid,
            detail: format!("cannot seek pagemap: {e}"),
        })?;

    let mut buf = [0u8; 8];
    file.read_exact(&mut buf)
        .map_err(|e| ZramdedupError::Procfs {
            pid,
            detail: format!("cannot read pagemap entry: {e}"),
        })?;

    let entry = u64::from_ne_bytes(buf);

    // Bit 63: page present
    let present = (entry >> 63) & 1 != 0;
    if !present {
        return Ok(None);
    }

    // Bits 0-54: PFN
    let pfn = entry & 0x7FFFFFFFFFFFFF;
    Ok(Some(pfn))
}

/// Read PFNs for all pages in a memory range.
pub fn read_pagemap_range(pid: u32, start: u64, end: u64) -> Result<Vec<u64>> {
    let path = format!("/proc/{pid}/pagemap");
    let mut pfns = Vec::new();

    use std::io::{Read, Seek, SeekFrom};
    let mut file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            return Err(ZramdedupError::Procfs {
                pid,
                detail: format!("cannot open {path}: {e}"),
            });
        }
    };

    let page_start = start / 4096;
    let page_count = (end - start) / 4096;

    if let Err(_) = file.seek(SeekFrom::Start(page_start * 8)) {
        return Ok(pfns); // Process may have exited
    }

    let mut buf = vec![0u8; (page_count as usize) * 8];
    let bytes_read = file.read(&mut buf).unwrap_or(0);
    let entries_read = bytes_read / 8;

    for i in 0..entries_read {
        let offset = i * 8;
        let entry = u64::from_ne_bytes([
            buf[offset],
            buf[offset + 1],
            buf[offset + 2],
            buf[offset + 3],
            buf[offset + 4],
            buf[offset + 5],
            buf[offset + 6],
            buf[offset + 7],
        ]);

        // Bit 63: present, Bit 62: swap, Bits 0-54: PFN
        let present = (entry >> 63) & 1 != 0;
        if present {
            let pfn = entry & 0x7FFFFFFFFFFFFF;
            if pfn != 0 {
                pfns.push(pfn);
            }
        }
    }

    Ok(pfns)
}

/// Check if a process name matches any pattern in the blocklist.
pub fn is_blocklisted(name: &str, blocklist: &[String]) -> bool {
    blocklist.iter().any(|pattern| name.contains(pattern))
}

/// Get the comm (short name) of a process.
pub fn read_process_comm(pid: u32) -> Result<String> {
    let path = format!("/proc/{pid}/comm");
    fs::read_to_string(&path)
        .map(|s| s.trim().to_string())
        .map_err(|e| ZramdedupError::Procfs {
            pid,
            detail: format!("cannot read {path}: {e}"),
        })
}

// --- Internal helpers ---

fn parse_kb_value(s: &str) -> u64 {
    s.trim()
        .trim_end_matches(" kB")
        .trim()
        .parse::<u64>()
        .unwrap_or(0)
}

fn parse_maps_line(line: &str) -> Option<MapsEntry> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 5 {
        return None;
    }

    let range: Vec<&str> = parts[0].split('-').collect();
    if range.len() != 2 {
        return None;
    }

    let start = u64::from_str_radix(range[0], 16).ok()?;
    let end = u64::from_str_radix(range[1], 16).ok()?;
    let perms = parts[1].to_string();
    let offset = u64::from_str_radix(parts[2], 16).unwrap_or(0);
    let dev = parts[3].to_string();
    let inode = parts[4].parse::<u64>().unwrap_or(0);
    let pathname = if parts.len() > 5 {
        parts[5..].join(" ")
    } else {
        String::new()
    };

    Some(MapsEntry {
        start,
        end,
        perms,
        offset,
        dev,
        inode,
        pathname,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_maps_line() {
        let line = "7f8c40000000-7f8c40021000 rw-p 00000000 00:00 0";
        let entry = parse_maps_line(line).unwrap();
        assert_eq!(entry.start, 0x7f8c40000000);
        assert_eq!(entry.end, 0x7f8c40021000);
        assert!(entry.is_anon_rw());
        assert!(!entry.has_exec());
    }

    #[test]
    fn test_parse_maps_line_file_backed() {
        let line = "7f8c3fe00000-7f8c3fe01000 r--p 00000000 08:01 12345 /usr/lib/libc.so";
        let entry = parse_maps_line(line).unwrap();
        assert!(!entry.is_anon_rw());
    }

    #[test]
    fn test_is_blocklisted() {
        let blocklist = vec!["zramdedup".to_string(), "ksmd".to_string()];
        assert!(is_blocklisted("zramdedup-daemon", &blocklist));
        assert!(is_blocklisted("ksmd", &blocklist));
        assert!(!is_blocklisted("firefox", &blocklist));
    }

    #[test]
    fn test_parse_kb_value() {
        assert_eq!(parse_kb_value("  12345 kB"), 12345);
        assert_eq!(parse_kb_value("0 kB"), 0);
    }

    #[test]
    fn test_parse_kb_value_invalid() {
        // Invalid number → unwrap_or(0)
        assert_eq!(parse_kb_value("not_a_number"), 0);
        assert_eq!(parse_kb_value(""), 0);
    }

    // ── MapsEntry ───────────────────────────────────────────────────────

    #[test]
    fn test_maps_entry_size() {
        let entry = MapsEntry {
            start: 0x1000,
            end: 0x5000,
            perms: "rw-p".into(),
            offset: 0,
            dev: "00:00".into(),
            inode: 0,
            pathname: String::new(),
        };
        assert_eq!(entry.size(), 0x4000);
    }

    #[test]
    fn test_maps_entry_page_count() {
        let entry = MapsEntry {
            start: 0x1000,
            end: 0x5000,
            perms: "rw-p".into(),
            offset: 0,
            dev: "00:00".into(),
            inode: 0,
            pathname: String::new(),
        };
        assert_eq!(entry.page_count(), 4);
    }

    #[test]
    fn test_maps_entry_page_count_partial() {
        // A 1-byte range still counts as 1 page (floor division)
        let entry = MapsEntry {
            start: 0x1000,
            end: 0x1001,
            perms: "rw-p".into(),
            offset: 0,
            dev: "00:00".into(),
            inode: 0,
            pathname: String::new(),
        };
        assert_eq!(entry.page_count(), 0); // 1/4096 = 0
    }

    #[test]
    fn test_is_anon_rw_true() {
        let entry = MapsEntry {
            start: 0x1000,
            end: 0x2000,
            perms: "rw-p".into(),
            offset: 0,
            dev: "00:00".into(),
            inode: 0,
            pathname: String::new(),
        };
        assert!(entry.is_anon_rw());
    }

    #[test]
    fn test_is_anon_rw_false_shared() {
        let entry = MapsEntry {
            start: 0x1000,
            end: 0x2000,
            perms: "rws-".into(),
            offset: 0,
            dev: "00:00".into(),
            inode: 0,
            pathname: String::new(),
        };
        assert!(!entry.is_anon_rw());
    }

    #[test]
    fn test_is_anon_rw_false_file_backed() {
        let entry = MapsEntry {
            start: 0x1000,
            end: 0x2000,
            perms: "rw-p".into(),
            offset: 0,
            dev: "00:00".into(),
            inode: 12345,
            pathname: "/usr/lib/libc.so".into(),
        };
        assert!(!entry.is_anon_rw());
    }

    #[test]
    fn test_has_exec() {
        let exec_entry = MapsEntry {
            start: 0x1000,
            end: 0x2000,
            perms: "r-xp".into(),
            offset: 0,
            dev: "00:00".into(),
            inode: 0,
            pathname: String::new(),
        };
        assert!(exec_entry.has_exec());

        let no_exec_entry = MapsEntry {
            start: 0x1000,
            end: 0x2000,
            perms: "rw-p".into(),
            offset: 0,
            dev: "00:00".into(),
            inode: 0,
            pathname: String::new(),
        };
        assert!(!no_exec_entry.has_exec());
    }

    #[test]
    fn test_is_special() {
        let vdso = MapsEntry {
            start: 0x1000,
            end: 0x2000,
            perms: "r-xp".into(),
            offset: 0,
            dev: "00:00".into(),
            inode: 0,
            pathname: "[vdso]".into(),
        };
        assert!(vdso.is_special());

        let heap = MapsEntry {
            start: 0x1000,
            end: 0x2000,
            perms: "rw-p".into(),
            offset: 0,
            dev: "00:00".into(),
            inode: 0,
            pathname: "[heap]".into(),
        };
        assert!(heap.is_special());

        let normal = MapsEntry {
            start: 0x1000,
            end: 0x2000,
            perms: "rw-p".into(),
            offset: 0,
            dev: "00:00".into(),
            inode: 0,
            pathname: String::new(),
        };
        assert!(!normal.is_special());
    }

    // ── parse_maps_line edge cases ──────────────────────────────────────

    #[test]
    fn test_parse_maps_line_too_few_parts() {
        assert!(parse_maps_line("").is_none());
        assert!(parse_maps_line("7f8c40000000-7f8c40021000 rw-p 00000000 00:00").is_none());
    }

    #[test]
    fn test_parse_maps_line_bad_range() {
        // range without '-' separator
        assert!(parse_maps_line("invalid rw-p 00000000 00:00 0").is_none());
    }

    #[test]
    fn test_parse_maps_line_file_backed_path() {
        let line = "7f8c3fe00000-7f8c3fe01000 r--p 00000000 08:01 12345 /usr/lib/libc.so";
        let entry = parse_maps_line(line).unwrap();
        assert_eq!(entry.pathname, "/usr/lib/libc.so");
        assert_eq!(entry.inode, 12345);
    }

    #[test]
    fn test_parse_maps_line_path_with_spaces() {
        let line = "7f8c3fe00000-7f8c3fe01000 rw-p 00000000 00:00 0 /path/with spaces (deleted)";
        let entry = parse_maps_line(line).unwrap();
        assert!(entry.pathname.contains("spaces"));
        assert!(entry.pathname.contains("deleted"));
    }

    #[test]
    fn test_parse_maps_line_non_zero_offset() {
        let line = "7f8c40000000-7f8c40001000 rw-p 00001000 00:00 0";
        let entry = parse_maps_line(line).unwrap();
        assert_eq!(entry.offset, 0x1000);
    }

    #[test]
    fn test_parse_maps_line_vvar() {
        let line = "7fff3fe00000-7fff3fe01000 r--p 00000000 00:00 0 [vvar]";
        let entry = parse_maps_line(line).unwrap();
        assert!(entry.is_special());
    }

    // ── ProcessStatus / parse_kb_value ──────────────────────────────────

    #[test]
    fn test_parse_kb_value_large_number() {
        assert_eq!(parse_kb_value("  1048576 kB"), 1048576);
    }

    // ── read_ksm_stat format ────────────────────────────────────────────

    #[test]
    fn test_parse_ksm_stat_content() {
        // parse the raw format that /proc/PID/ksm_stat produces
        let content = "ksm_rmap_items 42\nksm_merging_pages 7\nksm_process_profit 12345\nksm_merge_any 1\nksm_mergeable yes\n";
        let mut stat = KsmProcStat::default();
        for line in content.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                match parts[0] {
                    "ksm_rmap_items" => stat.rmap_items = parts[1].parse().unwrap_or(0),
                    "ksm_merging_pages" => stat.merging_pages = parts[1].parse().unwrap_or(0),
                    "ksm_process_profit" => stat.process_profit = parts[1].parse().unwrap_or(0),
                    "ksm_merge_any" => stat.merge_any = parts[1] == "1" || parts[1] == "yes",
                    "ksm_mergeable" => stat.mergeable = parts[1] == "1" || parts[1] == "yes",
                    _ => {}
                }
            }
        }
        assert_eq!(stat.rmap_items, 42);
        assert_eq!(stat.merging_pages, 7);
        assert_eq!(stat.process_profit, 12345);
        assert!(stat.merge_any);
        assert!(stat.mergeable);
    }

    #[test]
    fn test_parse_ksm_stat_unknown_field_skipped() {
        let content = "ksm_unknown_field 99\nksm_rmap_items 5\n";
        let mut stat = KsmProcStat::default();
        for line in content.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                match parts[0] {
                    "ksm_rmap_items" => stat.rmap_items = parts[1].parse().unwrap_or(0),
                    _ => {}
                }
            }
        }
        assert_eq!(stat.rmap_items, 5);
    }

    // ── MergeCandidate ───────────────────────────────────────────────────

    #[test]
    fn test_merge_candidate_construction() {
        let mc = MergeCandidate {
            pid: 100,
            process_name: "test_proc".into(),
            start: 0x10000,
            end: 0x20000,
            size_bytes: 0x10000,
            anon_rss_kb: 64,
        };
        assert_eq!(mc.pid, 100);
        assert_eq!(mc.size_bytes, 65536);
        assert_eq!(mc.anon_rss_kb, 64);
    }
}

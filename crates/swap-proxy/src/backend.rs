//! Tier 3: Backend I/O for the swap proxy.
//!
//! Manages the underlying zram block device, forwarding page reads/writes
//! and maintaining a translation table between virtual swap offsets and
//! physical backend locations.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::fingerprint::PAGE_SIZE;

/// Backend storage statistics.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct BackendStats {
    pub pages_written: u64,
    pub pages_read: u64,
    pub bytes_written: u64,
    pub bytes_read: u64,
    pub slots_allocated: u64,
    pub slots_freed: u64,
}

/// A backend page store backed by a block device or file.
pub struct PageStore {
    /// The underlying file/device.
    file: File,
    /// Total number of page slots available.
    total_slots: u64,
    /// Statistics.
    pages_written: AtomicU64,
    pages_read: AtomicU64,
}

impl PageStore {
    /// Open a block device or file as a page store.
    pub fn open(path: &Path, total_slots: u64) -> anyhow::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;

        Ok(Self {
            file,
            total_slots,
            pages_written: AtomicU64::new(0),
            pages_read: AtomicU64::new(0),
        })
    }

    /// Create a file-backed page store for testing.
    pub fn create_file(path: &Path, total_slots: u64) -> anyhow::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;

        // Pre-allocate the file
        let total_size = total_slots * PAGE_SIZE as u64;
        file.set_len(total_size)?;

        Ok(Self {
            file,
            total_slots,
            pages_written: AtomicU64::new(0),
            pages_read: AtomicU64::new(0),
        })
    }

    /// Write a page to the given slot.
    pub fn write_page(&mut self, slot: u64, data: &[u8]) -> anyhow::Result<()> {
        if slot >= self.total_slots {
            anyhow::bail!("Slot {slot} out of range (max {})", self.total_slots);
        }

        let offset = slot * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;

        // Pad or truncate to exactly PAGE_SIZE
        if data.len() >= PAGE_SIZE {
            self.file.write_all(&data[..PAGE_SIZE])?;
        } else {
            self.file.write_all(data)?;
            let padding = vec![0u8; PAGE_SIZE - data.len()];
            self.file.write_all(&padding)?;
        }

        self.pages_written.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Read a page from the given slot.
    pub fn read_page(&mut self, slot: u64) -> anyhow::Result<Vec<u8>> {
        if slot >= self.total_slots {
            anyhow::bail!("Slot {slot} out of range (max {})", self.total_slots);
        }

        let offset = slot * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;

        let mut buf = vec![0u8; PAGE_SIZE];
        self.file.read_exact(&mut buf)?;

        self.pages_read.fetch_add(1, Ordering::Relaxed);
        Ok(buf)
    }

    /// Get backend statistics.
    #[allow(dead_code)]
    pub fn stats(&self) -> BackendStats {
        BackendStats {
            pages_written: self.pages_written.load(Ordering::Relaxed),
            pages_read: self.pages_read.load(Ordering::Relaxed),
            slots_allocated: self.total_slots,
            ..Default::default()
        }
    }

    /// Total capacity in pages.
    #[allow(dead_code)]
    pub fn total_slots(&self) -> u64 {
        self.total_slots
    }
}

/// Translation table mapping virtual swap offsets to dedup table state.
///
/// Each entry points to either:
/// - A unique page stored at a backend slot
/// - A deduplicated page (ref-counted in the dedup table)
pub struct TranslationTable {
    /// Maps virtual offset (page-aligned) to the fingerprint + backend slot.
    entries: std::collections::HashMap<u64, TranslationEntry>,
}

#[derive(Debug, Clone)]
pub struct TranslationEntry {
    /// Fingerprint of the page stored at this virtual offset.
    pub fingerprint: crate::fingerprint::Fingerprint,
    /// Backend slot where the actual data lives.
    pub backend_slot: u64,
}

impl TranslationTable {
    pub fn new() -> Self {
        Self {
            entries: std::collections::HashMap::new(),
        }
    }

    /// Record that a virtual offset maps to a given fingerprint and backend slot.
    pub fn insert(&mut self, virtual_offset: u64, fingerprint: crate::fingerprint::Fingerprint, backend_slot: u64) {
        self.entries.insert(virtual_offset, TranslationEntry {
            fingerprint,
            backend_slot,
        });
    }

    /// Look up the translation for a virtual offset.
    pub fn lookup(&self, virtual_offset: u64) -> Option<&TranslationEntry> {
        self.entries.get(&virtual_offset)
    }

    /// Remove a translation entry (when the virtual page is discarded).
    pub fn remove(&mut self, virtual_offset: u64) -> Option<TranslationEntry> {
        self.entries.remove(&virtual_offset)
    }

    /// Number of active translations.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

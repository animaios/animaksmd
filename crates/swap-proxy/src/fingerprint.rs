//! Tier 3: Page fingerprinting using xxh3-128.
//!
//! Provides fast 128-bit fingerprints for 4KB pages, used by the
//! dedup table to identify duplicate pages without byte-for-byte comparison.

use xxhash_rust::xxh3::xxh3_128;

/// Size of a memory page in bytes.
pub const PAGE_SIZE: usize = 4096;

/// A 128-bit page fingerprint.
pub type Fingerprint = [u8; 16];

/// Compute a 128-bit fingerprint of a 4KB page.
///
/// Uses xxh3-128 which runs at ~6GB/s on modern x86_64,
/// giving ~1.5 million pages/second single-threaded.
#[inline]
pub fn fingerprint_page(data: &[u8]) -> Fingerprint {
    let hash = xxh3_128(data);
    hash.to_le_bytes()
}

/// Verify a fingerprint match by comparing the first N bytes of page content.
///
/// Used as a collision guard: when the dedup table reports a hit,
/// we verify the first 64 bytes match before trusting the fingerprint.
#[inline]
pub fn verify_page_prefix(stored_prefix: &[u8], new_data: &[u8]) -> bool {
    let prefix_len = 64.min(stored_prefix.len()).min(new_data.len());
    stored_prefix[..prefix_len] == new_data[..prefix_len]
}

/// Extract a 64-byte prefix from a page for collision verification.
#[inline]
pub fn page_prefix(data: &[u8]) -> [u8; 64] {
    let mut prefix = [0u8; 64];
    let len = data.len().min(64);
    prefix[..len].copy_from_slice(&data[..len]);
    prefix
}

/// Derive bloom filter hash indices from a fingerprint.
///
/// Returns 3 indices suitable for a bloom filter of the given size.
/// Uses different byte slices of the 128-bit fingerprint.
pub fn bloom_indices(fp: &Fingerprint, num_bits: usize) -> [usize; 3] {
    let h1 = u64::from_le_bytes([fp[0], fp[1], fp[2], fp[3], fp[4], fp[5], fp[6], fp[7]]);
    let h2 = u64::from_le_bytes([fp[8], fp[9], fp[10], fp[11], fp[12], fp[13], fp[14], fp[15]]);
    let h3 = h1.wrapping_add(h2);

    [
        (h1 as usize) % num_bits,
        (h2 as usize) % num_bits,
        (h3 as usize) % num_bits,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fingerprint_deterministic() {
        let page = vec![0xABu8; PAGE_SIZE];
        let fp1 = fingerprint_page(&page);
        let fp2 = fingerprint_page(&page);
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn test_fingerprint_different_pages() {
        let page_a = vec![0x00u8; PAGE_SIZE];
        let page_b = vec![0xFFu8; PAGE_SIZE];
        let fp_a = fingerprint_page(&page_a);
        let fp_b = fingerprint_page(&page_b);
        assert_ne!(fp_a, fp_b);
    }

    #[test]
    fn test_verify_page_prefix() {
        let page = vec![0xABu8; PAGE_SIZE];
        let prefix = page_prefix(&page);
        assert!(verify_page_prefix(&prefix, &page));

        let different = vec![0xCDu8; PAGE_SIZE];
        assert!(!verify_page_prefix(&prefix, &different));
    }

    #[test]
    fn test_bloom_indices_within_range() {
        let page = vec![0x42u8; PAGE_SIZE];
        let fp = fingerprint_page(&page);
        let indices = bloom_indices(&fp, 1_000_000);
        for idx in indices {
            assert!(idx < 1_000_000);
        }
    }

    #[test]
    fn test_no_collisions_in_small_set() {
        use std::collections::HashSet;
        let mut fingerprints = HashSet::new();

        // Hash 10K unique pages
        for i in 0u64..10_000 {
            let mut page = vec![0u8; PAGE_SIZE];
            page[0..8].copy_from_slice(&i.to_le_bytes());
            let fp = fingerprint_page(&page);
            assert!(fingerprints.insert(fp), "Collision at page {i}");
        }
    }
}

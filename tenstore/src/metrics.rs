use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Traffic counters for a [`crate::GenStore`] instance.
///
/// A single `Arc<StoreMetrics>` is shared by every clone of a `GenStore`
/// (the run itself, the background writer thread, handle clones...), so the
/// counters reflect the traffic of the whole store.
#[derive(Debug, Default)]
pub struct StoreMetrics {
    /// Number of `fetch` calls issued on the store.
    fetch_count: AtomicU64,
    /// Number of fetches that missed the in-memory LRU cache and therefore
    /// read the backing page file from disk.
    disk_read_count: AtomicU64,
    /// Total bytes read from backing page files.
    disk_read_bytes: AtomicU64,
    /// Number of `store` calls issued on the store.
    store_count: AtomicU64,
    /// Total bytes written to backing page files (serialized size).
    store_bytes: AtomicU64,
}
/// Point-in-time copy of the counters, for logging and assertions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoreMetricsSnapshot {
    pub fetch_count: u64,
    pub disk_read_count: u64,
    pub disk_read_bytes: u64,
    pub store_count: u64,
    pub store_bytes: u64,
}

impl StoreMetrics {
    pub fn new_arc() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn inc_fetch(&self) {
        self.fetch_count.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn inc_store_count(&self) {
        self.store_count.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn inc_disk_read(&self, bytes: usize) {
        self.disk_read_count.fetch_add(1, Ordering::Relaxed);
        self.disk_read_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub(crate) fn inc_store_bytes(&self, bytes: usize) {
        self.store_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> StoreMetricsSnapshot {
        let load = |c: &AtomicU64| c.load(Ordering::Relaxed);
        StoreMetricsSnapshot {
            fetch_count: load(&self.fetch_count),
            disk_read_count: load(&self.disk_read_count),
            disk_read_bytes: load(&self.disk_read_bytes),
            store_count: load(&self.store_count),
            store_bytes: load(&self.store_bytes),
        }
    }

    pub fn reset(&self) {
        for c in [
            &self.fetch_count,
            &self.disk_read_count,
            &self.disk_read_bytes,
            &self.store_count,
            &self.store_bytes,
        ] {
            c.store(0, Ordering::Relaxed);
        }
    }
}

impl StoreMetricsSnapshot {
    /// True when no fetch had to touch the backing page files, i.e. every
    /// fetch was served from the in-memory cache (or no fetch happened at all).
    pub fn no_disk_reads(&self) -> bool {
        self.disk_read_count == 0
    }
}

impl std::fmt::Display for StoreMetricsSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "fetch={} disk_reads={} ({} bytes) stores={} ({} bytes)",
            self.fetch_count,
            self.disk_read_count,
            self.disk_read_bytes,
            self.store_count,
            self.store_bytes
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate_and_reset() {
        let m = StoreMetrics::new_arc();
        m.inc_fetch();
        m.inc_fetch();
        m.inc_disk_read(128);
        m.inc_store_count();
        m.inc_store_bytes(256);
        let snap = m.snapshot();
        assert_eq!(snap.fetch_count, 2);
        assert_eq!(snap.disk_read_count, 1);
        assert_eq!(snap.disk_read_bytes, 128);
        assert_eq!(snap.store_count, 1);
        assert_eq!(snap.store_bytes, 256);
        assert!(!snap.no_disk_reads());
        m.reset();
        assert_eq!(m.snapshot(), StoreMetricsSnapshot::default());
        assert!(m.snapshot().no_disk_reads());
    }
}

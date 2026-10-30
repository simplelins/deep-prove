//! A purely in-memory page store.
//!
//! Same page semantics as [`crate::local::DiskStore`] (msgpack pages keyed by
//! run id + key) without any disk backing: the pages live in a process-local
//! map. Used by the streaming prove path with `--memory-store` to skip the
//! write-behind disk round-trip entirely — the in-memory tensor copies held
//! by the streaming pins are the primary data, the store only serves as a
//! safety net for unexpected re-hydration.
//!
//! NOTE: with this backend, a tensor whose in-memory copies have all been
//! dropped cannot be recovered (there is no file to re-read); the memory
//! budget must account for every pinned page.

use crate::{
    StorageKey, StoreError, metrics::StoreMetrics,
    local::InternalKey,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use uuid::Uuid;

/// A process-local page store, no disk backing.
#[derive(Debug)]
pub struct MemoryStore {
    /// Pages indexed by run id and internal key. The serialized bytes are
    /// shared through an `Arc`, mirroring the LRU entry semantics of the
    /// disk store.
    pages: HashMap<Uuid, HashMap<InternalKey, Arc<Vec<u8>>>>,

    /// Optional traffic counters (stored bytes / fetches).
    metrics: Option<Arc<StoreMetrics>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            pages: HashMap::new(),
            metrics: None,
        }
    }

    /// Attach traffic counters to this store.
    pub fn set_metrics(&mut self, metrics: Arc<StoreMetrics>) {
        self.metrics = Some(metrics);
    }

    /// Fetch and decode the data under the given run id and key.
    pub(crate) fn fetch<T>(
        &mut self,
        run_id: Uuid,
        storage_key: &StorageKey<T>,
    ) -> Result<T, StoreError>
    where
        T: for<'a> Deserialize<'a>,
    {
        let key = InternalKey::from_storage_key_with_run_id(run_id, storage_key);
        let bytes = self
            .pages
            .get(&run_id)
            .and_then(|ks| ks.get(&key).cloned())
            .ok_or(StoreError::KeyUnknown)?;
        Ok(rmp_serde::from_slice(&bytes)?)
    }

    /// Encode and store the data under the given run id and key.
    pub(crate) fn store<T>(
        &mut self,
        run_id: Uuid,
        storage_key: &StorageKey<T>,
        data: &T,
    ) -> Result<(), StoreError>
    where
        T: Serialize,
    {
        let serialized = rmp_serde::to_vec(&data).map_err(StoreError::from)?;
        if let Some(metrics) = self.metrics.as_ref() {
            metrics.inc_store_bytes(serialized.len());
        }
        let key = InternalKey::from_storage_key_with_run_id(run_id, storage_key);
        self.pages
            .entry(run_id)
            .or_default()
            .insert(key, Arc::new(serialized));
        Ok(())
    }

    /// Drop all pages of the given run.
    pub(crate) fn clean_up(&mut self, run_id: Uuid) -> Result<(), StoreError> {
        self.pages.remove(&run_id);
        Ok(())
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GenStore, GenericStore};

    #[test]
    fn round_trip() {
        let store = GenStore::new_memory();
        let key: StorageKey<Vec<u64>> = StorageKey::new("N1_0");
        store
            .store(&key, &vec![1u64, 2, 3])
            .expect("store failed");
        let fetched: Vec<u64> = store.fetch(&key).expect("fetch failed");
        assert_eq!(fetched, vec![1, 2, 3]);

        let metrics = store.metrics_snapshot();
        assert_eq!(metrics.disk_read_count, 0, "memory store never reads disk");
        assert_eq!(metrics.store_count, 1);

        // a different key namespace is a miss
        let other: StorageKey<Vec<u64>> = StorageKey::new("N2_0");
        assert!(store.fetch::<Vec<u64>>(&other).is_err());

        // clean-up drops the run's pages (`clean_up` consumes the store, the
        // clone shares the same underlying page map)
        store.clone().clean_up().expect("clean-up failed");
        assert!(store.fetch::<Vec<u64>>(&key).is_err());
    }
}

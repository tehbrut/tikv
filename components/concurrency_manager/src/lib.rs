// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

//! The concurrency manager is responsible for concurrency control of
//! transactions.
//!
//! The concurrency manager contains a lock table in memory. Lock information
//! can be stored in it and reading requests can check if these locks block
//! the read.
//!
//! In order to mutate the lock of a key stored in the lock table, it needs
//! to be locked first using `lock_key` or `lock_keys`.

mod key_handle;
mod lock_table;

pub use self::key_handle::{KeyHandle, KeyHandleGuard};
pub use self::lock_table::LockTable;

use std::{
    mem::{self, MaybeUninit},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use txn_types::{Key, Lock, TimeStamp};

// TODO: Currently we are using a Mutex<BTreeMap> to implement the handle table.
// In the future we should replace it with a concurrent ordered map.
// Pay attention that the async functions of ConcurrencyManager should not hold
// the mutex.
#[derive(Clone)]
pub struct ConcurrencyManager {
    max_read_ts: Arc<AtomicU64>,
    lock_table: LockTable,
}

impl ConcurrencyManager {
    pub fn new(latest_ts: TimeStamp) -> Self {
        ConcurrencyManager {
            max_read_ts: Arc::new(AtomicU64::new(latest_ts.into_inner())),
            lock_table: LockTable::default(),
        }
    }

    pub fn max_read_ts(&self) -> TimeStamp {
        TimeStamp::new(self.max_read_ts.load(Ordering::SeqCst))
    }

    /// Updates max_read_ts with the given read_ts. It has no effect if
    /// max_read_ts >= read_ts or read_ts is TimeStamp::max().
    pub fn update_max_read_ts(&self, read_ts: TimeStamp) {
        if read_ts != TimeStamp::max() {
            self.max_read_ts
                .fetch_max(read_ts.into_inner(), Ordering::SeqCst);
        }
    }

    /// Acquires a mutex of the key and returns an RAII guard. When the guard goes
    /// out of scope, the mutex will be unlocked.
    ///
    /// The guard can be used to store Lock in the table. The stored lock
    /// is visible to `read_key_check` and `read_range_check`.
    pub async fn lock_key(&self, key: &Key) -> KeyHandleGuard {
        self.lock_table.lock_key(key).await
    }

    /// Acquires mutexes of the keys and returns the RAII guards. The order of the
    /// guards is the same with the given keys.
    ///
    /// The guards can be used to store Lock in the table. The stored lock
    /// is visible to `read_key_check` and `read_range_check`.
    pub async fn lock_keys(&self, keys: impl Iterator<Item = &Key>) -> Vec<KeyHandleGuard> {
        let mut keys_with_index: Vec<_> = keys.enumerate().collect();
        // To prevent deadlock, we sort the keys and lock them one by one.
        keys_with_index.sort_by_key(|(_, key)| *key);
        let mut result: Vec<MaybeUninit<KeyHandleGuard>> = Vec::new();
        result.resize_with(keys_with_index.len(), || MaybeUninit::uninit());
        for (index, key) in keys_with_index {
            result[index] = MaybeUninit::new(self.lock_table.lock_key(key).await);
        }
        #[allow(clippy::unsound_collection_transmute)]
        unsafe {
            mem::transmute(result)
        }
    }

    /// Checks if there is a memory lock of the key which blocks the read.
    /// The given `check_fn` should return false iff the lock passed in
    /// blocks the read.
    pub fn read_key_check<E>(
        &self,
        key: &Key,
        check_fn: impl FnOnce(&Lock) -> Result<(), E>,
    ) -> Result<(), E> {
        self.lock_table.check_key(key, check_fn)
    }

    /// Checks if there is a memory lock in the range which blocks the read.
    /// The given `check_fn` should return false iff the lock passed in
    /// blocks the read.
    pub fn read_range_check<E>(
        &self,
        start_key: Option<&Key>,
        end_key: Option<&Key>,
        check_fn: impl FnMut(&Key, &Lock) -> Result<(), E>,
    ) -> Result<(), E> {
        self.lock_table.check_range(start_key, end_key, check_fn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_lock_keys_order() {
        let concurrency_manager = ConcurrencyManager::new(1.into());
        let keys: Vec<_> = [b"c", b"a", b"b"]
            .iter()
            .map(|k| Key::from_raw(*k))
            .collect();
        let guards = concurrency_manager.lock_keys(keys.iter()).await;
        for (key, guard) in keys.iter().zip(&guards) {
            assert_eq!(key, guard.key());
        }
    }

    #[tokio::test]
    async fn test_update_max_read_ts() {
        let concurrency_manager = ConcurrencyManager::new(10.into());
        concurrency_manager.update_max_read_ts(20.into());
        assert_eq!(concurrency_manager.max_read_ts(), 20.into());

        concurrency_manager.update_max_read_ts(5.into());
        assert_eq!(concurrency_manager.max_read_ts(), 20.into());

        concurrency_manager.update_max_read_ts(TimeStamp::max());
        assert_eq!(concurrency_manager.max_read_ts(), 20.into());
    }
}

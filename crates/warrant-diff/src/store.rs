//! Where file contents live while they are being reasoned about.
//!
//! Snapshots are manifests of addresses, not of bytes. The bytes sit in a
//! content store — in production the ledger's blob store, so that every
//! pre-image a probe was run against is still retrievable when someone asks
//! how a number was produced six weeks later.

use std::collections::HashMap;
use std::sync::Mutex;

use warrant_core::Hash;
use warrant_ledger::BlobStore;

/// Somewhere content-addressed bytes can be put and got.
pub trait ContentStore: Send + Sync {
    /// Store bytes, returning their address.
    fn put(&self, bytes: &[u8]) -> std::result::Result<Hash, String>;

    /// Retrieve bytes by address.
    fn get(&self, hash: &Hash) -> std::result::Result<Vec<u8>, String>;

    /// Whether an address is present.
    fn contains(&self, hash: &Hash) -> bool;
}

impl ContentStore for BlobStore {
    fn put(&self, bytes: &[u8]) -> std::result::Result<Hash, String> {
        BlobStore::put(self, bytes).map_err(|e| e.to_string())
    }

    fn get(&self, hash: &Hash) -> std::result::Result<Vec<u8>, String> {
        BlobStore::get(self, hash).map_err(|e| e.to_string())
    }

    fn contains(&self, hash: &Hash) -> bool {
        BlobStore::contains(self, hash)
    }
}

/// An in-memory store, for tests and for probes that never need to outlive
/// the run.
#[derive(Debug, Default)]
pub struct MemoryStore {
    blobs: Mutex<HashMap<Hash, Vec<u8>>>,
}

impl MemoryStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many distinct blobs are held.
    pub fn len(&self) -> usize {
        self.blobs.lock().expect("store poisoned").len()
    }

    /// Whether anything is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl ContentStore for MemoryStore {
    fn put(&self, bytes: &[u8]) -> std::result::Result<Hash, String> {
        let hash = Hash::of(bytes);
        self.blobs.lock().expect("store poisoned").insert(hash, bytes.to_vec());
        Ok(hash)
    }

    fn get(&self, hash: &Hash) -> std::result::Result<Vec<u8>, String> {
        self.blobs
            .lock()
            .expect("store poisoned")
            .get(hash)
            .cloned()
            .ok_or_else(|| format!("{hash} is not in the store"))
    }

    fn contains(&self, hash: &Hash) -> bool {
        self.blobs.lock().expect("store poisoned").contains_key(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_roundtrips_and_deduplicates() {
        let store = MemoryStore::new();
        let a = store.put(b"content").unwrap();
        let b = store.put(b"content").unwrap();
        assert_eq!(a, b);
        assert_eq!(store.len(), 1);
        assert_eq!(store.get(&a).unwrap(), b"content");
        assert!(store.contains(&a));
    }

    #[test]
    fn missing_content_is_an_error_not_an_empty_file() {
        let store = MemoryStore::new();
        assert!(store.get(&Hash::of(b"absent")).is_err());
    }
}

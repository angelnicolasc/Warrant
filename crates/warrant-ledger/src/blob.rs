//! The content-addressed blob store.
//!
//! Payloads live here, keyed by their own BLAKE3 digest. Two consequences
//! follow and both are load-bearing for the design: identical payloads are
//! stored once, and a payload that has been altered on disk no longer
//! matches the address that names it, which is what makes tampering visible
//! rather than merely disallowed.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use warrant_core::Hash;

use crate::error::{LedgerError, Result};

/// A directory of content-addressed blobs.
#[derive(Clone, Debug)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// Open (creating if needed) a blob store rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| LedgerError::io("creating blob root", &root, e))?;
        Ok(BlobStore { root })
    }

    /// Where a given address is stored. Two-level fan-out keeps directory
    /// sizes tolerable for filesystems that degrade on very wide directories.
    pub fn path_for(&self, hash: &Hash) -> PathBuf {
        let hex = hash.to_hex();
        self.root.join(&hex[..2]).join(&hex[2..])
    }

    /// Store `bytes` and return its address.
    ///
    /// Writing is atomic: the payload is written to a temporary file in the
    /// same directory and renamed into place, so a reader never observes a
    /// half-written blob. Re-storing identical bytes is a no-op.
    pub fn put(&self, bytes: &[u8]) -> Result<Hash> {
        let hash = Hash::of(bytes);
        let final_path = self.path_for(&hash);
        if final_path.exists() {
            return Ok(hash);
        }

        let dir = final_path.parent().expect("blob paths always have a parent");
        fs::create_dir_all(dir).map_err(|e| LedgerError::io("creating blob shard", dir, e))?;

        // The temporary name includes the address, so concurrent writers of
        // the same payload race harmlessly onto the same final path.
        let tmp = dir.join(format!(".{}.tmp", hash.to_hex()));
        {
            let mut f = fs::File::create(&tmp)
                .map_err(|e| LedgerError::io("creating temp blob", &tmp, e))?;
            f.write_all(bytes).map_err(|e| LedgerError::io("writing temp blob", &tmp, e))?;
            f.sync_all().map_err(|e| LedgerError::io("syncing temp blob", &tmp, e))?;
        }
        match fs::rename(&tmp, &final_path) {
            Ok(()) => Ok(hash),
            // Another writer won the race and the content is identical by
            // construction, so this is success.
            Err(_) if final_path.exists() => {
                let _ = fs::remove_file(&tmp);
                Ok(hash)
            }
            Err(e) => Err(LedgerError::io("publishing blob", &final_path, e)),
        }
    }

    /// Read a blob, verifying that its content still matches its address.
    ///
    /// The verification is the point. A blob whose bytes were edited in place
    /// returns [`LedgerError::BlobCorrupt`] rather than the edited bytes.
    pub fn get(&self, hash: &Hash) -> Result<Vec<u8>> {
        let path = self.path_for(hash);
        let bytes = fs::read(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => LedgerError::BlobMissing { hash: *hash },
            _ => LedgerError::io("reading blob", &path, e),
        })?;
        let actual = Hash::of(&bytes);
        if actual != *hash {
            return Err(LedgerError::BlobCorrupt { expected: *hash, actual });
        }
        Ok(bytes)
    }

    /// Whether a blob is present. Does not verify its content.
    pub fn contains(&self, hash: &Hash) -> bool {
        self.path_for(hash).exists()
    }

    /// The store's root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, BlobStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::open(dir.path().join("blobs")).unwrap();
        (dir, store)
    }

    #[test]
    fn roundtrips() {
        let (_d, store) = store();
        let h = store.put(b"hello").unwrap();
        assert_eq!(store.get(&h).unwrap(), b"hello");
    }

    #[test]
    fn identical_payloads_are_stored_once() {
        let (_d, store) = store();
        let a = store.put(b"same").unwrap();
        let b = store.put(b"same").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn empty_payload_is_addressable() {
        let (_d, store) = store();
        let h = store.put(b"").unwrap();
        assert_eq!(store.get(&h).unwrap(), b"");
    }

    #[test]
    fn a_missing_blob_is_distinguishable_from_a_corrupt_one() {
        let (_d, store) = store();
        let absent = Hash::of(b"never stored");
        assert!(matches!(store.get(&absent), Err(LedgerError::BlobMissing { .. })));
    }

    /// Editing a blob in place — the move an agent with filesystem access
    /// would reach for — is detected on the next read.
    #[test]
    fn editing_a_blob_in_place_is_detected() {
        let (_d, store) = store();
        let h = store.put(b"the original payload").unwrap();
        fs::write(store.path_for(&h), b"the doctored payload").unwrap();

        match store.get(&h) {
            Err(LedgerError::BlobCorrupt { expected, actual }) => {
                assert_eq!(expected, h);
                assert_ne!(actual, h);
            }
            other => panic!("expected corruption to be detected, got {other:?}"),
        }
    }
}

//! The append-only log.
//!
//! There is no `remove`, no `update`, no `truncate` and no `rewind` on this
//! type, and no tool call reaches past it to the files underneath. That
//! absence is the second prohibition: within a session, no sequence of
//! actions removes evidence of earlier actions.
//!
//! A force-push rewrites a repository. It cannot unwrite this.

use std::fs;
use std::path::{Path, PathBuf};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, TableError};
use serde::Serialize;
use warrant_core::Hash;

use crate::blob::BlobStore;
use crate::entry::{Entry, EntryKind};
use crate::error::{LedgerError, Result};
use crate::projection::Projection;

/// The log table: sequence number to sealed entry header.
const ENTRIES: TableDefinition<u64, &[u8]> = TableDefinition::new("warrant.entries.v1");

/// Standard directory name for a ledger inside a repository.
pub const LEDGER_DIR: &str = ".warrant";

/// A head observed at some point in the past.
///
/// Recording one of these somewhere the agent cannot reach — a receipt, a CI
/// artefact, a colleague's terminal — is what makes tail truncation
/// detectable. Without an anchor, a shortened log verifies cleanly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Checkpoint {
    /// Sequence number of the head at that moment.
    pub seq: u64,
    /// Address of that entry.
    pub head: Hash,
}

/// An append-only, content-addressed record.
#[derive(Debug)]
pub struct Ledger {
    db: Database,
    blobs: BlobStore,
    root: PathBuf,
}

impl Ledger {
    /// Open, creating the ledger if it is not there.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| LedgerError::io("creating ledger root", &root, e))?;
        let blobs = BlobStore::open(root.join("blobs"))?;
        let db = Database::create(root.join("ledger.redb"))?;

        // Materialise the table so read transactions on an empty ledger do
        // not have to special-case its absence.
        let write = db.begin_write()?;
        write.open_table(ENTRIES)?;
        write.commit()?;

        Ok(Ledger { db, blobs, root })
    }

    /// Open the ledger belonging to a repository (`<repo>/.warrant`).
    pub fn open_for_repo(repo: impl AsRef<Path>) -> Result<Self> {
        Ledger::open(repo.as_ref().join(LEDGER_DIR))
    }

    /// Append a payload. This is the only way to extend the record.
    ///
    /// The payload is stored by content address first, then a hash-chained
    /// header committing to it is inserted. A crash between the two leaves an
    /// unreferenced blob, which is harmless; the reverse order would leave a
    /// header pointing at nothing, which is not.
    pub fn append(&self, kind: EntryKind, payload: &[u8], at_ms: u64) -> Result<Entry> {
        let payload_hash = self.blobs.put(payload)?;

        let write = self.db.begin_write()?;
        let entry = {
            let mut table = write.open_table(ENTRIES)?;
            let (seq, prev) = match table.last()? {
                Some((key, value)) => {
                    let last: Entry = serde_json::from_slice(value.value())?;
                    (key.value() + 1, last.hash)
                }
                None => (0, Hash::ZERO),
            };

            let entry = Entry::seal(seq, prev, at_ms, kind, payload_hash);
            let encoded = serde_json::to_vec(&entry)?;
            if table.insert(seq, encoded.as_slice())?.is_some() {
                // Unreachable while `seq` is derived from `last()`, but the
                // append-only property is worth asserting rather than assuming.
                return Err(LedgerError::Overwrite { seq });
            }
            entry
        };
        write.commit()?;
        Ok(entry)
    }

    /// Append a JSON-serialisable payload.
    pub fn append_json<T: Serialize>(
        &self,
        kind: EntryKind,
        value: &T,
        at_ms: u64,
    ) -> Result<Entry> {
        let bytes = serde_json::to_vec(value)?;
        self.append(kind, &bytes, at_ms)
    }

    /// Fetch one entry header.
    pub fn get(&self, seq: u64) -> Result<Option<Entry>> {
        let read = self.db.begin_read()?;
        let table = match read.open_table(ENTRIES) {
            Ok(t) => t,
            Err(TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        match table.get(seq)? {
            Some(value) => Ok(Some(serde_json::from_slice(value.value())?)),
            None => Ok(None),
        }
    }

    /// The most recent entry, if any.
    pub fn head(&self) -> Result<Option<Entry>> {
        let read = self.db.begin_read()?;
        let table = match read.open_table(ENTRIES) {
            Ok(t) => t,
            Err(TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        match table.last()? {
            Some((_, value)) => Ok(Some(serde_json::from_slice(value.value())?)),
            None => Ok(None),
        }
    }

    /// Number of entries.
    pub fn len(&self) -> Result<u64> {
        Ok(self.head()?.map_or(0, |e| e.seq + 1))
    }

    /// Whether anything has been recorded.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.head()?.is_none())
    }

    /// Every entry header, in order.
    pub fn entries(&self) -> Result<Vec<Entry>> {
        let read = self.db.begin_read()?;
        let table = match read.open_table(ENTRIES) {
            Ok(t) => t,
            Err(TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for row in table.iter()? {
            let (_, value) = row?;
            out.push(serde_json::from_slice(value.value())?);
        }
        Ok(out)
    }

    /// The payload bytes an entry points at, verified against its address.
    pub fn payload(&self, entry: &Entry) -> Result<Vec<u8>> {
        self.blobs.get(&entry.payload)
    }

    /// The payload, decoded as JSON.
    pub fn payload_json<T: serde::de::DeserializeOwned>(&self, entry: &Entry) -> Result<T> {
        let bytes = self.payload(entry)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Fold the log into a projection.
    pub fn project<P: Projection>(&self, mut projection: P) -> Result<P::Out> {
        for entry in self.entries()? {
            let payload = self.blobs.get(&entry.payload)?;
            projection.observe(&entry, &payload)?;
        }
        Ok(projection.finish())
    }

    /// Verify the hash chain: every entry self-consistent, every parent
    /// pointer correct, no gaps in the sequence.
    ///
    /// Returns the number of entries verified.
    pub fn verify(&self) -> Result<u64> {
        self.verify_inner(false)
    }

    /// Verify the chain *and* re-read every payload, checking that its bytes
    /// still hash to the address the log recorded.
    ///
    /// Costs one pass over all stored bytes. This is what catches a payload
    /// edited in place, as distinct from a header rewritten.
    pub fn verify_deep(&self) -> Result<u64> {
        self.verify_inner(true)
    }

    fn verify_inner(&self, check_payloads: bool) -> Result<u64> {
        let read = self.db.begin_read()?;
        let table = match read.open_table(ENTRIES) {
            Ok(t) => t,
            Err(TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(e) => return Err(e.into()),
        };

        let mut expected_seq = 0u64;
        let mut prev = Hash::ZERO;
        let mut count = 0u64;

        for row in table.iter()? {
            let (key, value) = row?;
            let key_seq = key.value();
            let entry: Entry = serde_json::from_slice(value.value())?;

            if key_seq != expected_seq {
                return Err(LedgerError::SequenceGap {
                    previous: expected_seq.saturating_sub(1),
                    found: key_seq,
                });
            }
            if entry.seq != key_seq {
                return Err(LedgerError::EntryForged {
                    seq: key_seq,
                    recorded: entry.hash,
                    computed: Entry::compute_hash(
                        key_seq,
                        &entry.prev,
                        entry.at_ms,
                        entry.kind,
                        &entry.payload,
                    ),
                });
            }
            if !entry.is_self_consistent() {
                return Err(LedgerError::EntryForged {
                    seq: entry.seq,
                    recorded: entry.hash,
                    computed: Entry::compute_hash(
                        entry.seq,
                        &entry.prev,
                        entry.at_ms,
                        entry.kind,
                        &entry.payload,
                    ),
                });
            }
            if entry.prev != prev {
                return Err(LedgerError::ChainBroken {
                    seq: entry.seq,
                    claimed: entry.prev,
                    actual: prev,
                });
            }
            if check_payloads {
                self.blobs.get(&entry.payload)?;
            }

            prev = entry.hash;
            expected_seq = key_seq + 1;
            count += 1;
        }

        Ok(count)
    }

    /// The current head, as a value that can be recorded elsewhere.
    pub fn checkpoint(&self) -> Result<Option<Checkpoint>> {
        Ok(self.head()?.map(|e| Checkpoint { seq: e.seq, head: e.hash }))
    }

    /// Whether this log still contains, unchanged, the history a previously
    /// observed checkpoint committed to.
    ///
    /// A hash chain detects insertion, reordering and modification on its
    /// own, because every entry commits to its predecessor. It cannot detect
    /// *truncation of the tail* — a shortened log is internally consistent.
    /// Closing that requires an anchor outside the log, which is what a
    /// checkpoint is: given any head observed earlier, this proves the
    /// current log extends it rather than replaces it.
    ///
    /// This is the same consistency-proof shape transparency logs use, and it
    /// is why receipts embed the head they were issued against.
    pub fn extends(&self, checkpoint: &Checkpoint) -> Result<bool> {
        match self.get(checkpoint.seq)? {
            Some(entry) => Ok(entry.hash == checkpoint.head),
            // The log is shorter than the checkpoint: entries were removed.
            None => Ok(false),
        }
    }

    /// Verify the chain and confirm it extends `checkpoint`.
    pub fn verify_from(&self, checkpoint: &Checkpoint) -> Result<u64> {
        let verified = self.verify_deep()?;
        if !self.extends(checkpoint)? {
            return Err(LedgerError::Truncated {
                expected_seq: checkpoint.seq,
                expected_head: checkpoint.head,
                present: verified,
            });
        }
        Ok(verified)
    }

    /// The blob store, for callers that need to address large artefacts
    /// directly rather than through an entry.
    pub fn blobs(&self) -> &BlobStore {
        &self.blobs
    }

    /// The ledger's root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::Replay;

    fn ledger() -> (tempfile::TempDir, Ledger) {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(dir.path().join(".warrant")).unwrap();
        (dir, ledger)
    }

    #[test]
    fn an_empty_ledger_is_valid() {
        let (_d, l) = ledger();
        assert_eq!(l.verify().unwrap(), 0);
        assert!(l.is_empty().unwrap());
        assert_eq!(l.head().unwrap(), None);
    }

    #[test]
    fn entries_chain_from_the_zero_address() {
        let (_d, l) = ledger();
        let first = l.append(EntryKind::RunStarted, b"one", 100).unwrap();
        let second = l.append(EntryKind::ToolCall, b"two", 200).unwrap();

        assert_eq!(first.seq, 0);
        assert_eq!(first.prev, Hash::ZERO);
        assert_eq!(second.seq, 1);
        assert_eq!(second.prev, first.hash);
        assert_eq!(l.verify_deep().unwrap(), 2);
    }

    #[test]
    fn identical_payloads_share_one_blob_but_get_distinct_entries() {
        let (_d, l) = ledger();
        let a = l.append(EntryKind::Note, b"same", 1).unwrap();
        let b = l.append(EntryKind::Note, b"same", 2).unwrap();
        assert_eq!(a.payload, b.payload, "payload storage is deduplicated");
        assert_ne!(a.hash, b.hash, "entries are still distinct");
    }

    #[test]
    fn the_log_survives_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".warrant");
        {
            let l = Ledger::open(&path).unwrap();
            l.append(EntryKind::RunStarted, b"before", 1).unwrap();
        }
        let l = Ledger::open(&path).unwrap();
        assert_eq!(l.len().unwrap(), 1);
        let next = l.append(EntryKind::RunFinished, b"after", 2).unwrap();
        assert_eq!(next.seq, 1);
        assert_eq!(l.verify_deep().unwrap(), 2);
    }

    #[test]
    fn replay_reproduces_every_payload_byte_for_byte() {
        let (_d, l) = ledger();
        let payloads: Vec<Vec<u8>> = vec![
            b"plain".to_vec(),
            vec![],
            vec![0u8, 255, 128, 7],
            "unicode: ✓ ⬒ ⚠".as_bytes().to_vec(),
            vec![0xAB; 100_000],
        ];
        for (i, p) in payloads.iter().enumerate() {
            l.append(EntryKind::ToolResult, p, 1000 + i as u64).unwrap();
        }

        let replayed = l.project(Replay::new()).unwrap();
        assert_eq!(replayed.len(), payloads.len());
        for (i, event) in replayed.iter().enumerate() {
            assert_eq!(event.payload, payloads[i], "payload {i} did not replay byte-identically");
            assert_eq!(event.entry.at_ms, 1000 + i as u64, "timestamps replay from the record");
        }
    }

    #[test]
    fn payload_json_roundtrips() {
        let (_d, l) = ledger();
        let value = serde_json::json!({ "tool": "exec", "argv": ["pytest", "-q"] });
        let entry = l.append_json(EntryKind::ToolCall, &value, 1).unwrap();
        let back: serde_json::Value = l.payload_json(&entry).unwrap();
        assert_eq!(back, value);
    }
}

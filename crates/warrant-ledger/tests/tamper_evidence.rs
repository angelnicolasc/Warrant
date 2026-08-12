//! The second prohibition, tested rather than asserted.
//!
//! These tests do not go through the [`Ledger`] API — that API has no delete
//! verb, so there is nothing to test there. They reach *underneath* it and
//! edit the storage directly, which is what an attacker with filesystem
//! access has, and check that every such edit is detected.
//!
//! The one move a hash chain cannot detect on its own is truncation of the
//! tail, and the last two tests pin down exactly where that line falls.

use redb::{Database, ReadableDatabase, TableDefinition};
use warrant_ledger::{Entry, EntryKind, Ledger, LedgerError};

const ENTRIES: TableDefinition<u64, &[u8]> = TableDefinition::new("warrant.entries.v1");

/// A ledger with a short history, plus the directory keeping it alive.
fn seeded() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".warrant");
    let ledger = Ledger::open(&path).unwrap();
    ledger.append(EntryKind::RunStarted, b"warrant wrap claude-code", 1_000).unwrap();
    ledger
        .append(EntryKind::ToolCall, br#"{"tool":"fs.write","path":"src/net/fetch.py"}"#, 2_000)
        .unwrap();
    ledger.append(EntryKind::ToolResult, b"wrote 41 lines", 3_000).unwrap();
    ledger.append(EntryKind::ModelResponse, b"I have reviewed the code.", 4_000).unwrap();
    ledger.append(EntryKind::RunFinished, b"ok", 5_000).unwrap();
    assert_eq!(ledger.verify_deep().unwrap(), 5);
    (dir, path)
}

/// Rewrite the raw row at `seq`, bypassing the public API entirely.
fn overwrite_row(path: &std::path::Path, seq: u64, bytes: &[u8]) {
    let db = Database::create(path.join("ledger.redb")).unwrap();
    let write = db.begin_write().unwrap();
    {
        let mut table = write.open_table(ENTRIES).unwrap();
        table.insert(seq, bytes).unwrap();
    }
    write.commit().unwrap();
}

fn remove_row(path: &std::path::Path, seq: u64) {
    let db = Database::create(path.join("ledger.redb")).unwrap();
    let write = db.begin_write().unwrap();
    {
        let mut table = write.open_table(ENTRIES).unwrap();
        table.remove(seq).unwrap();
    }
    write.commit().unwrap();
}

fn read_row(path: &std::path::Path, seq: u64) -> Entry {
    let db = Database::create(path.join("ledger.redb")).unwrap();
    let read = db.begin_read().unwrap();
    let table = read.open_table(ENTRIES).unwrap();
    let raw = table.get(seq).unwrap().unwrap();
    serde_json::from_slice(raw.value()).unwrap()
}

#[test]
fn removing_an_entry_from_the_middle_is_detected() {
    let (_d, path) = seeded();
    remove_row(&path, 2);

    let ledger = Ledger::open(&path).unwrap();
    match ledger.verify() {
        Err(LedgerError::SequenceGap { previous, found }) => {
            assert_eq!(previous, 1);
            assert_eq!(found, 3);
        }
        other => panic!("expected a sequence gap, got {other:?}"),
    }
}

#[test]
fn relabelling_an_entry_in_place_is_detected() {
    let (_d, path) = seeded();

    // Change what the entry claims to be, leaving its recorded address alone.
    let mut entry = read_row(&path, 3);
    entry.kind = EntryKind::Note;
    overwrite_row(&path, 3, &serde_json::to_vec(&entry).unwrap());

    let ledger = Ledger::open(&path).unwrap();
    match ledger.verify() {
        Err(LedgerError::EntryForged { seq, .. }) => assert_eq!(seq, 3),
        other => panic!("expected a forged entry, got {other:?}"),
    }
}

#[test]
fn resealing_an_entry_honestly_still_breaks_the_chain_after_it() {
    let (_d, path) = seeded();

    // The thorough version of the previous attack: rewrite the entry *and*
    // recompute its address so it is internally consistent. The chain still
    // catches it, because entry 4 commits to entry 3's original address.
    let original = read_row(&path, 3);
    let forged =
        Entry::seal(original.seq, original.prev, original.at_ms, EntryKind::Note, original.payload);
    assert!(forged.is_self_consistent());
    overwrite_row(&path, 3, &serde_json::to_vec(&forged).unwrap());

    let ledger = Ledger::open(&path).unwrap();
    match ledger.verify() {
        Err(LedgerError::ChainBroken { seq, .. }) => {
            assert_eq!(seq, 4, "the successor is where a resealed entry surfaces");
        }
        other => panic!("expected a broken chain, got {other:?}"),
    }
}

#[test]
fn editing_a_payload_on_disk_is_detected_by_the_deep_check() {
    let (_d, path) = seeded();

    let ledger = Ledger::open(&path).unwrap();
    let entry = ledger.get(3).unwrap().unwrap();
    let blob_path = ledger.blobs().path_for(&entry.payload);
    std::fs::write(&blob_path, b"I did not review the code.").unwrap();

    // The header chain is untouched, so the cheap check still passes...
    assert_eq!(ledger.verify().unwrap(), 5);
    // ...and the deep check is what catches it.
    match ledger.verify_deep() {
        Err(LedgerError::BlobCorrupt { expected, .. }) => assert_eq!(expected, entry.payload),
        other => panic!("expected blob corruption, got {other:?}"),
    }
}

#[test]
fn truncating_the_tail_leaves_a_self_consistent_log() {
    let (_d, path) = seeded();
    remove_row(&path, 4);
    remove_row(&path, 3);

    let ledger = Ledger::open(&path).unwrap();
    assert_eq!(
        ledger.verify_deep().unwrap(),
        3,
        "a hash chain alone cannot see that a tail was cut off — this is the honest limit"
    );
}

#[test]
fn truncating_the_tail_is_detected_against_a_checkpoint() {
    let (_d, path) = seeded();

    // Anything that observed the log earlier — a receipt, a CI artefact —
    // holds a checkpoint.
    let checkpoint = {
        let ledger = Ledger::open(&path).unwrap();
        ledger.checkpoint().unwrap().unwrap()
    };
    assert_eq!(checkpoint.seq, 4);

    remove_row(&path, 4);
    remove_row(&path, 3);

    let ledger = Ledger::open(&path).unwrap();
    assert!(!ledger.extends(&checkpoint).unwrap());
    match ledger.verify_from(&checkpoint) {
        Err(LedgerError::Truncated { expected_seq, present, .. }) => {
            assert_eq!(expected_seq, 4);
            assert_eq!(present, 3);
        }
        other => panic!("expected truncation to be detected, got {other:?}"),
    }
}

#[test]
fn an_honestly_extended_log_still_satisfies_an_older_checkpoint() {
    let (_d, path) = seeded();
    let ledger = Ledger::open(&path).unwrap();
    let checkpoint = ledger.checkpoint().unwrap().unwrap();

    ledger.append(EntryKind::Note, b"more work", 6_000).unwrap();
    ledger.append(EntryKind::Note, b"and more", 7_000).unwrap();

    assert_eq!(ledger.verify_from(&checkpoint).unwrap(), 7);
}

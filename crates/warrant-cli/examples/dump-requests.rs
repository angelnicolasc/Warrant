//! Print the model requests a ledger recorded.
//!
//! A debugging aid for replay divergence: when a replay is told it is being
//! asked a different question, this is how you find out which part changed.
//!
//! ```text
//! cargo run --example dump-requests -- <ledger-dir> [index]
//! ```

use warrant_ledger::{EntryKind, Ledger};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or("usage: dump-requests <ledger-dir> [index]")?;
    let wanted: Option<usize> = args.next().and_then(|n| n.parse().ok());

    let ledger = Ledger::open(&path)?;
    let mut index = 0usize;

    for entry in ledger.entries()? {
        if entry.kind != EntryKind::ModelRequest {
            continue;
        }
        if wanted.is_none_or(|w| w == index) {
            let value: serde_json::Value = ledger.payload_json(&entry)?;
            println!("=== request {index} (entry {}) ===", entry.seq);
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        index += 1;
    }
    Ok(())
}

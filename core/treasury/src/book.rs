//! Where the treasury writes down what it has done: one JSON line per change, appended and never rewritten.
//!
//! A line is written before anything is done that depends on it, so a treasury that stops between two lines starts
//! again from the first and repeats only what is safe to repeat: every command it sends carries a command ID fixed by
//! the transfer, which the sequencer group deduplicates (spec E-7.1).

use serde_json::Value;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

pub struct Book {
    file: File,
    fsync: bool,
}

impl Book {
    /// Opens the file, creating it if there is none, and returns what it holds. A last line cut short by a stop
    /// mid-write never happened: it is cut off, so the next line starts cleanly. A damaged line anywhere else is an
    /// error, because nothing after it could be trusted.
    pub fn open(path: &Path, fsync: bool) -> Result<(Self, Vec<Value>), String> {
        let describe = |e: std::io::Error| format!("{}: {e}", path.display());
        let mut file = OpenOptions::new().read(true).append(true).create(true).open(path).map_err(describe)?;
        let mut text = Vec::new();
        file.read_to_end(&mut text).map_err(describe)?;
        let whole = text.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        if whole < text.len() {
            eprintln!("treasury: {}: dropping a last line cut short ({} bytes)", path.display(), text.len() - whole);
            file.set_len(whole as u64).map_err(describe)?;
        }
        let mut records = Vec::new();
        for (n, line) in text[..whole].split(|b| *b == b'\n').enumerate().filter(|(_, l)| !l.is_empty()) {
            let record = serde_json::from_slice(line).map_err(|e| format!("{} line {}: {e}", path.display(), n + 1))?;
            records.push(record);
        }
        Ok((Self { file, fsync }, records))
    }

    /// Appends one record, and returns only once it is written (and synced, if asked for).
    pub fn append(&mut self, record: &Value) -> Result<(), String> {
        let mut line = record.to_string();
        line.push('\n');
        self.file.write_all(line.as_bytes()).map_err(|e| format!("writing the ledger: {e}"))?;
        if self.fsync {
            self.file.sync_data().map_err(|e| format!("syncing the ledger: {e}"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Book;
    use serde_json::json;
    use std::io::Write;

    #[test]
    fn a_line_cut_short_is_dropped_and_the_next_one_starts_cleanly() {
        let path = std::env::temp_dir().join(format!("venue-treasury-book-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let (mut book, records) = Book::open(&path, false).expect("a new book");
            assert!(records.is_empty());
            book.append(&json!({"n": 1})).expect("written");
        }
        std::fs::OpenOptions::new().append(true).open(&path).expect("reopened").write_all(b"{\"n\": 2").expect("cut");
        {
            let (mut book, records) = Book::open(&path, false).expect("reopened");
            assert_eq!(records, vec![json!({"n": 1})]);
            book.append(&json!({"n": 3})).expect("written");
        }
        let (_, records) = Book::open(&path, false).expect("reopened again");
        assert_eq!(records, vec![json!({"n": 1}), json!({"n": 3})]);
        let _ = std::fs::remove_file(&path);
    }
}

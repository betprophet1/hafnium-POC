//! `venue-view`: takes both halves of a snapshot — the engine's, for a node, and a reader's, for everything
//! that follows one. See `core/link/src/pair.rs` for why a reader cannot be restored from the engine's half.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use venue_link::pair;
use venue_link::wire::Error;

const USAGE: &str = "usage:
  venue-view snapshot JOURNAL DIR [--at N] [--results FILE]
                       writes DIR/engine.snapshot and DIR/view.json, both taken at the same sequence: the
                       engine's state, for a node restored from it, and a reader's view, for a subscriber that
                       would otherwise ask a founded journal for a backlog it does not have.
                       --at takes the state after input N, rather than after the last input.
                       --results names an existing result log to pair the journal with; without it the journal
                       is replayed to produce one, which a journal `cut` has shortened cannot be.
  venue-view sequence DIR
                       prints the sequence a pair was taken at, refusing one whose halves disagree.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let outcome = match (args.first().map(String::as_str), args.len()) {
        (Some("snapshot"), 3 | 5 | 7) => snapshot(&args[1..]),
        (Some("sequence"), 2) => sequence(Path::new(&args[1])),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(64);
        }
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("venue-view: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `snapshot JOURNAL DIR [--at N] [--results FILE]`, in either flag order.
fn snapshot(args: &[String]) -> Result<(), Error> {
    let (mut at, mut results) = (None, None);
    let mut rest = args[2..].iter();
    while let Some(flag) = rest.next() {
        let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--at" => at = Some(value.parse::<u64>().map_err(|e| format!("--at: {e}"))?),
            "--results" => results = Some(PathBuf::from(value)),
            other => return Err(format!("unknown argument {other}\n{USAGE}").into()),
        }
    }
    let dir = Path::new(&args[1]);
    let taken = pair::take(Path::new(&args[0]), dir, at, results.as_deref())?;
    eprintln!("both halves taken at sequence {taken}: {}", dir.display());
    Ok(())
}

fn sequence(dir: &Path) -> Result<(), Error> {
    println!("{}", pair::sequence_of(dir)?);
    Ok(())
}

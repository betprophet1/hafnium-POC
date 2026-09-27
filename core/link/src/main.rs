//! `venue-view`: takes both halves of a snapshot — the engine's, for a node, and a reader's, for everything
//! that follows one. See `core/link/src/pair.rs` for why a reader cannot be restored from the engine's half.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use venue_link::pair;
use venue_link::wire::Error;

const USAGE: &str = "usage:
  venue-view snapshot JOURNAL DIR [--at N] [--results FILE] [--from-pair DIR]
                       writes DIR/engine.snapshot and DIR/view.json, both taken at the same sequence: the
                       engine's state, for a node restored from it, and a reader's view, for a subscriber that
                       would otherwise ask a founded journal for a backlog it does not have.
                       --at takes the state after input N, rather than after the last input. `--at trusted`
                       takes it after the last input the result log is *trusted* through, which is what a
                       scheduled pair wants: it is the highest sequence both halves can be built at, and it
                       needs nobody to know a sequence number in advance.
                       --results names an existing result log to pair the journal with; without it the journal
                       is replayed to produce one, which a journal `cut` has shortened cannot be.
                       --from-pair names the previously published pair to carry on from, which a journal that has
                       been cut *requires*: the units before the cut are in the archive, so a view built without
                       it would silently hold only what followed.
  venue-view sequence DIR
                       prints the sequence a pair was taken at, refusing one whose halves disagree.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let outcome = match (args.first().map(String::as_str), args.len()) {
        (Some("snapshot"), 3 | 5 | 7 | 9) => snapshot(&args[1..]),
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
    let (mut at, mut results, mut previous) = (None, None, None);
    let mut rest = args[2..].iter();
    while let Some(flag) = rest.next() {
        let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            // `trusted` rather than a number: the result log's last confirmed group. The engine half could be
            // taken later than that, but the view half could not — `View::replayed` needs results to pair each
            // input with — so this is the furthest a *pair* can be taken, and asking for more is an error the
            // job would only hit at runtime.
            "--at" if value == "trusted" => at = Some(trusted(args)?),
            "--at" => at = Some(value.parse::<u64>().map_err(|e| format!("--at: {e}"))?),
            "--results" => results = Some(PathBuf::from(value)),
            "--from-pair" => previous = Some(PathBuf::from(value).join(pair::VIEW)),
            other => return Err(format!("unknown argument {other}\n{USAGE}").into()),
        }
    }
    let dir = Path::new(&args[1]);
    let taken = pair::take_after(previous.as_deref(), Path::new(&args[0]), dir, at, results.as_deref())?;
    eprintln!("both halves taken at sequence {taken}: {}", dir.display());
    Ok(())
}

/// The sequence the result log is trusted through, for `--at trusted`. The result log has to be named: a
/// journal the tool would have to replay to produce one is a journal whose trusted point is its own end.
fn trusted(args: &[String]) -> Result<u64, Error> {
    let results = args[2..]
        .windows(2)
        .find(|pair| pair[0] == "--results")
        .map(|pair| PathBuf::from(&pair[1]))
        .ok_or("--at trusted needs --results: the trusted point is a property of the result log")?;
    match venue_node::node::trusted_results(&results)? {
        Some((sequence, _)) => Ok(sequence),
        None => Err(format!("{} has nothing trusted yet, so no pair can be taken", results.display()).into()),
    }
}

fn sequence(dir: &Path) -> Result<(), Error> {
    println!("{}", pair::sequence_of(dir)?);
    Ok(())
}

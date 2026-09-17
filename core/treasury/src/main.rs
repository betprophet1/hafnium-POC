//! `venue-treasury`: the mini venue's dummy treasury. See `core/treasury/README.md`.

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::process::ExitCode;
use std::thread;
use venue_treasury::bank::FakeBank;
use venue_treasury::faults::Faults;
use venue_treasury::recon::Options;
use venue_treasury::Settings;

const USAGE: &str = "usage:
  venue-treasury --ingress ADDR --feed ADDR --catalogue FILE --ledger FILE [--listen 127.0.0.1:8083] [--fsync false]
                 [--seed 1] [--min-delay-ms 2000] [--max-delay-ms 8000] [--failure-per-mille 100]
                 [--max-cents 100000000] [--reconcile-every-ms 60000] [--reconcile-grace-ms 10000]
                 [--faults none|demo] [--lost-notice-per-mille N] [--late-notice-per-mille N] [--late-notice-ms N]
                 [--duplicate-notice-per-mille N] [--lost-answer-per-mille N] [--lost-command-per-mille N]

The fake bank decides dummy_bank transfers after a delay drawn between the two delays, failing the given share of
them; its decisions depend only on the seed and the transfer. It keeps its own book beside the ledger (FILE with the
extension bank.jsonl).

Reconciliation runs every --reconcile-every-ms (0: only when asked) against the bank's statement and the journal.
--faults loses messages on purpose so it has something to find: none (the default) or demo, a few percent of each
kind; each per-mille flag overrides the profile's share. Every flag can also be given as VENUE_<FLAG>, for example
VENUE_LEDGER.";

struct Args(BTreeMap<String, String>);

impl Args {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut values = BTreeMap::new();
        let mut it = args.iter();
        while let Some(flag) = it.next() {
            let key = flag.strip_prefix("--").ok_or_else(|| format!("unexpected argument {flag}\n{USAGE}"))?;
            let value = it.next().ok_or_else(|| format!("{flag} needs a value"))?;
            values.insert(key.to_string(), value.clone());
        }
        Ok(Self(values))
    }

    /// A flag, else `VENUE_<FLAG>` from the environment, else the default.
    fn get(&self, key: &str, default: Option<&str>) -> Result<String, String> {
        if let Some(v) = self.0.get(key) {
            return Ok(v.clone());
        }
        let env = format!("VENUE_{}", key.to_uppercase().replace('-', "_"));
        std::env::var(&env)
            .ok()
            .or_else(|| default.map(str::to_string))
            .ok_or_else(|| format!("--{key} (or {env}) is required"))
    }

    fn number(&self, key: &str, default: u64) -> Result<u64, String> {
        let text = self.get(key, Some(default.to_string().as_str()))?;
        text.parse().map_err(|_| format!("--{key} must be a whole number, not {text}"))
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match Args::parse(&args).and_then(|a| run(&a)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("venue-treasury: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<(), String> {
    let bank = FakeBank {
        seed: args.number("seed", 1)?,
        min_delay_ms: args.number("min-delay-ms", 2000)?,
        max_delay_ms: args.number("max-delay-ms", 8000)?,
        failure_per_mille: args.number("failure-per-mille", 100)?.min(1000),
    };
    if bank.max_delay_ms < bank.min_delay_ms {
        return Err("--max-delay-ms must not be less than --min-delay-ms".into());
    }
    let profile = args.get("faults", Some("none"))?;
    let base = Faults::profile(&profile).ok_or_else(|| format!("--faults must be none or demo, not {profile}"))?;
    let faults = Faults {
        lost_notice_per_mille: args.number("lost-notice-per-mille", base.lost_notice_per_mille)?,
        late_notice_per_mille: args.number("late-notice-per-mille", base.late_notice_per_mille)?,
        late_notice_ms: args.number("late-notice-ms", base.late_notice_ms)?,
        duplicate_notice_per_mille: args.number("duplicate-notice-per-mille", base.duplicate_notice_per_mille)?,
        lost_answer_per_mille: args.number("lost-answer-per-mille", base.lost_answer_per_mille)?,
        lost_command_per_mille: args.number("lost-command-per-mille", base.lost_command_per_mille)?,
    };
    let notices = [faults.lost_notice_per_mille, faults.late_notice_per_mille, faults.duplicate_notice_per_mille];
    let commands = [faults.lost_answer_per_mille, faults.lost_command_per_mille];
    let share = |shares: &[u64]| shares.iter().fold(0u64, |sum, s| sum.saturating_add(*s));
    if share(&notices) > 1000 || share(&commands) > 1000 {
        return Err("the notice losses, and the command losses, must each add up to at most 1000 per mille".into());
    }
    let recon = Options {
        faults,
        every_ms: args.number("reconcile-every-ms", 60_000)?,
        grace_ms: args.number("reconcile-grace-ms", 10_000)?,
    };
    let settings = Settings {
        ingress: args.get("ingress", None)?,
        feed: args.get("feed", None)?,
        catalogue: args.get("catalogue", None)?,
        ledger: args.get("ledger", None)?.into(),
        fsync: args.get("fsync", Some("false"))? == "true",
        bank,
        max_cents: args.number("max-cents", 100_000_000)?,
        recon,
    };
    let listen = args.get("listen", Some("127.0.0.1:8083"))?;
    let listener = TcpListener::bind(&listen).map_err(|e| format!("{listen}: {e}"))?;
    eprintln!("treasury on {listen}, ledger {}", settings.ledger.display());
    if faults.any() {
        eprintln!("treasury: injecting lost messages: {}", faults.to_json());
    }
    venue_treasury::start(&settings, listener)?;
    loop {
        thread::park();
    }
}

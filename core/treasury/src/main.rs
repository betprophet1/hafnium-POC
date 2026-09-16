//! `venue-treasury`: the mini venue's dummy treasury. See `core/treasury/README.md`.

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::process::ExitCode;
use std::thread;
use venue_treasury::bank::FakeBank;
use venue_treasury::Settings;

const USAGE: &str = "usage:
  venue-treasury --ingress ADDR --feed ADDR --catalogue FILE --ledger FILE [--listen 127.0.0.1:8083] [--fsync false]
                 [--seed 1] [--min-delay-ms 2000] [--max-delay-ms 8000] [--failure-per-mille 100]
                 [--max-cents 100000000]

The fake bank decides dummy_bank transfers after a delay drawn between the two delays, failing the given share of
them; its decisions depend only on the seed and the transfer. Every flag can also be given as VENUE_<FLAG>, for
example VENUE_LEDGER.";

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
    let settings = Settings {
        ingress: args.get("ingress", None)?,
        feed: args.get("feed", None)?,
        catalogue: args.get("catalogue", None)?,
        ledger: args.get("ledger", None)?.into(),
        fsync: args.get("fsync", Some("false"))? == "true",
        bank,
        max_cents: args.number("max-cents", 100_000_000)?,
    };
    let listen = args.get("listen", Some("127.0.0.1:8083"))?;
    let listener = TcpListener::bind(&listen).map_err(|e| format!("{listen}: {e}"))?;
    eprintln!("treasury on {listen}, ledger {}", settings.ledger.display());
    venue_treasury::start(&settings, listener)?;
    loop {
        thread::park();
    }
}

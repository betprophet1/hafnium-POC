//! `venue-treasury`: the mini venue's dummy treasury. See `core/treasury/README.md`.

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::Path;
use std::process::ExitCode;
use std::thread;
use venue_store::Database;
use venue_treasury::bank::FakeBank;
use venue_treasury::faults::Faults;
use venue_treasury::recon::Options;
use venue_treasury::{postgres, Settings, Storage};

const USAGE: &str = "usage:
  venue-treasury [serve] --ingress ADDR --feed ADDR --catalogue FILE --database-url URL [--listen 127.0.0.1:8083]
                 [--migrate-on-start false] [--seed 1] [--min-delay-ms 2000] [--max-delay-ms 8000]
                 [--failure-per-mille 100] [--max-cents 100000000] [--reconcile-every-ms 60000]
                 [--reconcile-grace-ms 10000] [--faults none|demo] [--lost-notice-per-mille N]
                 [--late-notice-per-mille N] [--late-notice-ms N] [--duplicate-notice-per-mille N]
                 [--lost-answer-per-mille N] [--lost-command-per-mille N]
  venue-treasury migrate --database-url URL
  venue-treasury import --ledger FILE --database-url URL

The treasury's records (the ledger, transfers, the fake bank's book, reconciliation) are kept in PostgreSQL:
--database-url postgres://USER:PASSWORD@HOST:5432/DATABASE, with --database-schema (default treasury),
--database-pool-size (4), --database-tls disable|prefer|require|verify-ca|verify-full and --database-ca-file
(core/store/README.md). serve waits for the database and for its schema to be at this build's migration, which
migrate applies (--migrate-on-start true applies it at start instead, for laptops). import moves a book kept by an
earlier version in JSON-lines files (FILE and the bank's FILE with the extension bank.jsonl) into empty records.

The fake bank decides dummy_bank transfers after a delay drawn between the two delays, failing the given share of
them; its decisions depend only on the seed and the transfer.

Reconciliation runs every --reconcile-every-ms (0: only when asked) against the bank's statement and the journal.
--faults loses messages on purpose so it has something to find: none (the default) or demo, a few percent of each
kind; each per-mille flag overrides the profile's share. Every flag can also be given as VENUE_<FLAG>, for example
VENUE_DATABASE_URL.";

struct Args(BTreeMap<String, String>);

fn env_name(key: &str) -> String {
    format!("VENUE_{}", key.to_uppercase().replace('-', "_"))
}

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

    /// A flag, else `VENUE_<FLAG>` from the environment.
    fn lookup(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned().or_else(|| std::env::var(env_name(key)).ok())
    }

    /// A flag, else `VENUE_<FLAG>` from the environment, else the default.
    fn get(&self, key: &str, default: Option<&str>) -> Result<String, String> {
        self.lookup(key)
            .or_else(|| default.map(str::to_string))
            .ok_or_else(|| format!("--{key} (or {}) is required", env_name(key)))
    }

    fn number(&self, key: &str, default: u64) -> Result<u64, String> {
        let text = self.get(key, Some(default.to_string().as_str()))?;
        text.parse().map_err(|_| format!("--{key} must be a whole number, not {text}"))
    }

    fn database(&self) -> Result<venue_store::Config, String> {
        venue_store::Config::from_settings(|k| self.lookup(k), "venue-treasury", "treasury")?.ok_or_else(|| {
            "--database-url (or VENUE_DATABASE_URL) is required: the treasury keeps its records in PostgreSQL".into()
        })
    }
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let command = match args.first().map(String::as_str) {
        Some(c @ ("serve" | "migrate" | "import")) => {
            let c = c.to_string();
            args.remove(0);
            c
        }
        _ => "serve".to_string(),
    };
    let outcome = Args::parse(&args).and_then(|a| match command.as_str() {
        "migrate" => migrate(&a),
        "import" => import(&a),
        _ => run(&a),
    });
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("venue-treasury: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Applies the treasury's migrations and exits: the chart's migrate job, or an operator.
fn migrate(args: &Args) -> Result<(), String> {
    let config = args.database()?;
    let db = Database::connect_retrying(&config, "venue-treasury")?;
    let applied = db.migrate(&venue_store::migrations_dir!())?;
    eprintln!(
        "venue-treasury: schema {} of {} is at migration {} (applied now: {applied:?})",
        config.schema,
        config.describe(),
        postgres::MIGRATIONS.len()
    );
    Ok(())
}

/// Moves a book from the JSON-lines files of earlier versions into the records.
fn import(args: &Args) -> Result<(), String> {
    let ledger = args.get("ledger", None)?;
    let config = args.database()?;
    let db = Database::connect_retrying(&config, "venue-treasury")?;
    db.wait_for_schema(postgres::MIGRATIONS, "venue-treasury")?;
    let records = postgres::Postgres::new(db);
    let imported = venue_treasury::import::import(Path::new(&ledger), &records)?;
    eprintln!("venue-treasury: imported {ledger} into {}: {imported:?}", config.describe());
    Ok(())
}

fn run(args: &Args) -> Result<(), String> {
    if args.lookup("ledger").is_some() {
        return Err(
            "the treasury no longer keeps a ledger file: its records are in PostgreSQL (--database-url); move \
                    an existing book with venue-treasury import --ledger FILE"
                .into(),
        );
    }
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
        bank,
        max_cents: args.number("max-cents", 100_000_000)?,
        recon,
    };
    let migrate = match args.get("migrate-on-start", Some("false"))?.as_str() {
        "true" => true,
        "false" => false,
        other => return Err(format!("--migrate-on-start must be true or false, not {other}")),
    };
    let config = args.database()?;
    eprintln!("treasury: records in {}", config.describe());
    // The database first: until it answers, nothing listens, so a gateway is refused at once rather than left waiting.
    let records = venue_treasury::records(&Storage::Postgres { config, migrate })?;
    let listen = args.get("listen", Some("127.0.0.1:8083"))?;
    let listener = TcpListener::bind(&listen).map_err(|e| format!("{listen}: {e}"))?;
    eprintln!("treasury on {listen}");
    if faults.any() {
        eprintln!("treasury: injecting lost messages: {}", faults.to_json());
    }
    venue_treasury::start(&settings, records, listener)?;
    loop {
        thread::park();
    }
}

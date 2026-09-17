//! `venue-parlays`: the parlay service. See `venue_parlays` and `interfaces/gateway/README.md` (parlays).

use std::collections::BTreeMap;
use std::process::ExitCode;
use std::thread;
use venue_link::catalogue::Catalogue;
use venue_link::wire::Error;
use venue_parlays::service::{self, Config};
use venue_parlays::{operator, responders, rules};
use venue_store::Database;

const USAGE: &str = "usage:
  venue-parlays [serve] --ingress ADDR --feed ADDR --catalogue FILE --database-url URL [--listen 127.0.0.1:7103]
                [--window-seconds 30] [--max-parlays 100] [--capacity small] [--operator 127.0.0.1:7104]
                [--min-quantity 10] [--migrate-on-start false] [--store postgres|memory]
  venue-parlays migrate --database-url URL
  venue-parlays import --responders FILE --database-url URL

Lists parlays when they are first requested, relays requests for quote to the subscribed sessions of approved
responders and their quotes to the requester through the gateways that connect to --listen, and settles every parlay
from its legs. It uses the core's ingress as an operator. The approved responders are kept in PostgreSQL
(--database-url postgres://USER:PASSWORD@HOST:5432/DATABASE, --database-schema parlays, --database-tls,
--database-ca-file: core/store/README.md), started from the catalogue's \"responder\": true accounts the first time,
and are managed through the operator API on --operator (GET /v1/responders, PUT and DELETE /v1/responders/ACCOUNT);
--operator none serves no operator API. serve waits for its schema, which migrate applies (--migrate-on-start true
applies it at start); --store memory keeps approvals in memory only, for throwaway local venues. import starts the
list from a responder file kept by an earlier version. A request is for at least --min-quantity contracts, a multiple
of the parlay lot step (10). --capacity must name the node's capacity profile: no parlay is listed that would take the
venue past 95% of its events, markets or price levels. Settled parlays are released by the engine a retention after
settlement, which makes room again. Every flag can also be given as VENUE_<FLAG>, for example VENUE_FEED.";

struct Args(BTreeMap<String, String>);

impl Args {
    fn parse(args: &[String]) -> Result<Self, Error> {
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
    fn get(&self, key: &str, default: Option<&str>) -> Result<String, Error> {
        if let Some(v) = self.0.get(key) {
            return Ok(v.clone());
        }
        let env = format!("VENUE_{}", key.to_uppercase().replace('-', "_"));
        std::env::var(&env)
            .ok()
            .or_else(|| default.map(str::to_string))
            .ok_or_else(|| Error::from(format!("--{key} (or {env}) is required")))
    }

    /// A flag, else `VENUE_<FLAG>` from the environment, if either is given.
    fn lookup(&self, key: &str) -> Option<String> {
        self.get(key, None).ok()
    }

    fn database(&self) -> Result<venue_store::Config, Error> {
        venue_store::Config::from_settings(|k| self.lookup(k), "venue-parlays", "parlays")?.ok_or_else(|| {
            Error::from(
                "--database-url (or VENUE_DATABASE_URL) is required: approved responders are kept in PostgreSQL",
            )
        })
    }

    fn number(&self, key: &str, default: u64) -> Result<u64, Error> {
        Ok(self.get(key, Some(default.to_string().as_str()))?.parse()?)
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
            eprintln!("venue-parlays: {e}");
            ExitCode::FAILURE
        }
    }
}

fn migrate(args: &Args) -> Result<(), Error> {
    let config = args.database()?;
    let db = Database::connect_retrying(&config, "venue-parlays")?;
    let applied = db.migrate(&venue_store::migrations_dir!())?;
    eprintln!(
        "venue-parlays: schema {} of {} is at migration {} (applied now: {applied:?})",
        config.schema,
        config.describe(),
        responders::MIGRATIONS.len()
    );
    Ok(())
}

fn import(args: &Args) -> Result<(), Error> {
    let file = args.get("responders", None)?;
    let config = args.database()?;
    let db = Database::connect_retrying(&config, "venue-parlays")?;
    db.wait_for_schema(responders::MIGRATIONS, "venue-parlays")?;
    let accounts = responders::import(&db, std::path::Path::new(&file))?;
    eprintln!("venue-parlays: the approved responders are now {accounts:?}, from {file}");
    Ok(())
}

fn run(args: &Args) -> Result<(), Error> {
    let catalogue = Catalogue::load(&args.get("catalogue", None)?)?;
    if args.0.contains_key("responders") {
        return Err(
            "approved responders are no longer kept in a file: they are in PostgreSQL (--database-url); move a \
                    file's list with venue-parlays import --responders FILE"
                .into(),
        );
    }
    let responders = match args.get("store", Some("postgres"))?.as_str() {
        "postgres" => {
            let config = args.database()?;
            eprintln!("parlay service: approved responders in {}", config.describe());
            let db = Database::connect_retrying(&config, "venue-parlays")?;
            if args.get("migrate-on-start", Some("false"))? == "true" {
                db.migrate(&venue_store::migrations_dir!())?;
            }
            db.wait_for_schema(responders::MIGRATIONS, "venue-parlays")?;
            Some(db)
        }
        "memory" => {
            eprintln!("parlay service: approvals kept in memory only (--store memory): they revert when this stops");
            None
        }
        other => return Err(format!("--store must be postgres or memory, not {other}").into()),
    };
    let config = Config {
        ingress: args.get("ingress", None)?,
        feed: args.get("feed", None)?,
        listen: args.get("listen", Some("127.0.0.1:7103"))?,
        window_seconds: args.number("window-seconds", 30)?,
        max_parlays: usize::try_from(args.number("max-parlays", 100)?)?,
        capacity: {
            let profile = venue_link::catalogue::capacity_profile(&args.get("capacity", Some("small"))?)?;
            rules::Capacity { events: profile.events, instruments: profile.instruments, levels: profile.levels }
        },
        responders,
        min_quantity: args.number("min-quantity", rules::LOT_STEP)?,
    };
    let service = service::start(catalogue, &config)?;
    let operator = args.get("operator", Some("127.0.0.1:7104"))?;
    if operator != "none" {
        let listener = std::net::TcpListener::bind(&operator)?;
        eprintln!("parlay service: operator API on {}", listener.local_addr()?);
        operator::serve(listener, service);
    }
    loop {
        thread::park();
    }
}

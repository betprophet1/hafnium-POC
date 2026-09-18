//! `venue-identity`: the identity service of test environments. See `core/identity/README.md`.

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::Path;
use std::process::ExitCode;
use std::thread;
use venue_identity::{records, Settings, Storage};
use venue_link::catalogue::Catalogue;
use venue_store::Database;

const USAGE: &str = "usage:
  venue-identity [serve] --catalogue FILE --database-url URL [--listen 127.0.0.1:8084] [--migrate-on-start false]
  venue-identity [serve] --catalogue FILE --store memory [--listen 127.0.0.1:8084] [--show-mail false]
  venue-identity migrate --database-url URL
  venue-identity import --file FILE --catalogue FILE --database-url URL
  venue-identity staff --email ADDRESS [--on true] --database-url URL

Keeps the catalogue's accounts and their API keys in PostgreSQL, and answers the gateways' questions about keys.
--database-url postgres://USER:PASSWORD@HOST:5432/DATABASE, with --database-schema (default identity),
--database-pool-size (4), --database-tls disable|prefer|require|verify-ca|verify-full and --database-ca-file
(core/store/README.md). serve waits for the database and for its schema to be at this build's migration, which
migrate applies (--migrate-on-start true applies it at start instead). --store memory keeps keys in memory only, for
tests and throwaway local venues. import moves a key store kept by an earlier version in a JSON-lines file into
empty records. staff says a member is one of the venue's own people, which decides only which environments the
bootstrap will name to them (interfaces/gateway/README.md, choosing an environment) and is never a way into one.
--show-mail true answers GET /v1/mail with the mail this environment would have sent, which a demo
needs and a venue with real addresses must never turn on: the messages hold the codes. Every flag can also be given as VENUE_<FLAG>, for example VENUE_DATABASE_URL.";

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

    fn database(&self) -> Result<venue_store::Config, String> {
        venue_store::Config::from_settings(|k| self.lookup(k), "venue-identity", "identity")?.ok_or_else(|| {
            "--database-url (or VENUE_DATABASE_URL) is required: the identity service keeps keys in PostgreSQL".into()
        })
    }
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let command = match args.first().map(String::as_str) {
        Some(c @ ("serve" | "migrate" | "import" | "staff")) => {
            let c = c.to_string();
            args.remove(0);
            c
        }
        _ => "serve".to_string(),
    };
    let outcome = Args::parse(&args).and_then(|a| match command.as_str() {
        "migrate" => migrate(&a),
        "import" => import(&a),
        "staff" => staff(&a),
        _ => run(&a),
    });
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("venue-identity: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `venue-identity staff --email ADDRESS [--on false]`: says that a member is one of the venue's own people, which
/// decides only whether the live venue will name its test environments to them (decision G-20). Run by an operator
/// against the records; there is deliberately no API for it, because nothing a member does should be able to reach
/// it — not a session, not a key, not a request that got as far as the gateway.
fn staff(args: &Args) -> Result<(), String> {
    let email = venue_identity::member::normalise_email(&args.get("email", None)?);
    let on = match args.get("on", Some("true"))?.as_str() {
        "true" => true,
        "false" => false,
        other => return Err(format!("--on must be true or false, not {other}")),
    };
    let config = args.database()?;
    let db = Database::connect_retrying(&config, "venue-identity")?;
    db.wait_for_schema(records::MIGRATIONS, "venue-identity")?;
    let records = records::Postgres::new(db);
    if records::KeyRecords::set_staff(&records, &email, on)? {
        eprintln!("venue-identity: {email} is {}", if on { "staff" } else { "no longer staff" });
        Ok(())
    } else {
        Err(format!("no member with the address {email}"))
    }
}

fn migrate(args: &Args) -> Result<(), String> {
    let config = args.database()?;
    let db = Database::connect_retrying(&config, "venue-identity")?;
    let applied = db.migrate(&venue_store::migrations_dir!())?;
    eprintln!(
        "venue-identity: schema {} of {} is at migration {} (applied now: {applied:?})",
        config.schema,
        config.describe(),
        records::MIGRATIONS.len()
    );
    Ok(())
}

fn import(args: &Args) -> Result<(), String> {
    let file = args.get("file", None)?;
    let catalogue_file = args.get("catalogue", None)?;
    let catalogue = Catalogue::load(&catalogue_file).map_err(|e| format!("{catalogue_file}: {e}"))?;
    let config = args.database()?;
    let db = Database::connect_retrying(&config, "venue-identity")?;
    db.wait_for_schema(records::MIGRATIONS, "venue-identity")?;
    let records = records::Postgres::new(db);
    records::KeyRecords::seed_accounts(
        &records,
        &venue_identity::accounts(&catalogue),
        venue_identity::clock::wall_ms(),
    )?;
    let moved = records::import(Path::new(&file), &records)?;
    eprintln!("venue-identity: imported {moved} keys from {file} into {}", config.describe());
    Ok(())
}

fn run(args: &Args) -> Result<(), String> {
    // Off unless this environment is asked to show its mail. A demo needs it so the codes reach somebody; anywhere
    // an address could really receive them it stays off, because the messages hold the codes themselves.
    let show_mail = match args.get("show-mail", Some("false"))?.as_str() {
        "true" => true,
        "false" => false,
        other => return Err(format!("--show-mail must be true or false, not {other}")),
    };
    let settings = Settings { catalogue: args.get("catalogue", None)?, show_mail };
    let storage = match args.get("store", Some("postgres"))?.as_str() {
        "postgres" => {
            let migrate = match args.get("migrate-on-start", Some("false"))?.as_str() {
                "true" => true,
                "false" => false,
                other => return Err(format!("--migrate-on-start must be true or false, not {other}")),
            };
            let config = args.database()?;
            eprintln!("identity: keys in {}", config.describe());
            Storage::Postgres { config, migrate }
        }
        "memory" => {
            eprintln!("identity: keys kept in memory only (--store memory): every key is forgotten when this stops");
            Storage::Memory(records::Memory::default())
        }
        other => {
            return Err(format!(
                "--store must be postgres or memory, not {other}: keys are no longer kept in a file; move a file's \
                 keys with venue-identity import --file FILE"
            ))
        }
    };
    // The database first: until it answers, nothing listens, so a gateway is refused at once rather than left waiting.
    let records = venue_identity::records(&storage)?;
    let listen = args.get("listen", Some("127.0.0.1:8084"))?;
    let listener = TcpListener::bind(&listen).map_err(|e| format!("{listen}: {e}"))?;
    eprintln!("identity on {listen}");
    venue_identity::start(&settings, records, listener)?;
    loop {
        thread::park();
    }
}

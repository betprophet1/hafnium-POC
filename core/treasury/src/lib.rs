//! `venue-treasury`: the dummy treasury of the mini venue. See `core/treasury/README.md`.
//!
//! It is a process of its own because it holds state that must outlive any gateway: the gateway keeps nothing a
//! failover could lose (interfaces/gateway/README.md), and a transfer is exactly that.

pub mod bank;
pub mod clock;
pub mod faults;
pub mod import;
pub mod ledger;
pub mod node;
pub mod postgres;
pub mod recon;
pub mod records;
pub mod service;
pub mod transfer;
pub mod treasury;

use records::Records;
use std::collections::BTreeMap;
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use treasury::{Config, Treasury};
use venue_link::catalogue::Catalogue;
use venue_store::Database;

pub struct Settings {
    /// The node's ingress and feed.
    pub ingress: String,
    pub feed: String,
    /// The environment's catalogue, whose accounts transfers may be made for.
    pub catalogue: String,
    pub bank: bank::FakeBank,
    pub max_cents: u64,
    /// Messages to lose on purpose, and when to reconcile.
    pub recon: recon::Options,
}

/// Where the treasury's records are kept.
pub enum Storage {
    /// PostgreSQL. With `migrate`, the schema is brought up to date at start (laptops and tests); otherwise the
    /// treasury waits for `venue-treasury migrate` to have done it.
    Postgres { config: venue_store::Config, migrate: bool },
    /// Memory, for tests: nothing outlives the process.
    Memory(records::Memory),
}

/// Opens the records: connects to the database, trying again until it answers, and waits for its schema to be the
/// one this build expects (or migrates it, if asked).
pub fn records(storage: &Storage) -> Result<Arc<dyn Records>, String> {
    match storage {
        Storage::Memory(memory) => Ok(Arc::new(memory.clone())),
        Storage::Postgres { config, migrate } => {
            let db = Database::connect_retrying(config, "venue-treasury")?;
            if *migrate {
                let applied = db.migrate(postgres::MIGRATIONS)?;
                if !applied.is_empty() {
                    eprintln!("treasury: applied migrations {applied:?} to schema {}", db.schema());
                }
            }
            db.wait_for_schema(postgres::MIGRATIONS, "venue-treasury")?;
            Ok(Arc::new(postgres::Postgres::new(db)))
        }
    }
}

/// Opens the book in `records`, and starts sending to the node and serving on `listener`, each on threads of their
/// own.
pub fn start(settings: &Settings, records: Arc<dyn Records>, listener: TcpListener) -> Result<Arc<Treasury>, String> {
    let catalogue = Catalogue::load(&settings.catalogue).map_err(|e| format!("{}: {e}", settings.catalogue))?;
    let accounts: BTreeMap<u64, u64> = catalogue
        .accounts()
        .iter()
        .filter_map(|a| Some((a["id"].as_u64()?, a["cents"].as_u64().unwrap_or(0))))
        .collect();
    let config = Config { bank: settings.bank, max_cents: settings.max_cents, accounts, recon: settings.recon };
    let treasury = Arc::new(Treasury::open(config, records, clock::wall_ms())?);
    let node = node::Node::new(&settings.ingress, &settings.feed, treasury.command_prefix());
    let mut node = faults::Lossy::new(node, settings.recon.faults, settings.bank.seed);
    let driver = Arc::clone(&treasury);
    thread::spawn(move || loop {
        let seen = driver.generation();
        let wait = driver.step(&mut node, clock::wall_ms());
        driver.wait_for_change(seen, wait);
    });
    let server = Arc::clone(&treasury);
    thread::spawn(move || service::serve(&listener, &server));
    Ok(treasury)
}

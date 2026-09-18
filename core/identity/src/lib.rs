//! `venue-identity`: the identity service of test environments. See `core/identity/README.md`.
//!
//! It is a process of its own because it holds state that must outlive any gateway: the gateway keeps nothing a
//! failover could lose (interfaces/gateway/README.md), and a key made through one gateway must work at every other.
//! The gateway also uses this crate's `key` module, so a credential means the same thing on both sides.

pub mod clock;
pub mod key;
pub mod member;
pub mod records;
pub mod service;
pub mod store;

use records::{Account, KeyRecords};
use std::collections::BTreeSet;
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use store::Store;
use venue_link::catalogue::Catalogue;
use venue_store::Database;

pub struct Settings {
    /// The environment's catalogue, whose accounts keys may be made for.
    pub catalogue: String,
}

/// Where the accounts and keys are kept.
pub enum Storage {
    /// PostgreSQL. With `migrate`, the schema is brought up to date at start (laptops and tests); otherwise the
    /// service waits for `venue-identity migrate` to have done it.
    Postgres { config: venue_store::Config, migrate: bool },
    /// Memory: every key is forgotten when the process ends. For tests and throwaway local venues only.
    Memory(records::Memory),
}

/// Opens the records: connects to the database, trying again until it answers, and waits for its schema to be the
/// one this build expects (or migrates it, if asked).
pub fn records(storage: &Storage) -> Result<Arc<dyn KeyRecords>, String> {
    match storage {
        Storage::Memory(memory) => Ok(Arc::new(memory.clone())),
        Storage::Postgres { config, migrate } => {
            let db = Database::connect_retrying(config, "venue-identity")?;
            if *migrate {
                let applied = db.migrate(&venue_store::migrations_dir!())?;
                if !applied.is_empty() {
                    eprintln!("identity: applied migrations {applied:?} to schema {}", db.schema());
                }
            }
            db.wait_for_schema(records::MIGRATIONS, "venue-identity")?;
            Ok(Arc::new(records::Postgres::new(db)))
        }
    }
}

/// The catalogue's accounts, as the accounts table keeps them.
pub fn accounts(catalogue: &Catalogue) -> Vec<Account> {
    catalogue
        .accounts()
        .iter()
        .filter_map(|a| {
            Some(Account {
                id: a["id"].as_u64()?,
                name: a["name"].as_str().unwrap_or("").to_string(),
                dev_token: a["token"].as_str().map(str::to_string),
            })
        })
        .collect()
}

/// Seeds the accounts from the catalogue, reads the keys, and serves on `listener`, on a thread of its own.
pub fn start(settings: &Settings, records: Arc<dyn KeyRecords>, listener: TcpListener) -> Result<Arc<Store>, String> {
    let catalogue = Catalogue::load(&settings.catalogue).map_err(|e| format!("{}: {e}", settings.catalogue))?;
    let accounts = accounts(&catalogue);
    records.seed_accounts(&accounts, clock::wall_ms())?;
    let ids: BTreeSet<u64> = accounts.iter().map(|a| a.id).collect();
    let store = Arc::new(Store::open(records, ids)?);
    let server = Arc::clone(&store);
    thread::spawn(move || service::serve(&listener, &server));
    Ok(store)
}

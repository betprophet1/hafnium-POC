//! `venue-identity`: the identity service of test environments. See `core/identity/README.md`.
//!
//! It is a process of its own because it holds state that must outlive any gateway: the gateway keeps nothing a
//! failover could lose (interfaces/gateway/README.md), and a key made through one gateway must work at every other.
//! The gateway also uses this crate's `key` module, so a credential means the same thing on both sides.

pub mod clock;
pub mod key;
pub mod service;
pub mod store;

use std::collections::BTreeSet;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use store::Store;
use venue_link::catalogue::Catalogue;

pub struct Settings {
    /// The environment's catalogue, whose accounts keys may be made for.
    pub catalogue: String,
    /// The key store: the append-only file every key is kept in.
    pub store: PathBuf,
    pub fsync: bool,
}

/// Opens the store and serves on `listener`, on a thread of its own.
pub fn start(settings: &Settings, listener: TcpListener) -> Result<Arc<Store>, String> {
    let catalogue = Catalogue::load(&settings.catalogue).map_err(|e| format!("{}: {e}", settings.catalogue))?;
    let accounts: BTreeSet<u64> = catalogue.accounts().iter().filter_map(|a| a["id"].as_u64()).collect();
    let store = Arc::new(Store::open(&settings.store, settings.fsync, accounts)?);
    let server = Arc::clone(&store);
    thread::spawn(move || service::serve(&listener, &server));
    Ok(store)
}

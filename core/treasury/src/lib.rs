//! `venue-treasury`: the dummy treasury of the mini venue. See `core/treasury/README.md`.
//!
//! It is a process of its own because it holds state that must outlive any gateway: the gateway keeps nothing a
//! failover could lose (interfaces/gateway/README.md), and a transfer is exactly that.

pub mod bank;
pub mod book;
pub mod clock;
pub mod ledger;
pub mod node;
pub mod service;
pub mod transfer;
pub mod treasury;

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use treasury::{Config, Treasury};
use venue_link::catalogue::Catalogue;

pub struct Settings {
    /// The node's ingress and feed.
    pub ingress: String,
    pub feed: String,
    /// The environment's catalogue, whose accounts transfers may be made for.
    pub catalogue: String,
    /// The book: the append-only file the ledger and transfers are kept in.
    pub ledger: PathBuf,
    pub fsync: bool,
    pub bank: bank::FakeBank,
    pub max_cents: u64,
}

/// Opens the book, and starts sending to the node and serving on `listener`, each on threads of their own.
pub fn start(settings: &Settings, listener: TcpListener) -> Result<Arc<Treasury>, String> {
    let catalogue = Catalogue::load(&settings.catalogue).map_err(|e| format!("{}: {e}", settings.catalogue))?;
    let accounts: BTreeMap<u64, u64> = catalogue
        .accounts()
        .iter()
        .filter_map(|a| Some((a["id"].as_u64()?, a["cents"].as_u64().unwrap_or(0))))
        .collect();
    let config = Config { bank: settings.bank, max_cents: settings.max_cents, accounts };
    let treasury = Arc::new(Treasury::open(config, &settings.ledger, settings.fsync, clock::wall_ms())?);
    let mut node = node::Node::new(&settings.ingress, &settings.feed, treasury.command_prefix());
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

//! A whole gateway for tests, started the way `main` starts one: a real node of its own on free local ports,
//! seeded with the mini venue's catalogue, a parlay service following it, and a gateway that sends to both, follows
//! the node's feed, conflates its books and serves connections on a port of its own. What a test sees is what the engine and the gateway actually do.

use crate::api::{Gateway, Limits};
use crate::hub::Hub;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use venue_link::catalogue::{self, Catalogue};
use venue_link::submit::Submit;
use venue_node::node::{self, Config};

const CATALOGUE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/charts/venue-mini/files/markets.json");
/// A binary market the catalogue lists and opens.
pub const MARKET: u64 = catalogue::instrument_id(catalogue::event_id(1, 1), 0);
/// The mini venue's market maker, funded with 5 000 000 cents; its dev token.
pub const MAKER: u64 = 1;
pub const MAKER_TOKEN: &str = "dev-market-maker";
/// Its retail customer, funded with 50 000 cents; its dev token.
pub const RETAIL: u64 = 2;
pub const RETAIL_TOKEN: &str = "dev-retail-customer";
/// The synthetic maker, the catalogue's registered responder to requests for quote; its dev token.
pub const RESPONDER_TOKEN: &str = "dev-synthetic-maker";

/// The first market of round 1 of a catalogue template: 1 is rain, 2 the football (Arsenal, Draw, Chelsea), 3
/// bitcoin.
pub const fn market(template: u64, index: u64) -> u64 {
    catalogue::instrument_id(catalogue::event_id(template, 1), index)
}

/// How far behind a test's clients may fall before the gateway drops them: small, so a test can get there quickly,
/// and still past tungstenite's own 128 KiB of batching.
pub const MAX_BEHIND: usize = 256 * 1024;

pub struct Venue {
    /// The private half: orders and accounts.
    pub gateway: Arc<Gateway>,
    /// Where the gateway listens, as `host:port`, for REST.
    pub address: String,
    /// Where each half's WebSocket is served, as a client would dial it.
    pub ws_public: String,
    pub ws_private: String,
    /// The node's ingress, for a test that acts as an operator.
    pub ingress: String,
}

/// A seeded node, a treasury on it with a fake bank that decides `dummy_bank` transfers within half a second and never
/// fails one, and a gateway serving both halves that passes transfers to it.
pub fn venue_with_treasury(name: &str) -> Venue {
    venue_with(name, true, true, true)
}

fn free_address() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
    listener.local_addr().expect("its address").to_string()
}

/// Starts a node on ports of its own and returns its ingress and feed addresses. A free port is found by binding it
/// and letting it go, so another test can take it before the node binds it. A node is taken as started only once its
/// ingress accepts a connection; one that stops first, however long it took to get there, is tried again on fresh
/// ports. Under a loaded machine a node can take well over a moment to reach its bind, and one taken as started before
/// it had would leave the test's seeder waiting for ever on a node that is gone.
fn start_node(name: &str) -> (String, String) {
    for attempt in 0..5 {
        let (ingress, feed) = (free_address(), free_address());
        let data_dir = std::env::temp_dir().join(format!("venue-gateway-test-{name}-{}-{attempt}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        let config = Config {
            listen: ingress.parse().expect("an ingress address"),
            feed: feed.parse().expect("a feed address"),
            data_dir,
            group: 1,
            node_id: 1,
            tick: Duration::from_millis(100),
            fsync: false,
            capacity: "small".into(),
        };
        let (stopped, stop) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let outcome = node::serve(&config).map_err(|e| e.to_string());
            let _ = stopped.send(outcome);
        });
        // Up to a minute, counted in waits rather than read off a clock.
        for _ in 0..3000 {
            match stop.recv_timeout(Duration::from_millis(20)) {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if TcpStream::connect(&ingress).is_ok() {
                        return (ingress, feed);
                    }
                }
                Ok(Err(e)) => {
                    eprintln!("test node for {name} could not start, trying other ports: {e}");
                    break;
                }
                _ => {
                    eprintln!("test node for {name} stopped, trying other ports");
                    break;
                }
            }
        }
    }
    panic!("no test node for {name} could start in five attempts");
}

/// A seeded node and a gateway serving both halves, both private to the test that asked. `name` keeps data
/// directories apart.
pub fn venue(name: &str) -> Venue {
    venue_serving(name, true, true)
}

/// The same, with the gateway process serving only the halves asked for, as `--serve` does. Both halves' gateways
/// still follow the feed, so a test can look at either.
pub fn venue_serving(name: &str, public: bool, private: bool) -> Venue {
    venue_with(name, public, private, false)
}

fn venue_with(name: &str, public: bool, private: bool, treasury: bool) -> Venue {
    let (ingress, feed) = start_node(name);
    // Sends wait for the forwarder's first connection, so seeding can start before the node is listening.
    let seeder = Submit::spawn(&ingress);
    let catalogue = Catalogue::load(CATALOGUE).expect("the mini venue's catalogue");
    let ceiling = catalogue::capacity_profile("small").expect("a known profile").orders;
    for input in catalogue.seed(ceiling).expect("the seed") {
        seeder.send(input).expect("seeded");
    }
    let limits = Limits { per_second: 1_000_000, burst: 1_000_000, logons_per_minute: 1_000_000 };
    let hub = || Hub::new(Catalogue::load(CATALOGUE).expect("the mini venue's catalogue"));
    let mut private_half = Gateway::private(hub(), Submit::spawn(&ingress), limits);
    let treasury = treasury.then(|| start_treasury(name, &ingress, &feed));
    if let Some(address) = &treasury {
        private_half = private_half.with_treasury(address);
    }
    let gateway = crate::start_half(private_half, &feed);
    if let Some(address) = treasury {
        crate::treasury::follow_updates(&gateway, address);
    }
    let parlays = free_address();
    let config = venue_parlays::service::Config {
        ingress: ingress.clone(),
        feed: feed.clone(),
        listen: parlays.clone(),
        window_seconds: 30,
        max_parlays: 20,
    };
    venue_parlays::service::start(Catalogue::load(CATALOGUE).expect("the catalogue"), &config).expect("parlay service");
    Gateway::connect_parlays(&gateway, &parlays);
    let public_half = crate::start_half(Gateway::public(hub(), limits), &feed);
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port for the gateway");
    let address = listener.local_addr().expect("its address").to_string();
    let halves = crate::Halves {
        public: public.then(|| Arc::clone(&public_half)),
        private: private.then(|| Arc::clone(&gateway)),
    };
    thread::spawn(move || crate::serve_with(&halves, &listener, MAX_BEHIND));
    // Up to ten seconds for the feed to deliver the listing to both halves, counted in waits rather than read off a
    // clock.
    for wait in 0.. {
        if gateway.hub().market(MARKET).is_some() && public_half.hub().market(MARKET).is_some() {
            break;
        }
        assert!(wait < 500, "the feed never listed market {MARKET}");
        thread::sleep(Duration::from_millis(20));
    }
    let (ws_public, ws_private) = (format!("ws://{address}/v4/ws_public"), format!("ws://{address}/v4/ws_private"));
    Venue { gateway, address, ws_public, ws_private, ingress }
}

/// Starts a treasury of the test's own on the node, with a fresh book, and returns where it listens.
fn start_treasury(name: &str, ingress: &str, feed: &str) -> String {
    let ledger = std::env::temp_dir().join(format!("venue-gateway-test-{name}-{}.treasury", std::process::id()));
    let _ = std::fs::remove_file(&ledger);
    let settings = venue_treasury::Settings {
        ingress: ingress.to_string(),
        feed: feed.to_string(),
        catalogue: CATALOGUE.to_string(),
        ledger,
        fsync: false,
        bank: venue_treasury::bank::FakeBank { seed: 1, min_delay_ms: 200, max_delay_ms: 500, failure_per_mille: 0 },
        max_cents: 100_000_000,
    };
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port for the treasury");
    let address = listener.local_addr().expect("its address").to_string();
    venue_treasury::start(&settings, listener).expect("a treasury");
    address
}

/// Waits up to five seconds, in 20 ms steps, for `ready` to hold.
pub fn eventually(what: &str, mut ready: impl FnMut() -> bool) {
    for wait in 0.. {
        if ready() {
            return;
        }
        assert!(wait < 250, "never happened: {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

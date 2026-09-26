//! A whole gateway for tests, started the way `main` starts one: a real node of its own on free local ports, seeded with
//! the mini venue's catalogue, a parlay service following it, an identity service for API keys, and a gateway that
//! sends to the node and the parlay service, verifies keys with the identity service, follows the node's feed,
//! conflates its books and serves connections on a port of its own. What a test sees is what the engine and the
//! gateway actually do.

use crate::api::{Gateway, Limits};
use crate::audit::Audit;
use crate::forwarded::TrustedProxies;
use crate::hub::Hub;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use venue_link::catalogue::{self, Catalogue};
use venue_link::submit::Submit;
use venue_node::node::{self, Config};

pub const CATALOGUE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/charts/venue-mini/files/markets.json");
/// A binary market the catalogue lists and opens.
pub const MARKET: u64 = catalogue::instrument_id(catalogue::event_id(1, 1), 0);
/// The mini venue's market maker, funded with 5 000 000 cents; its dev token.
pub const MAKER: u64 = 1;
pub const MAKER_TOKEN: &str = "dev-market-maker";
/// Its retail customer, funded with 50 000 cents; its dev token.
pub const RETAIL: u64 = 2;
pub const RETAIL_TOKEN: &str = "dev-retail-customer";
/// The synthetic maker, the catalogue's responder, approved to answer requests for quote; its dev token.
pub const RESPONDER: u64 = 10;
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
    /// The identity service's records, so a test can do what an operator does — making somebody staff, say — which
    /// deliberately has no route (decision G-20).
    pub identity_records: Arc<dyn venue_identity::records::KeyRecords>,
    /// The gateway's audit log: a directory of the test's own, written without syncing.
    pub audit_dir: PathBuf,
    /// The node's data directory, which holds the journal a test can replay.
    pub data_dir: PathBuf,
}

/// A seeded node, a treasury on it with a fake bank that decides `dummy_bank` transfers within half a second and never
/// fails one, and a gateway serving both halves that passes transfers to it.
pub fn venue_with_treasury(name: &str) -> Venue {
    venue_with(name, true, true, true, "")
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
fn start_node(name: &str) -> (String, String, PathBuf) {
    for attempt in 0..5 {
        let (ingress, feed) = (free_address(), free_address());
        let data_dir = std::env::temp_dir().join(format!("venue-gateway-test-{name}-{}-{attempt}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        let config = Config {
            listen: ingress.parse().expect("an ingress address"),
            feed: feed.parse().expect("a feed address"),
            data_dir: data_dir.clone(),
            group: 1,
            node_id: 1,
            tick: Duration::from_millis(100),
            fsync: false,
            capacity: "small".into(),
            metrics: None,
        };
        let (stopped, stop) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let outcome = node::serve(&config).map_err(|e| e.to_string());
            let _ = stopped.send(outcome);
        });
        // Up to a minute, counted in waits rather than read off a clock. A connection to the ingress port is not enough
        // on its own: free ports are found by binding and letting go, so until this node has bound its ports another
        // test's node, or the client end of another test's connection, may hold them. The node creates its journal
        // only once its engine is built, a moment before it binds its ingress and then its feed; a node that has done
        // that, whose ingress accepts, and that is still running some waits later has bound both, since one that could
        // not bind stops at once.
        let journal = data_dir.join("journal.sbe");
        let mut settled = None;
        for _ in 0..3000 {
            match stop.recv_timeout(Duration::from_millis(20)) {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    settled = match settled {
                        Some(0) => return (ingress, feed, data_dir),
                        Some(n) => Some(n - 1),
                        None if journal.exists() && TcpStream::connect(&ingress).is_ok() => Some(10),
                        None => None,
                    };
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
    venue_with(name, public, private, false, "")
}

/// A venue serving both halves whose gateway takes connections from `proxies` (`--trusted-proxies`) as a proxy's.
pub fn venue_trusting(name: &str, proxies: &str) -> Venue {
    venue_with(name, true, true, false, proxies)
}

fn venue_with(name: &str, public: bool, private: bool, treasury: bool, proxies: &str) -> Venue {
    let (ingress, feed, data_dir) = start_node(name);
    seed(name, &ingress);
    let limits = Limits { per_second: 1_000_000, burst: 1_000_000, logons_per_minute: 1_000_000 };
    let audit_dir = std::env::temp_dir().join(format!("venue-gateway-test-{name}-{}.audit", std::process::id()));
    let _ = std::fs::remove_dir_all(&audit_dir);
    let audit = Audit::start(&audit_dir, "test", false).expect("an audit log");
    let proxies = TrustedProxies::parse(proxies).expect("trusted proxies");
    let hub = || Hub::new(Catalogue::load(CATALOGUE).expect("the mini venue's catalogue"));
    let (identity_address, identity_records) = start_identity(name);
    // Three environments, so a test can see which of them a caller is told about: the live venue, a branch that is
    // somebody else's, and the venue answering. The last is the one a caller is always told about, however little
    // else they are entitled to.
    let environments_file = std::env::temp_dir().join(format!("venue-environments-{name}-{}.json", std::process::id()));
    std::fs::write(
        &environments_file,
        r#"{"environments": [
             {"id": "prod", "label": "Live", "url": "https://venue.test", "live": true},
             {"id": "pr-7", "label": "PR 7", "url": "https://pr-7.venue.test", "live": false},
             {"id": "here", "label": "This venue", "url": "https://here.venue.test", "live": false, "self": true}
           ]}"#,
    )
    .expect("an environments file");
    let mut private_half = Gateway::private(hub(), Submit::spawn(&ingress), limits)
        .with_audit(audit.clone())
        .with_trusted_proxies(proxies.clone())
        .with_origins(vec!["http://app.test".into()])
        .with_environments(crate::bootstrap::Environments::from_file(&environments_file))
        .with_identity(Some(identity_address), true);
    let treasury = treasury.then(|| start_treasury(name, &ingress, &feed));
    if let Some(address) = &treasury {
        private_half = private_half.with_treasury(address);
    }
    let gateway = crate::start_half(private_half, &feed);
    if let Some(address) = treasury {
        crate::treasury::follow_updates(&gateway, address);
    }
    let (service, parlays) = start_parlays(&ingress, &feed);
    Gateway::connect_parlays(&gateway, &parlays);
    let public_half = crate::start_half(
        Gateway::public(hub(), limits)
            .with_audit(audit)
            .with_trusted_proxies(proxies)
            .with_origins(vec!["http://app.test".into()]),
        &feed,
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port for the gateway");
    let address = listener.local_addr().expect("its address").to_string();
    let halves = crate::Halves {
        public: public.then(|| Arc::clone(&public_half)),
        private: private.then(|| Arc::clone(&gateway)),
    };
    thread::spawn(move || crate::serve_with(&halves, &listener, MAX_BEHIND, None, false));
    // Up to ten seconds for the feed to deliver the listing to both halves, counted in waits rather than read off a
    // clock.
    for wait in 0.. {
        if gateway.hub().market(MARKET).is_some() && public_half.hub().market(MARKET).is_some() {
            break;
        }
        assert!(wait < 1_500, "the feed never listed market {MARKET}");
        thread::sleep(Duration::from_millis(20));
    }
    // Requests for quote are served once the parlay service has caught up with the feed and the private half has
    // reached it; a test that asked sooner would be refused as unavailable.
    eventually("the parlay service catches up with the feed", || service.ready());
    eventually("the private half reaches the parlay service", || {
        gateway.open_rfqs_pending(RESPONDER, None).is_ok_and(|pending| gateway.wait_for(pending).is_ok())
    });
    let (ws_public, ws_private) = (format!("ws://{address}/v4/ws_public"), format!("ws://{address}/v4/ws_private"));
    Venue { gateway, address, ws_public, ws_private, ingress, identity_records, audit_dir, data_dir }
}

/// Seeds a node with the mini venue's catalogue.
fn seed(name: &str, ingress: &str) {
    // Sends wait for the forwarder's first connection, so seeding can start before the node is listening.
    let seeder = Submit::spawn(ingress);
    let catalogue = Catalogue::load(CATALOGUE).expect("the mini venue's catalogue");
    let profile = catalogue::capacity_profile("small").expect("a known profile");
    // The seed's commands have fixed IDs, so one resent after its connection ended under load is sequenced once.
    for input in catalogue.seed(&profile).expect("the seed") {
        let mut attempt = 0;
        while let Err(e) = seeder.send(input.clone()) {
            attempt += 1;
            assert!(attempt < 5, "seeding {name} failed five times: {e}");
            thread::sleep(Duration::from_millis(200));
        }
    }
}

/// Starts a parlay service on the node, with the catalogue's responders approved, and returns it and where it listens.
fn start_parlays(ingress: &str, feed: &str) -> (Arc<venue_parlays::service::Service>, String) {
    let listen = free_address();
    let profile = catalogue::capacity_profile("small").expect("a known profile");
    let config = venue_parlays::service::Config {
        ingress: ingress.to_string(),
        feed: feed.to_string(),
        listen: listen.clone(),
        window_seconds: 30,
        max_parlays: 20,
        capacity: venue_parlays::rules::Capacity {
            events: profile.events,
            instruments: profile.instruments,
            levels: profile.levels,
        },
        responders: None,
        min_quantity: venue_parlays::rules::LOT_STEP,
        from_view: None,
    };
    let service = venue_parlays::service::start(Catalogue::load(CATALOGUE).expect("the catalogue"), &config)
        .expect("parlay service");
    (service, listen)
}

/// Starts an identity service of the test's own, with a fresh store, and returns where it listens. Its keys are in
/// PostgreSQL, in a schema of the test's own, when `VENUE_TEST_DATABASE_URL` names a server, and in memory otherwise.
fn start_identity(name: &str) -> (String, Arc<dyn venue_identity::records::KeyRecords>) {
    let settings = venue_identity::Settings { catalogue: CATALOGUE.to_string(), show_mail: true };
    let storage =
        match venue_store::testing::url().and_then(|_| venue_store::testing::config(&format!("gw_identity_{name}"))) {
            Some(config) => venue_identity::Storage::Postgres { config, migrate: true },
            None => venue_identity::Storage::Memory(venue_identity::records::Memory::default()),
        };
    let records = venue_identity::records(&storage).expect("the identity service's records");
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port for the identity service");
    let address = listener.local_addr().expect("its address").to_string();
    venue_identity::start(&settings, Arc::clone(&records), listener).expect("an identity service");
    (address, records)
}

/// Starts a treasury of the test's own on the node, with a fresh book, and returns where it listens. Its records are
/// in PostgreSQL, in a schema of the test's own, when `VENUE_TEST_DATABASE_URL` names a server, and in memory
/// otherwise.
fn start_treasury(name: &str, ingress: &str, feed: &str) -> String {
    let settings = venue_treasury::Settings {
        ingress: ingress.to_string(),
        feed: feed.to_string(),
        catalogue: CATALOGUE.to_string(),
        bank: venue_treasury::bank::FakeBank { seed: 1, min_delay_ms: 200, max_delay_ms: 500, failure_per_mille: 0 },
        max_cents: 100_000_000,
        recon: venue_treasury::recon::Options::default(),
        founded_at: 0,
    };
    let storage =
        match venue_store::testing::url().and_then(|_| venue_store::testing::config(&format!("gw_treasury_{name}"))) {
            Some(config) => venue_treasury::Storage::Postgres { config, migrate: true },
            None => venue_treasury::Storage::Memory(venue_treasury::records::Memory::default()),
        };
    let records = venue_treasury::records(&storage).expect("the treasury's records");
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port for the treasury");
    let address = listener.local_addr().expect("its address").to_string();
    venue_treasury::start(&settings, records, listener).expect("a treasury");
    address
}

/// Waits up to thirty seconds, in 20 ms steps, for `ready` to hold. Generous, because tests share a machine with
/// other builds: the wait costs time only when something is wrong.
pub fn eventually(what: &str, mut ready: impl FnMut() -> bool) {
    for wait in 0.. {
        if ready() {
            return;
        }
        assert!(wait < 1_500, "never happened: {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// A stand-in for the network between a client and a server, which a test can cut: it accepts connections on a port of
/// its own and passes each through to its target, both ways. Cutting it ends every connection it carries, as a server
/// that went away would, and a closed one takes new connections only to drop them. Its target can change, so that the
/// next connection reaches a server started in place of the old one.
pub struct Proxy {
    /// Where clients dial.
    pub address: String,
    target: Arc<std::sync::Mutex<String>>,
    carried: Arc<std::sync::Mutex<Vec<TcpStream>>>,
    open: Arc<std::sync::atomic::AtomicBool>,
}

impl Proxy {
    pub fn start(target: &str) -> Self {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Mutex;
        let listener = TcpListener::bind("127.0.0.1:0").expect("a port for a proxy");
        let address = listener.local_addr().expect("its address").to_string();
        let target = Arc::new(Mutex::new(target.to_string()));
        let carried: Arc<Mutex<Vec<TcpStream>>> = Arc::new(Mutex::new(Vec::new()));
        let open = Arc::new(AtomicBool::new(true));
        let (to, streams, accepting) = (Arc::clone(&target), Arc::clone(&carried), Arc::clone(&open));
        thread::spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { continue };
                if !accepting.load(Ordering::SeqCst) {
                    continue;
                }
                let upstream = to.lock().map(|t| t.clone()).unwrap_or_default();
                let Ok(server) = TcpStream::connect(&upstream) else { continue };
                let _ = (client.set_nodelay(true), server.set_nodelay(true));
                let clones = (client.try_clone(), server.try_clone(), client.try_clone(), server.try_clone());
                let (Ok(mut from_client), Ok(mut to_client), Ok(kept_client), Ok(kept_server)) = clones else {
                    continue;
                };
                if let Ok(mut carried) = streams.lock() {
                    carried.retain(|s| s.peer_addr().is_ok());
                    carried.extend([kept_client, kept_server]);
                }
                let (mut to_server, mut from_server) = (server, client);
                thread::spawn(move || {
                    let _ = std::io::copy(&mut from_client, &mut to_server);
                    let _ = to_server.shutdown(std::net::Shutdown::Both);
                    let _ = from_client.shutdown(std::net::Shutdown::Both);
                });
                thread::spawn(move || {
                    let _ = std::io::copy(&mut to_client, &mut from_server);
                    let _ = from_server.shutdown(std::net::Shutdown::Both);
                    let _ = to_client.shutdown(std::net::Shutdown::Both);
                });
            }
        });
        Self { address, target, carried, open }
    }

    /// Ends every connection carried now; later ones are carried as before.
    pub fn cut(&self) {
        if let Ok(mut carried) = self.carried.lock() {
            for stream in carried.drain(..) {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
    }

    /// Ends every connection and carries no more: the server has gone.
    pub fn close(&self) {
        self.open.store(false, std::sync::atomic::Ordering::SeqCst);
        self.cut();
    }

    /// New connections go to `target` from now on.
    pub fn retarget(&self, target: &str) {
        if let Ok(mut to) = self.target.lock() {
            *to = target.to_string();
        }
    }
}

/// What a gateway process of a fleet serves (`--serve`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Serve {
    Public,
    Private,
    Both,
}

/// One gateway process of a fleet, as its clients and a test see it.
pub struct Member {
    /// Its `--audit-instance`.
    pub instance: String,
    /// Where its clients dial it, for REST, through a proxy that stands for the network (`Proxy`).
    pub address: String,
    pub ws_public: String,
    pub ws_private: String,
    /// Its private half, if `--serve` made one.
    pub private: Option<Arc<Gateway>>,
    front: Proxy,
}

impl Member {
    /// The private half, which the test asked for.
    pub fn private(&self) -> &Arc<Gateway> {
        self.private.as_ref().expect("a gateway serving the private half")
    }

    /// The gateway goes away as far as its clients can tell: every connection to it ends and it takes no more. Its
    /// halves stay in memory, unreachable, which a test in one process cannot avoid; they still see their sessions end.
    pub fn stop(&self) {
        self.front.close();
    }
}

/// Several gateway processes' worth of gateways in one test: one node, seeded; the shared services every gateway
/// forwards to (identity, a treasury, a parlay service); and gateways added one at a time, each serving what `--serve`
/// would, with its own feed follower, its own audit instance in one shared audit directory, and its own port. The
/// gateways reach the parlay service through a proxy, so that a test can restart it.
pub struct Fleet {
    name: String,
    pub ingress: String,
    feed: String,
    identity: String,
    treasury: String,
    /// The parlay service now behind `parlay_proxy`.
    pub parlays: std::sync::Mutex<Arc<venue_parlays::service::Service>>,
    parlay_proxy: Proxy,
    /// Every gateway's audit log, one instance each.
    pub audit_dir: PathBuf,
    pub gateways: Vec<Member>,
}

/// Limits no test meets unless it means to.
pub const GENEROUS: Limits = Limits { per_second: 1_000_000, burst: 1_000_000, logons_per_minute: 1_000_000 };

impl Fleet {
    /// The node and the shared services, and no gateway yet.
    pub fn start(name: &str) -> Self {
        let (ingress, feed, _data_dir) = start_node(name);
        seed(name, &ingress);
        // The fleet tests do not act as an operator, so the records go unused here.
        let (identity, _identity_records) = start_identity(name);
        let treasury = start_treasury(name, &ingress, &feed);
        let (service, parlays) = start_parlays(&ingress, &feed);
        eventually("the parlay service catches up with the feed", || service.ready());
        let audit_dir = std::env::temp_dir().join(format!("venue-gateway-fleet-{name}-{}.audit", std::process::id()));
        let _ = std::fs::remove_dir_all(&audit_dir);
        Self {
            name: name.to_string(),
            ingress,
            feed,
            identity,
            treasury,
            parlays: std::sync::Mutex::new(service),
            parlay_proxy: Proxy::start(&parlays),
            audit_dir,
            gateways: Vec::new(),
        }
    }

    /// Adds a gateway serving `serve`, with generous limits, and returns its index.
    pub fn add(&mut self, serve: Serve) -> usize {
        self.add_with(serve, GENEROUS)
    }

    /// Adds a gateway serving `serve` with flood limits `limits`, started as `main` starts one, and returns its index
    /// once it is ready: its halves have the venue from the feed and its private half has reached the parlay service.
    pub fn add_with(&mut self, serve: Serve, limits: Limits) -> usize {
        let index = self.gateways.len();
        let instance = format!("{}-{index}", self.name);
        let audit = Audit::start(&self.audit_dir, &instance, false).expect("an audit log");
        let hub = || Hub::new(Catalogue::load(CATALOGUE).expect("the mini venue's catalogue"));
        let public = (serve != Serve::Private)
            .then(|| crate::start_half(Gateway::public(hub(), limits).with_audit(audit.clone()), &self.feed));
        let private = (serve != Serve::Public).then(|| {
            let half = Gateway::private(hub(), Submit::spawn(&self.ingress), limits)
                .with_audit(audit.clone())
                .with_identity(Some(self.identity.clone()), true)
                .with_treasury(&self.treasury);
            let half = crate::start_half(half, &self.feed);
            crate::treasury::follow_updates(&half, self.treasury.clone());
            Gateway::connect_parlays(&half, &self.parlay_proxy.address);
            half
        });
        let listener = TcpListener::bind("127.0.0.1:0").expect("a port for the gateway");
        let listening = listener.local_addr().expect("its address").to_string();
        let halves = crate::Halves { public: public.clone(), private: private.clone() };
        thread::spawn(move || crate::serve_with(&halves, &listener, MAX_BEHIND, None, false));
        for half in public.iter().chain(private.iter()) {
            eventually("a gateway's half has the venue from the feed", || half.hub().market(MARKET).is_some());
        }
        if let Some(half) = &private {
            eventually("a private half reaches the parlay service", || {
                half.open_rfqs_pending(RESPONDER, None).is_ok_and(|pending| half.wait_for(pending).is_ok())
            });
        }
        let front = Proxy::start(&listening);
        let address = front.address.clone();
        let (ws_public, ws_private) = (format!("ws://{address}/v4/ws_public"), format!("ws://{address}/v4/ws_private"));
        self.gateways.push(Member { instance, address, ws_public, ws_private, private, front });
        index
    }

    /// Replaces the parlay service with a fresh one, as a restart would: it knows no requests and no subscriptions,
    /// every gateway's connection to the old one ends, and their next connection reaches the new one once it has
    /// caught up with the feed.
    pub fn restart_parlays(&self) -> Arc<venue_parlays::service::Service> {
        let (service, listen) = start_parlays(&self.ingress, &self.feed);
        eventually("the new parlay service catches up with the feed", || service.ready());
        if let Ok(mut current) = self.parlays.lock() {
            *current = Arc::clone(&service);
        }
        self.parlay_proxy.retarget(&listen);
        self.parlay_proxy.cut();
        service
    }
}

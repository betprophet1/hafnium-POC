//! A whole gateway for tests, started the way `main` starts one: a real node of its own on free local ports,
//! seeded with the mini venue's catalogue, and a gateway that sends to it, follows its feed, conflates its books and
//! serves connections on a port of its own. What a test sees is what the engine and the gateway actually do.

use crate::api::{Gateway, Limits};
use crate::hub::Hub;
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use venue_link::catalogue::{self, Catalogue};
use venue_link::submit::Submit;
use venue_node::node::{self, Config};

const CATALOGUE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/charts/venue-mini/files/markets.json");
/// A binary market the catalogue lists and opens.
pub const MARKET: u64 = 10010;
/// The mini venue's market maker, funded with 5 000 000 cents; its dev token.
pub const MAKER: u64 = 1;
pub const MAKER_TOKEN: &str = "dev-market-maker";
/// Its retail customer, funded with 50 000 cents; its dev token.
pub const RETAIL: u64 = 2;
pub const RETAIL_TOKEN: &str = "dev-retail-customer";

pub struct Venue {
    pub gateway: Arc<Gateway>,
    /// Where the gateway's WebSocket is served, as a client would dial it.
    pub ws_url: String,
}

fn free_address() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
    listener.local_addr().expect("its address").to_string()
}

/// A seeded node and a serving gateway, both private to the test that asked. `name` keeps data directories apart.
pub fn venue(name: &str) -> Venue {
    let (ingress, feed) = (free_address(), free_address());
    let data_dir = std::env::temp_dir().join(format!("venue-gateway-test-{name}-{}", std::process::id()));
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
    thread::spawn(move || {
        if let Err(e) = node::serve(&config) {
            eprintln!("test node: {e}");
        }
    });
    // Sends wait for the forwarder's first connection, so seeding can start before the node is listening.
    let seeder = Submit::spawn(&ingress);
    let catalogue = Catalogue::load(CATALOGUE).expect("the mini venue's catalogue");
    let ceiling = catalogue::capacity_profile("small").expect("a known profile").orders;
    for input in catalogue.seed(ceiling).expect("the seed") {
        seeder.send(input).expect("seeded");
    }
    let limits = Limits { per_second: 1_000_000, burst: 1_000_000, logons_per_minute: 1_000_000 };
    let hub = Hub::new(Catalogue::load(CATALOGUE).expect("the mini venue's catalogue"));
    let gateway = Arc::new(Gateway::new(hub, Submit::spawn(&ingress), limits));
    crate::follow_feed(&gateway, feed);
    crate::conflate(&gateway);
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port for the gateway");
    let ws_url = format!("ws://{}/v4/ws", listener.local_addr().expect("its address"));
    {
        let gateway = Arc::clone(&gateway);
        thread::spawn(move || crate::serve(&gateway, &listener));
    }
    // Up to ten seconds for the feed to deliver the listing, counted in waits rather than read off a clock.
    for wait in 0.. {
        if gateway.hub().market(MARKET).is_some() {
            break;
        }
        assert!(wait < 500, "the feed never listed market {MARKET}");
        thread::sleep(Duration::from_millis(20));
    }
    Venue { gateway, ws_url }
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

//! `venue-gateway`: interface 1, the REST and WebSocket gateway, in front of one node. The contract is
//! `interfaces/gateway/`; this is its first implementation, for the mini venue.

mod api;
mod http;
mod hub;
mod limits;
mod parlays;
mod price;
mod reactor;
mod rest;
#[cfg(test)]
mod testing;
mod treasury;
mod ws;

use api::{Gateway, Limits};
use hub::Hub;
use reactor::Reactors;
use std::collections::BTreeMap;
use std::net::TcpListener;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use venue_link::catalogue::Catalogue;
use venue_link::submit::Submit;
use venue_link::wire::{self, Error};

const USAGE: &str = "usage:
  venue-gateway --feed ADDR --catalogue FILE [--serve both] [--ingress ADDR] [--treasury ADDR] [--parlays ADDR]
                [--listen 127.0.0.1:8081] [--per-second 200] [--burst 400] [--logons-per-minute 30]

--serve is public, private or both: the halves of interface 1 this process serves. Public is market data on
/v4/ws_public and GET /v4/markets; private is orders and accounts on /v4/ws_private, /v4/account and /v4/orders,
and needs --ingress. With --treasury, the private half also passes deposits and withdrawals (/v4/transfers) to the
treasury there and sends its updates to private sessions; without it, /v4/transfers answers 503. --parlays is the
parlay service (venue-parlays) the private half sends requests for quote (/v4/rfqs) to; without it they are refused
as unavailable. Every flag can also be given as VENUE_<FLAG>, for example VENUE_FEED.";

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

    fn optional(&self, key: &str) -> Option<String> {
        self.get(key, None).ok()
    }

    fn number(&self, key: &str, default: u32) -> Result<u32, Error> {
        Ok(self.get(key, Some(default.to_string().as_str()))?.parse()?)
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match Args::parse(&args).and_then(|a| run(&a)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("venue-gateway: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<(), Error> {
    let catalogue = Catalogue::load(&args.get("catalogue", None)?)?;
    let limits = Limits {
        per_second: args.number("per-second", 200)?,
        burst: args.number("burst", 400)?,
        logons_per_minute: args.number("logons-per-minute", 30)?,
    };
    let serving = args.get("serve", Some("both"))?;
    let (public, private) = match serving.as_str() {
        "both" => (true, true),
        "public" => (true, false),
        "private" => (false, true),
        other => return Err(format!("--serve must be public, private or both, not {other}").into()),
    };
    let feed = args.get("feed", None)?;
    let catalogue_file = args.get("catalogue", None)?;
    let halves = Halves {
        public: if public {
            Some(start_half(Gateway::public(Hub::new(Catalogue::load(&catalogue_file)?), limits), &feed))
        } else {
            None
        },
        private: if private {
            let ingress = Submit::spawn(&args.get("ingress", None)?);
            let mut gateway = Gateway::private(Hub::new(catalogue), ingress, limits);
            let treasury = args.get("treasury", Some(""))?;
            if !treasury.is_empty() {
                gateway = gateway.with_treasury(&treasury);
            }
            let half = start_half(gateway, &feed);
            if !treasury.is_empty() {
                treasury::follow_updates(&half, treasury);
            }
            if let Some(parlays) = args.optional("parlays") {
                Gateway::connect_parlays(&half, &parlays);
            }
            Some(half)
        } else {
            None
        },
    };
    let listener = TcpListener::bind(args.get("listen", Some("127.0.0.1:8081"))?)?;
    eprintln!(
        "gateway on {}: serving {serving} (public /v4/ws_public, private /v4/ws_private)",
        listener.local_addr()?
    );
    serve(&halves, &listener);
    Ok(())
}

/// The halves of interface 1 one process serves (interfaces/gateway/README.md, public and private). Each is a gateway
/// of its own: its own view of the venue from the feed, its own lock, and its own loops.
struct Halves {
    public: Option<Arc<Gateway>>,
    private: Option<Arc<Gateway>>,
}

/// A half that follows the node's feed and sends its book subscribers what changed.
fn start_half(gateway: Gateway, feed: &str) -> Arc<Gateway> {
    let gateway = Arc::new(gateway);
    follow_feed(&gateway, feed.to_string());
    conflate(&gateway);
    gateway
}

/// Keeps the hub's view of the venue current from the node's feed, reconnecting for ever.
fn follow_feed(gateway: &Arc<Gateway>, feed: String) {
    let follower = Arc::clone(gateway);
    thread::spawn(move || {
        wire::follow(&feed, || follower.hub().restart(), |unit| follower.hub().apply(&unit));
    });
}

/// Sends book subscribers what changed, at a fixed rate.
fn conflate(gateway: &Arc<Gateway>) {
    let conflater = Arc::clone(gateway);
    thread::spawn(move || loop {
        // Book topics carry at most ten snapshots a second (interface 1 README, market data).
        thread::sleep(Duration::from_millis(100));
        conflater.hub().flush_books();
    });
}

/// How far behind on what it has been sent a WebSocket client may fall before it is disconnected: a few seconds of
/// busy market data, far more than a client that is reading ever holds.
const MAX_BEHIND: usize = 4 * 1024 * 1024;

/// Accepts connections for ever and hands each one to an event loop of the half it is for, which serves it whether it
/// is a WebSocket session or a REST request.
fn serve(halves: &Halves, listener: &TcpListener) {
    serve_with(halves, listener, MAX_BEHIND);
}

/// `serve`, with a WebSocket client allowed to fall `max_behind` bytes behind.
fn serve_with(halves: &Halves, listener: &TcpListener, max_behind: usize) {
    let threads = thread::available_parallelism().map_or(2, |n| n.get().min(4));
    let start = |gateway: &Arc<Gateway>| Reactors::start(gateway, threads, max_behind);
    let (public, private) =
        match (halves.public.as_ref().map(start).transpose(), halves.private.as_ref().map(start).transpose()) {
            (Ok(public), Ok(private)) => (public, private),
            (Err(e), _) | (_, Err(e)) => {
                eprintln!("venue-gateway: event loops: {e}");
                return;
            }
        };
    if let (Some(public), Some(private)) = (&public, &private) {
        Reactors::join(public, private);
    }
    // With both halves, a connection lands on the private half's loops first: a trader's connection is never taken
    // on by loops busy fanning out market data, and a viewer's moves on after one look at its request line.
    let Some(first) = private.or(public) else { return };
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        first.adopt(stream);
    }
}

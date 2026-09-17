//! `venue-gateway`: interface 1, the REST and WebSocket gateway, in front of one node. The contract is
//! `interfaces/gateway/`; this is its first implementation, for the mini venue.

mod api;
mod audit;
mod http;
mod hub;
mod identity;
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
use audit::Audit;
use hub::Hub;
use reactor::Reactors;
use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use venue_link::catalogue::Catalogue;
use venue_link::submit::Submit;
use venue_link::wire::{self, Error};

const USAGE: &str = "usage:
  venue-gateway verify-audit FILE...
  venue-gateway --feed ADDR --catalogue FILE [--serve both] [--ingress ADDR] [--treasury ADDR] [--parlays ADDR]
                [--identity ADDR] [--dev-tokens true] [--listen 127.0.0.1:8081] [--per-second 200] [--burst 400] [--logons-per-minute 30]
                [--audit-dir ./gateway-audit] [--audit-instance HOSTNAME] [--audit-fsync true]

--serve is public, private or both: the halves of interface 1 this process serves. Public is market data on
/v4/ws_public and GET /v4/markets; private is orders and accounts on /v4/ws_private, /v4/account and /v4/orders,
and needs --ingress. With --treasury, the private half also passes deposits and withdrawals (/v4/transfers) to the
treasury there and sends its updates to private sessions; without it, /v4/transfers answers 503. --parlays is the
parlay service (venue-parlays) the private half sends requests for quote (/v4/rfqs) to; without it they are refused
as unavailable. --identity is the identity service (venue-identity) the private half verifies API keys with and passes
/v4/keys to; without it keys are refused as unavailable. --dev-tokens false refuses the catalogue's dev tokens, which
otherwise act as their accounts with every permission. Every request and what it was answered is written to the audit log in --audit-dir
(interfaces/audit/README.md); a gateway that cannot write it does not start, and one that stops being able to exits.
Every flag can also be given as VENUE_<FLAG>, for example VENUE_FEED.";

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
    if args.first().map(String::as_str) == Some("verify-audit") {
        return verify_audit(&args[1..]);
    }
    match Args::parse(&args).and_then(|a| run(&a)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("venue-gateway: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `venue-gateway verify-audit FILE...`: checks audit log files (interfaces/audit/README.md, verifying), as
/// `interfaces/tools/audit_verify.py` does, for a machine that has the gateway and no Python.
fn verify_audit(files: &[String]) -> ExitCode {
    if files.is_empty() {
        eprintln!("usage: venue-gateway verify-audit FILE...");
        return ExitCode::from(64);
    }
    let mut failed = false;
    for file in files {
        match audit::verify(std::path::Path::new(file)) {
            Ok(closed) => println!("{file}: ok, {}", if closed { "closed" } else { "open" }),
            Err(broken) => {
                println!("FAIL {file}: {broken}");
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
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
    let hostname = std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty()).unwrap_or_else(|| "gateway".into());
    let audit_dir = PathBuf::from(args.get("audit-dir", Some("./gateway-audit"))?);
    let fsync = match args.get("audit-fsync", Some("true"))?.as_str() {
        "true" => true,
        "false" => false,
        other => return Err(format!("--audit-fsync must be true or false, not {other}").into()),
    };
    // Before anything listens: a gateway that cannot keep its audit log takes no connection.
    let audit = Audit::start(&audit_dir, &args.get("audit-instance", Some(&hostname))?, fsync)
        .map_err(|e| format!("audit log in {}: {e}", audit_dir.display()))?;
    let halves = Halves {
        public: if public {
            let gateway = Gateway::public(Hub::new(Catalogue::load(&catalogue_file)?), limits);
            Some(start_half(gateway.with_audit(audit.clone()), &feed))
        } else {
            None
        },
        private: if private {
            let ingress = Submit::spawn(&args.get("ingress", None)?);
            let dev_tokens = match args.get("dev-tokens", Some("true"))?.as_str() {
                "true" => true,
                "false" => false,
                other => return Err(format!("--dev-tokens must be true or false, not {other}").into()),
            };
            let mut gateway = Gateway::private(Hub::new(catalogue), ingress, limits)
                .with_audit(audit.clone())
                .with_identity(args.optional("identity").filter(|a| !a.is_empty()), dev_tokens);
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
        "gateway on {}: serving {serving} (public /v4/ws_public, private /v4/ws_private), audit log in {}",
        listener.local_addr()?,
        audit_dir.display()
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

/// A half that follows the node's feed and sends its book subscribers what changed, and keeps checking the API keys
/// its sessions use when it verifies keys.
fn start_half(gateway: Gateway, feed: &str) -> Arc<Gateway> {
    let gateway = Arc::new(gateway);
    follow_feed(&gateway, feed.to_string());
    conflate(&gateway);
    if gateway.identity.serves_keys() {
        identity::keep_checking(&gateway);
    }
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

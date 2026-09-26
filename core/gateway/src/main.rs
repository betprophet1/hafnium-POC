//! `venue-gateway`: interface 1, the REST and WebSocket gateway, in front of one node. The contract is
//! `interfaces/gateway/`; this is its first implementation, for the mini venue.

mod affinity;
mod api;
mod audit;
mod bootstrap;
#[cfg(test)]
mod fleet;
mod forwarded;
mod http;
mod hub;
mod identity;
mod limits;
mod metrics;
mod parlays;
mod price;
mod reactor;
mod reconstruct;
mod rest;
#[cfg(test)]
mod testing;
mod treasury;
mod ws;

use api::{Gateway, Limits};
use audit::Audit;
use forwarded::TrustedProxies;
use hub::Hub;
use limits::monotonic;
use metrics::{IngressWatch, Metrics};
use reactor::Reactors;
use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use venue_link::catalogue::Catalogue;
use venue_link::named;
use venue_link::submit::Submit;
use venue_link::wire::{self, Error};
use venue_telemetry::log;
use venue_telemetry::prometheus_client::registry::Registry;

const USAGE: &str = "usage:
  venue-gateway verify-audit FILE...
  venue-gateway reconstruct --journal FILE --results FILE --catalogue FILE AUDIT-FILE...
                                           renders every frame the audit log kept a reference to and checks it against
                                           the hash it kept; --results is what `venue-node replay` wrote
  venue-gateway --feed ADDR --catalogue FILE [--serve both] [--ingress ADDR] [--treasury ADDR] [--parlays ADDR]
                [--identity ADDR] [--dev-tokens true] [--listen 127.0.0.1:8081] [--per-second 200] [--burst 400] [--logons-per-minute 30]
                [--audit-dir ./gateway-audit] [--audit-instance HOSTNAME] [--audit-fsync true] [--trusted-proxies ADDRS]
                [--allowed-origins ORIGINS] [--environments FILE] [--from-view FILE]
                [--metrics ADDR] [--reactor-threads N] [--pin-reactor-threads true]

--serve is public, private or both: the halves of interface 1 this process serves. Public is market data on
/v4/ws_public and GET /v4/markets; private is orders and accounts on /v4/ws_private, /v4/account and /v4/orders,
and needs --ingress. With --treasury, the private half also passes deposits and withdrawals (/v4/transfers) to the
treasury there and sends its updates to private sessions; without it, /v4/transfers answers 503. --parlays is the
parlay service (venue-parlays) the private half sends requests for quote (/v4/rfqs) to; without it they are refused
as unavailable. --identity is the identity service (venue-identity) the private half verifies API keys with and passes
/v4/keys to; without it keys are refused as unavailable. --dev-tokens false refuses the catalogue's dev tokens, which
otherwise act as their accounts with every permission. Every request and what it was answered is written to the audit log in --audit-dir
(interfaces/audit/README.md); a gateway that cannot write it does not start, and one that stops being able to exits.
--environments names a JSON file listing the venues an app may point itself at, served as GET /v4/bootstrap and read
afresh on every request so that a file replaced under a running gateway is noticed; without it the route is not
offered (interfaces/gateway/README.md, choosing an environment).
--from-view names the reader's half of a snapshot pair (venue-view snapshot), so the hub starts holding the venue
as it was then rather than nothing. A venue founded on a snapshot holds no inputs in its journal at all, so a hub
that asks it for everything is sent nothing and serves an empty venue; in one of those this is required rather
than an optimisation. Its sequence is where the feed's own resume handshake then carries on from.
--allowed-origins is a comma-separated list of the origins a browser page may call this gateway from, such as
https://app.example.com or http://127.0.0.1:8081 for a front end in development. None by default, which serves a page
from the gateway's own origin and refuses every other, since a browser asks before it sends.
--trusted-proxies is a comma-separated list of addresses and networks (such as 10.42.0.0/16) of proxies in front of
the gateway, an ingress: on a connection from one, the audit log also records the client its X-Forwarded-For or
Forwarded header names. None by default, and those headers are never believed from any other address.
--metrics serves Prometheus metrics at http://ADDR/metrics, apart from the client-facing --listen (none by default);
VENUE_LOG_FORMAT=json writes the log as JSON lines (deploy/observability/metrics.md).
--reactor-threads sizes the event-loop pool serving connections; without it, the host's own core count is asked
(capped at 4, since past that on the hosts this was tuned against, contention between reactor threads for the
same connections' work outweighs the benefit). Size it explicitly on a host with more to give.
--pin-reactor-threads true pins each reactor thread to its own core instead of leaving it to the scheduler
(false by default, matching every host this ran on before it existed). Measured to matter: raising
--reactor-threads past the physical core count did nothing at all without it, since nothing kept a thread on
the core whose cache it had warmed. Public and private each pin their own pool to a different range of cores,
so the two never both claim core 0.
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
    if args.first().map(String::as_str) == Some("reconstruct") {
        return follow_references(&args[1..]).unwrap_or_else(|e| {
            eprintln!("venue-gateway: {e}");
            ExitCode::FAILURE
        });
    }
    match Args::parse(&args).and_then(|a| run(&a)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log::error("stopped", &[("reason", &e)]);
            ExitCode::FAILURE
        }
    }
}

/// `venue-gateway reconstruct --journal FILE --results FILE --catalogue FILE AUDIT-FILE...`: follows the audit log's
/// references into the journal (interfaces/audit/README.md, frames derived from the journal).
fn follow_references(args: &[String]) -> Result<ExitCode, Error> {
    let (mut flags, mut files) = (Vec::new(), Vec::new());
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg.starts_with("--") {
            flags.push(arg.clone());
            flags.push(it.next().cloned().ok_or_else(|| format!("{arg} needs a value"))?);
        } else {
            files.push(arg.clone());
        }
    }
    if files.is_empty() {
        return Err("usage: venue-gateway reconstruct --journal F --results F --catalogue F AUDIT-FILE...".into());
    }
    let args = Args::parse(&flags)?;
    let journal = PathBuf::from(args.get("journal", None)?);
    let results = PathBuf::from(args.get("results", None)?);
    let catalogue = PathBuf::from(args.get("catalogue", None)?);
    let outcome = reconstruct::reconstruct(&journal, &results, &catalogue, &files)?;
    for broken in &outcome.broken {
        println!("FAIL {broken}");
    }
    if outcome.broken.is_empty() {
        println!("{} frames rendered again from the journal, every one as the log says it was sent", outcome.checked);
        return Ok(ExitCode::SUCCESS);
    }
    println!("{} of {} frames are not what the log says was sent", outcome.broken.len(), outcome.checked);
    Ok(ExitCode::FAILURE)
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
    let reactor_threads = args
        .optional("reactor-threads")
        .map(|v| v.parse::<usize>())
        .transpose()
        .map_err(|e| format!("--reactor-threads: {e}"))?;
    let pin_reactor_threads = match args.get("pin-reactor-threads", Some("false"))?.as_str() {
        "true" => true,
        "false" => false,
        other => return Err(format!("--pin-reactor-threads must be true or false, not {other}").into()),
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
    let proxies = TrustedProxies::parse(&args.get("trusted-proxies", Some(""))?)
        .map_err(|e| format!("--trusted-proxies: {e}"))?;
    let origins: Vec<String> = args
        .get("allowed-origins", Some(""))?
        .split(',')
        .map(str::trim)
        .filter(|o| !o.is_empty())
        .map(String::from)
        .collect();
    for origin in &origins {
        if !origin.starts_with("http://") && !origin.starts_with("https://") {
            return Err(format!("--allowed-origins: {origin} is not an origin (scheme://host[:port])").into());
        }
    }
    let mut registry = Registry::default();
    venue_telemetry::build_info(&mut registry, "venue_gateway", env!("CARGO_PKG_VERSION"), &[]);
    let metrics = Metrics::register(&mut registry);
    // Before anything listens: a gateway that cannot keep its audit log takes no connection.
    let audit =
        Audit::start_measured(&audit_dir, &args.get("audit-instance", Some(&hostname))?, fsync, metrics.audit.clone())
            .map_err(|e| format!("audit log in {}: {e}", audit_dir.display()))?;
    // The reader's half of a snapshot pair, for a gateway serving a venue founded on a snapshot rather than on
    // genesis: that venue's journal holds no inputs at all, so a hub that asked for everything would be sent
    // nothing and serve an empty venue. Loaded once, before either half, so a bad path fails the process rather
    // than leaving one half up and wrong.
    let founding = match args.optional("from-view").filter(|p| !p.is_empty()) {
        Some(path) => {
            venue_link::view::View::load(std::path::Path::new(&path)).map_err(|e| format!("--from-view {path}: {e}"))?
        }
        None => venue_link::view::View::default(),
    };
    let halves = Halves {
        public: if public {
            // The origins go to this half too. Market data is what a page asks for first, and a browser will not
            // let it read an answer from an origin the server has not named — so without this a front end served
            // from anywhere else failed on its very first call, while every private route worked. The test harness
            // gives both halves the origins, which is why no test saw it: what was wrong was the wiring here.
            let gateway = Gateway::public(Hub::founded_on(Catalogue::load(&catalogue_file)?, founding.clone()), limits)
                .with_origins(origins.clone())
                .with_metrics(metrics.half(api::Half::Public));
            Some(start_half(gateway.with_audit(audit.clone()).with_trusted_proxies(proxies.clone()), &feed))
        } else {
            None
        },
        private: if private {
            let half_metrics = metrics.half(api::Half::Private);
            let watch = Arc::new(IngressWatch(Arc::clone(&half_metrics)));
            let report = Arc::new(|e: &str| log::warn("ingress connection", &[("error", &e)]));
            let ingress = Submit::spawn_watched(&args.get("ingress", None)?, Some(watch), report);
            let dev_tokens = match args.get("dev-tokens", Some("true"))?.as_str() {
                "true" => true,
                "false" => false,
                other => return Err(format!("--dev-tokens must be true or false, not {other}").into()),
            };
            // Where an app may point itself (`bootstrap.rs`). Without it the route is not offered at all, and an app
            // stays on the venue it was built knowing.
            let environments = match args.get("environments", Some(""))?.as_str() {
                "" => crate::bootstrap::Environments::none(),
                file => crate::bootstrap::Environments::from_file(file),
            };
            let mut gateway = Gateway::private(Hub::founded_on(catalogue, founding), ingress, limits)
                .with_origins(origins.clone())
                .with_environments(environments)
                .with_metrics(half_metrics)
                .with_audit(audit.clone())
                .with_trusted_proxies(proxies)
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
    if let Some(address) = args.optional("metrics").filter(|a| !a.is_empty()) {
        let bound = venue_telemetry::serve::spawn(&address, registry, None)?;
        log::info("metrics served", &[("address", &bound)]);
    }
    log::info(
        "serving",
        &[("listen", &listener.local_addr()?), ("halves", &serving), ("audit_dir", &audit_dir.display())],
    );
    serve(&halves, &listener, reactor_threads, pin_reactor_threads);
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
    named("gw-feed", move || {
        let half = follower.half.name();
        wire::follow_reporting(
            &feed,
            || {
                // Carries on where it left off. This is the reader with most to gain from it: its view used to be
                // rebuilt from the first input on every reconnect, so a moment's network trouble cost the whole
                // journal, and that cost grew for as long as the venue ran.
                let at = follower.hub_mut().resume();
                follower.metrics.feed_connected();
                at
            },
            || {
                // The node cannot honour that position — a different journal under the same address — so whatever
                // this gateway kept is a view of a history that never happened.
                log::warn("the feed is starting over; the view is being thrown away", &[("half", &half)]);
                follower.hub_mut().restart();
            },
            |unit| {
                let record = &unit.input["record"];
                let (sequence, at) = (record["sequence"].as_u64(), record["sequencedAt"].as_u64());
                // Timed from before the lock: all of it is this thread's, and this thread applies every unit
                // the venue produces, so what it costs is the headroom the whole half has (ADR-0002).
                let started = monotonic();
                follower.hub_mut().apply(&unit);
                follower.metrics.feed_apply.observe_duration(monotonic().duration_since(started));
                follower.metrics.feed_unit(sequence.unwrap_or(0), at.unwrap_or(0), venue_telemetry::wall_ns());
            },
            |e| log::warn("feed connection", &[("half", &half), ("feed", &feed), ("error", &e)]),
        );
    });
}

/// Sends book subscribers what changed, at a fixed rate.
fn conflate(gateway: &Arc<Gateway>) {
    let conflater = Arc::clone(gateway);
    named("gw-conflate", move || loop {
        // Book topics carry at most ten snapshots a second (interface 1 README, market data).
        thread::sleep(Duration::from_millis(100));
        let mut hub = conflater.hub_mut();
        hub.flush_books();
        // Amendments and cancels waiting for an order the feed has not delivered are answered once past their wait.
        hub.expire_awaiting();
    });
}

/// How far behind on what it has been sent a WebSocket client may fall before it is disconnected: a few seconds of
/// busy market data, far more than a client that is reading ever holds.
const MAX_BEHIND: usize = 4 * 1024 * 1024;

/// Accepts connections for ever and hands each one to an event loop of the half it is for, which serves it whether it
/// is a WebSocket session or a REST request. `reactor_threads` overrides the auto-detected default (`--reactor-threads`,
/// `VENUE_REACTOR_THREADS`); `None` keeps auto-detecting. `pin_reactor_threads` pins each one to its own core
/// (`--pin-reactor-threads`, `affinity.rs`).
fn serve(halves: &Halves, listener: &TcpListener, reactor_threads: Option<usize>, pin_reactor_threads: bool) {
    serve_with(halves, listener, MAX_BEHIND, reactor_threads, pin_reactor_threads);
}

/// `serve`, with a WebSocket client allowed to fall `max_behind` bytes behind.
fn serve_with(
    halves: &Halves,
    listener: &TcpListener,
    max_behind: usize,
    reactor_threads: Option<usize>,
    pin_reactor_threads: bool,
) {
    // Capped at 4 by default: past that, contention between reactor threads for the same connections' work
    // outweighs the benefit on the hosts this was tuned against. A host with more to give can say so directly
    // rather than wait for a higher default that would need retuning for whatever the next host looks like.
    let threads = reactor_threads.unwrap_or_else(|| thread::available_parallelism().map_or(2, |n| n.get().min(4)));
    // Public and private each start their own pool; giving them different offsets into the same core list
    // keeps the two from both pinning their first thread to core 0 (`Reactors::start`'s own doc comment).
    let start = |gateway: &Arc<Gateway>, base: usize| {
        Reactors::start(gateway, threads, max_behind, pin_reactor_threads.then_some(base))
    };
    let (public, private) = match (
        halves.public.as_ref().map(|g| start(g, 0)).transpose(),
        halves.private.as_ref().map(|g| start(g, threads)).transpose(),
    ) {
        (Ok(public), Ok(private)) => (public, private),
        (Err(e), _) | (_, Err(e)) => {
            log::error("event loops could not start", &[("error", &e)]);
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

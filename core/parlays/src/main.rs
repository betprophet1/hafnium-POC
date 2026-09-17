//! `venue-parlays`: the parlay service. See `venue_parlays` and `interfaces/gateway/README.md` (parlays).

use std::collections::BTreeMap;
use std::process::ExitCode;
use std::thread;
use venue_link::catalogue::Catalogue;
use venue_link::wire::Error;
use venue_parlays::service::{self, Config};
use venue_parlays::{operator, rules};

const USAGE: &str = "usage:
  venue-parlays --ingress ADDR --feed ADDR --catalogue FILE [--listen 127.0.0.1:7103] [--window-seconds 30]
                [--max-parlays 100] [--capacity small] [--responders parlay-responders.json]
                [--operator 127.0.0.1:7104] [--min-quantity 10]

Lists parlays when they are first requested, relays requests for quote to the subscribed sessions of approved
responders and their quotes to the requester through the gateways that connect to --listen, and settles every parlay
from its legs. It uses the core's ingress as an operator. The approved responders are kept in the --responders file,
which starts from the catalogue's \"responder\": true accounts when it does not exist, and are managed through the
operator API on --operator (GET /v1/responders, PUT and DELETE /v1/responders/ACCOUNT); --operator none serves no
operator API. A request is for at least --min-quantity contracts, a multiple of the parlay lot step (10). --capacity
must name the node's capacity profile: no parlay is listed that would take the venue past 95% of its events, markets
or price levels. Settled parlays are released by the engine a retention after settlement, which makes room again.
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

    /// A flag, else `VENUE_<FLAG>` from the environment, else the default.
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

    fn number(&self, key: &str, default: u64) -> Result<u64, Error> {
        Ok(self.get(key, Some(default.to_string().as_str()))?.parse()?)
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match Args::parse(&args).and_then(|a| run(&a)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("venue-parlays: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<(), Error> {
    let catalogue = Catalogue::load(&args.get("catalogue", None)?)?;
    let config = Config {
        ingress: args.get("ingress", None)?,
        feed: args.get("feed", None)?,
        listen: args.get("listen", Some("127.0.0.1:7103"))?,
        window_seconds: args.number("window-seconds", 30)?,
        max_parlays: usize::try_from(args.number("max-parlays", 100)?)?,
        capacity: {
            let profile = venue_link::catalogue::capacity_profile(&args.get("capacity", Some("small"))?)?;
            rules::Capacity { events: profile.events, instruments: profile.instruments, levels: profile.levels }
        },
        responders: Some(args.get("responders", Some("parlay-responders.json"))?.into()),
        min_quantity: args.number("min-quantity", rules::LOT_STEP)?,
    };
    let service = service::start(catalogue, &config)?;
    let operator = args.get("operator", Some("127.0.0.1:7104"))?;
    if operator != "none" {
        let listener = std::net::TcpListener::bind(&operator)?;
        eprintln!("parlay service: operator API on {}", listener.local_addr()?);
        operator::serve(listener, service);
    }
    loop {
        thread::park();
    }
}

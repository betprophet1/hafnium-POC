//! `venue-gateway`: interface 1, the REST and WebSocket gateway, in front of one node. The contract is
//! `interfaces/gateway/`; this is its first implementation, for the mini venue.

mod api;
mod http;
mod hub;
mod limits;
mod price;
mod rest;
mod ws;

use api::{Gateway, Limits};
use hub::Hub;
use std::collections::BTreeMap;
use std::net::{TcpListener, TcpStream};
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use venue_link::catalogue::Catalogue;
use venue_link::submit::Submit;
use venue_link::wire::{self, Error};

const USAGE: &str = "usage:
  venue-gateway --ingress ADDR --feed ADDR --catalogue FILE [--listen 127.0.0.1:8081]
                [--per-second 200] [--burst 400] [--logons-per-minute 30]

REST on /v4/..., WebSocket on /v4/ws. Every flag can also be given as VENUE_<FLAG>, for example VENUE_FEED.";

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
    let gateway = Arc::new(Gateway::new(Hub::new(catalogue), Submit::spawn(&args.get("ingress", None)?), limits));
    let feed = args.get("feed", None)?;
    let follower = Arc::clone(&gateway);
    thread::spawn(move || {
        wire::follow(&feed, || follower.hub().restart(), |unit| follower.hub().apply(&unit));
    });
    let conflater = Arc::clone(&gateway);
    thread::spawn(move || loop {
        // Book topics carry at most ten snapshots a second (interface 1 README, market data).
        thread::sleep(Duration::from_millis(100));
        conflater.hub().flush_books();
    });
    let listener = TcpListener::bind(args.get("listen", Some("127.0.0.1:8081"))?)?;
    eprintln!("gateway on {}: REST /v4, WebSocket /v4/ws", listener.local_addr()?);
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let gateway = Arc::clone(&gateway);
        thread::spawn(move || connection(&gateway, stream));
    }
    Ok(())
}

/// Routes a connection by its request line without consuming it, so the WebSocket handshake reads it whole.
fn connection(gateway: &Gateway, stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    const WS: &[u8] = b"GET /v4/ws";
    let mut head = [0u8; WS.len()];
    for _ in 0..400 {
        match stream.peek(&mut head) {
            Ok(n) if n >= WS.len() => break,
            Ok(0) | Err(_) => return,
            Ok(_) => thread::sleep(Duration::from_millis(5)),
        }
    }
    if head == WS {
        ws::serve(gateway, stream);
    } else {
        rest::handle(gateway, &stream);
    }
}

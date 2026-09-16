//! `venue-sim`: the mini venue's actors. See `core/sim/README.md`.

mod bots;
mod client;
mod session;
mod web;
mod workload;

use std::collections::BTreeMap;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use venue_link::catalogue::Catalogue;
use venue_link::submit::Submit;
use venue_link::view;
use venue_link::wire::{Error, Ids, Ingress};

const USAGE: &str = "usage:
  venue-sim seed  --ingress ADDR --catalogue FILE [--capacity small]
  venue-sim web   --ingress ADDR --feed ADDR --gateway ADDR --catalogue FILE [--listen 127.0.0.1:8080]
  venue-sim maker --gateway ADDR [--token dev-synthetic-maker] [--size 10] [--levels 3] [--spread 1]
                  [--interval-ms 700] [--seed 1]
  venue-sim taker --gateway ADDR [--token dev-synthetic-taker] [--max-quantity 5] [--chance 250]
                  [--sweep-chance 30] [--interval-ms 1000] [--seed 2]
  venue-sim workload --ingress ADDR [--books 600] [--hot 15] [--warm 100] [--accounts 500]
                  [--makers 5] [--seconds 60] [--seed 7] [--order-limit 12000]
                  [--id-window-seconds 10] [--depth 16]

The bots are clients of the gateway. seed, workload and the web page's settlement tab are operator tools and use
the core's ingress and feed. workload puts a venue-shaped load on a core on its own, with no gateway in the way.
Every flag can also be given as VENUE_<FLAG> in the environment, for example VENUE_GATEWAY.";

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
    let Some(command) = args.first() else {
        eprintln!("{USAGE}");
        return ExitCode::from(64);
    };
    let outcome = Args::parse(&args[1..]).and_then(|a| match command.as_str() {
        "seed" => seed(&a),
        "web" => web(&a),
        "maker" => maker(&a),
        "taker" => taker(&a),
        "workload" => workload(&a),
        _ => Err(USAGE.into()),
    });
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("venue-sim: {e}");
            ExitCode::FAILURE
        }
    }
}

/// A venue-shaped load straight onto a core: many books, most of them quiet, a few busy, and orders that trade.
fn workload(args: &Args) -> Result<(), Error> {
    let config = workload::Config {
        books: args.number("books", 600)?,
        hot: args.number("hot", 15)?,
        warm: args.number("warm", 100)?,
        accounts: args.number("accounts", 500)?,
        makers: args.number("makers", 5)?,
        // Beneath the engine's order slab, and a window the order rate can live inside: see Config.
        order_limit: args.number("order-limit", 12_000)?,
        id_window_seconds: args.number("id-window-seconds", 10)?,
        depth: usize::try_from(args.number("depth", 16)?)?,
        seconds: args.number("seconds", 60)?,
        seed: args.number("seed", 7)?,
    };
    // Pipelined, so the generator measures the venue rather than its own waiting: see Config::depth.
    let ingress = Submit::spawn(&args.get("ingress", None)?);
    workload::run(&ingress, &config)
}

/// Lists the demo markets and funds the accounts, waiting up to two minutes for the core to accept connections.
fn seed(args: &Args) -> Result<(), Error> {
    let catalogue = Catalogue::load(&args.get("catalogue", None)?)?;
    let mut ingress = Ingress::new(&args.get("ingress", None)?);
    // The order limit is set beneath the node's order ceiling, so this must name the profile the node runs with.
    let ceiling = venue_link::catalogue::order_ceiling(&args.get("capacity", Some("small"))?)?;
    let mut inputs = catalogue.seed(ceiling)?.into_iter().peekable();
    let mut attempts = 0;
    while let Some(input) = inputs.peek() {
        match ingress.send(input.clone()) {
            Ok(results) => {
                for r in results.iter().filter(|r| r["msg"] == "CommandApplied") {
                    println!("{} {}", r["commandId"].as_str().unwrap_or(""), r["outcome"].as_str().unwrap_or(""));
                }
                inputs.next();
            }
            // Resending an input the core already took is answered DUPLICATE, so a retry is safe.
            Err(e) if attempts < 60 => {
                attempts += 1;
                eprintln!("waiting for the core: {e}");
                thread::sleep(Duration::from_secs(2));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn web(args: &Args) -> Result<(), Error> {
    let catalogue = Catalogue::load(&args.get("catalogue", None)?)?;
    let live = view::Live::follow(&args.get("feed", None)?);
    let ingress = Ingress::new(&args.get("ingress", None)?);
    let app = web::Web::new(catalogue, live, ingress, Ids::new('w')?, args.get("gateway", Some("127.0.0.1:8081"))?);
    web::serve(&args.get("listen", Some("127.0.0.1:8080"))?, Arc::new(app))
}

fn maker(args: &Args) -> Result<(), Error> {
    let config = bots::Maker {
        size: args.number("size", 10)?,
        levels: args.number("levels", 3)?,
        half_spread: args.number("spread", 1)?,
        interval: Duration::from_millis(args.number("interval-ms", 700)?),
        seed: args.number("seed", 1)?,
    };
    let (gateway, token) = (args.get("gateway", None)?, args.get("token", Some("dev-synthetic-maker"))?);
    let api = client::Api::new(&gateway, &token);
    bots::maker(&api, &gateway, &token, &config, Ids::new('m')?)
}

fn taker(args: &Args) -> Result<(), Error> {
    let config = bots::Taker {
        max_quantity: args.number("max-quantity", 5)?,
        per_mille: args.number("chance", 250)?,
        sweep_per_mille: args.number("sweep-chance", 30)?,
        interval: Duration::from_millis(args.number("interval-ms", 1000)?),
        seed: args.number("seed", 2)?,
    };
    let api = client::Api::new(&args.get("gateway", None)?, &args.get("token", Some("dev-synthetic-taker"))?);
    bots::taker(&api, &config, Ids::new('t')?)
}

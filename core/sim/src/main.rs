//! `venue-sim`: the mini venue's actors. See `core/sim/README.md`.

mod bots;
mod calendar;
mod cases;
mod client;
mod gwload;
mod oracle;
mod resolution;
mod retail;
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
                  [--orders-per-second N | --id-window-seconds N]
                  the client order ID window and the engine's duplicate table are one sizing rule: a group
                  takes `duplicates / window` new orders a second and then fail-stops, which is about 36/s on
                  `small` and 2 330/s on `scale` at the two-hour default. Found a venue that will see more on
                  a shorter window -- no profile is large enough to hold a two-hour window at a high order
                  rate, and the window can only be shortened by replacing both group parameters.
  venue-sim web   --ingress ADDR --feed ADDR --gateway ADDR --catalogue FILE [--listen 127.0.0.1:8080]
                  [--public-gateway ADDR] [--oracle 127.0.0.1:8082] [--treasury 127.0.0.1:8083] [--from-view FILE]
  venue-sim oracle --ingress ADDR --feed ADDR --catalogue FILE --calendar FILE --database-url URL [--capacity small]
                  [--listen 127.0.0.1:8082] [--migrate-on-start false] [--store postgres|memory] [--from-view FILE]
  venue-sim oracle migrate --database-url URL
  venue-sim maker --gateway ADDR [--public-gateway ADDR] [--token dev-synthetic-maker] [--size 10] [--levels 3] [--spread 1]
                  [--interval-ms 700] [--seed 1] [--margin 100]
  venue-sim taker --gateway ADDR [--public-gateway ADDR] [--token dev-synthetic-taker] [--max-quantity 5] [--chance 250]
                  [--sweep-chance 30] [--interval-ms 1000] [--seed 2]
  venue-sim workload --ingress ADDR [--books 600] [--hot 15] [--warm 100] [--accounts 500]
                  [--makers 5] [--seconds 60] [--seed 7] [--depth 16] [--capacity small]
                  [--order-limit N] [--id-window-seconds N]
                  (the last two default from the node's capacity profile: 3/4 of its orders, and its
                  client order ID table's entries / 120 000 seconds)
  venue-sim catalogue --catalogue FILE --out FILE [--accounts 0] [--token-prefix dev-load-]
                  [--first-id 9600000] [--cents 100000000]
                  writes the catalogue with `accounts` more accounts on the end, one per connection a load run
                  means to make: a gateway only knows the dev tokens its catalogue gave it, so they have to
                  exist before it starts. Seed from the written file and start the gateway on it, and
                  `gwload --token-prefix` then has an account per connection.
  venue-sim gwload --gateway ADDR [--token dev-market-maker] [--instrument 10010] [--connections 1] [--seconds 10]
                  [--instruments 1] [--ingress ADDR] [--token-prefix PREFIX]
                  [--mode amend|trade] [--makers N] [--watchers N] [--public-gateway ADDR]
                  --mode trade makes the connections a venue's own shape instead of one that never trades:
                  the first --makers of them (a quarter by default) rest a large sell, amending it back up to
                  size, and the rest cross it immediate-or-cancel, so every taker request is a fill with the
                  executions and sleeve updates that follow. --watchers N adds N connections that only
                  subscribe to the book on the public half and read, which is the market-data fan-out the
                  order-entry measurement leaves out entirely.
                  the gateway-facing counterpart to `venue-node load`: each connection logs in over
                  /v4/ws_private, places one resting order, then amends it back and forth until the deadline,
                  reporting messages/s and round-trip percentiles the same way.
                  --instruments N > 1 spreads connections across N synthetic markets instead of one, most-popular-
                  first with each one's share roughly half the last (needs --ingress, to list them on first).

The bots are clients of the gateway. --gateway serves the private half (orders, accounts, sessions); --public-gateway,
the gateway serving the public half (markets, books), defaults to it. The maker subscribes to requests for quote and answers them when its account is an
approved responder, adding --margin thousandths to its price for a parlay. seed, workload and the dummy oracle are operator
tools and use the core's ingress and feed; the web page's settlement tab drives the dummy oracle's control API.
workload puts a venue-shaped load on a core on its own, with no gateway in the way. The dummy oracle keeps its
resolution cases in PostgreSQL (--database-url postgres://USER:PASSWORD@HOST:5432/DATABASE, --database-schema oracle,
--database-tls, --database-ca-file: core/store/README.md) and waits for its schema, which oracle migrate applies;
--store memory keeps them in memory only.
--from-view names the reader's half of a snapshot pair (venue-view snapshot), to start from the venue as it was
then rather than from the beginning. A venue founded on a snapshot holds no inputs in its journal at all, so a
reader that asks it for everything is sent nothing and shows an empty venue; in one of those this flag is required
rather than an optimisation.
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

    /// A flag that may simply not be given: `None` when neither it nor `VENUE_<FLAG>` is set.
    fn optional(&self, key: &str) -> Option<String> {
        self.get(key, Some("")).ok().filter(|value| !value.is_empty())
    }

    /// The reader this process follows the feed with: from a view snapshot when one is named, else from the
    /// beginning. A venue founded on a snapshot has no inputs in its journal to send a reader that asks for
    /// everything, so in one of those this is the difference between the venue and an empty screen.
    fn live(&self) -> Result<Arc<view::Live>, Error> {
        let feed = self.get("feed", None)?;
        match self.optional("from-view") {
            Some(path) => Ok(view::Live::follow_from(&feed, std::path::Path::new(&path))?),
            None => Ok(view::Live::follow(&feed)),
        }
    }

    /// The dummy oracle's database, if a URL is given.
    fn database(&self) -> Result<Option<venue_store::Config>, Error> {
        Ok(venue_store::Config::from_settings(|k| self.get(k, None).ok(), "venue-sim oracle", "oracle")?)
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
    if command == "oracle" && args.get(1).map(String::as_str) == Some("migrate") {
        return match Args::parse(&args[2..]).and_then(|a| oracle_migrate(&a)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("venue-sim: {e}");
                ExitCode::FAILURE
            }
        };
    }
    let outcome = Args::parse(&args[1..]).and_then(|a| match command.as_str() {
        "seed" => seed(&a),
        "web" => web(&a),
        "oracle" => oracle(&a),
        "maker" => maker(&a),
        "taker" => taker(&a),
        "workload" => workload(&a),
        "gwload" => gwload(&a),
        "catalogue" => catalogue(&a),
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

/// Applies the dummy oracle's migrations and exits: the chart's migrate job, or an operator.
fn oracle_migrate(args: &Args) -> Result<(), Error> {
    let config = args.database()?.ok_or("--database-url (or VENUE_DATABASE_URL) is required")?;
    let db = venue_store::Database::connect_retrying(&config, "venue-sim oracle")?;
    let applied = db.migrate(&venue_store::migrations_dir!())?;
    eprintln!(
        "venue-sim oracle: schema {} of {} is at migration {} (applied now: {applied:?})",
        config.schema,
        config.describe(),
        cases::MIGRATIONS.len()
    );
    Ok(())
}

/// A venue-shaped load straight onto a core: many books, most of them quiet, a few busy, and orders that trade.
fn workload(args: &Args) -> Result<(), Error> {
    // Both limits follow the node's capacity profile, so this must name the profile the node runs with. The order
    // limit is where the seed puts it; the client order ID window keeps the duplicate table half full at the ~60 000
    // new orders a second a pipelined run reaches (the table's entries over 120 000 seconds). A fixed ten seconds
    // filled `small`'s 262 144 entries and fail-stopped the core about seven seconds into a run.
    let profile = venue_link::catalogue::capacity_profile(&args.get("capacity", Some("small"))?)?;
    let window = (profile.duplicates as u64 / 120_000).max(1);
    let config = workload::Config {
        books: args.number("books", 600)?,
        hot: args.number("hot", 15)?,
        warm: args.number("warm", 100)?,
        accounts: args.number("accounts", 500)?,
        makers: args.number("makers", 5)?,
        order_limit: args.number("order-limit", venue_link::catalogue::active_order_limit(profile.orders))?,
        id_window_seconds: args.number("id-window-seconds", window)?,
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
    // The order and listing limits are the node's own tables, so this must name the profile the node runs with.
    let profile = venue_link::catalogue::capacity_profile(&args.get("capacity", Some("small"))?)?;
    // The client order ID window and the duplicate table are one sizing rule: the group can take
    // `duplicates / window` new orders a second before the table fills and the engine fail-stops. A venue
    // expecting more says so with --orders-per-second and is founded on a window that fits, since the window
    // can only be shortened by replacing both group parameters and no profile is large enough to make a
    // two-hour window hold a high order rate. --id-window-seconds overrides the arithmetic outright.
    let window = match args.optional("orders-per-second") {
        Some(rate) => venue_link::catalogue::window_for(
            &profile,
            rate.parse::<u64>().map_err(|e| format!("--orders-per-second: {e}"))?,
        ),
        None => venue_link::catalogue::CLIENT_ORDER_ID_WINDOW_SECONDS,
    };
    let window = args.number("id-window-seconds", window)?;
    if window < venue_link::catalogue::CLIENT_ORDER_ID_WINDOW_SECONDS {
        eprintln!(
            "seeding with a {window}s client order ID window (the default is {}s): the group takes up to {} new \
             orders a second before its duplicate table fills",
            venue_link::catalogue::CLIENT_ORDER_ID_WINDOW_SECONDS,
            profile.duplicates as u64 / window.max(1)
        );
    }
    let mut inputs = catalogue.seed_with(&profile, window)?.into_iter().peekable();
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
    let live = args.live()?;
    let (gateway, oracle) = (args.get("gateway", Some("127.0.0.1:8081"))?, args.get("oracle", Some("127.0.0.1:8082"))?);
    let public = args.get("public-gateway", Some(&gateway))?;
    let treasury = args.get("treasury", Some("127.0.0.1:8083"))?;
    let app = web::Web::new(catalogue, live, session::Gateways { private: gateway, public }, oracle, treasury);
    web::serve(&args.get("listen", Some("127.0.0.1:8080"))?, Arc::new(app))
}

/// The dummy oracle: lists, runs and settles rounds on the calendar, and serves its control API.
fn oracle(args: &Args) -> Result<(), Error> {
    let calendar = calendar::Calendar::load(&args.get("calendar", None)?)?;
    let catalogue = Catalogue::load(&args.get("catalogue", None)?)?;
    // Listing stops short of the node's capacities, so this must name the profile the node runs with.
    let profile = venue_link::catalogue::capacity_profile(&args.get("capacity", Some("small"))?)?;
    let capacity =
        oracle::Capacity { events: profile.events, instruments: profile.instruments, levels: profile.levels };
    let live = args.live()?;
    let ingress = Ingress::new(&args.get("ingress", None)?);
    let store = match args.get("store", Some("postgres"))?.as_str() {
        "postgres" => {
            let config = args.database()?.ok_or("--database-url (or VENUE_DATABASE_URL) is required: the dummy oracle keeps its cases in PostgreSQL; --store memory keeps them in memory only")?;
            eprintln!("oracle: cases in {}", config.describe());
            let db = venue_store::Database::connect_retrying(&config, "venue-sim oracle")?;
            if args.get("migrate-on-start", Some("false"))? == "true" {
                db.migrate(&venue_store::migrations_dir!())?;
            }
            db.wait_for_schema(cases::MIGRATIONS, "venue-sim oracle")?;
            Some(cases::CaseStore::new(db))
        }
        "memory" => {
            eprintln!("oracle: cases kept in memory only (--store memory): a restart opens them afresh");
            None
        }
        other => return Err(format!("--store must be postgres or memory, not {other}").into()),
    };
    let oracle = oracle::Oracle::new(calendar, catalogue, capacity, live, ingress, store)?;
    oracle::serve(&args.get("listen", Some("127.0.0.1:8082"))?, Arc::new(oracle))
}

fn maker(args: &Args) -> Result<(), Error> {
    let config = bots::Maker {
        size: args.number("size", 10)?,
        levels: args.number("levels", 3)?,
        half_spread: args.number("spread", 1)?,
        interval: Duration::from_millis(args.number("interval-ms", 700)?),
        seed: args.number("seed", 1)?,
        margin_per_mille: args.number("margin", 100)?,
    };
    let (gateway, token) = (args.get("gateway", None)?, args.get("token", Some("dev-synthetic-maker"))?);
    let public = args.get("public-gateway", Some(&gateway))?;
    let api = client::Api::new(&gateway, &token).with_public(&public);
    let gateways = session::Gateways { private: gateway, public };
    bots::maker(&api, &gateways, &token, &config, Ids::new('m')?, Ids::new('q')?)
}

fn gwload(args: &Args) -> Result<(), Error> {
    let config = gwload::Config {
        gateway: args.get("gateway", None)?,
        token: args.get("token", Some("dev-market-maker"))?,
        instrument: args.number("instrument", 10010)?,
        connections: usize::try_from(args.number("connections", 1)?)?,
        seconds: args.number("seconds", 10)?,
        ingress: args.optional("ingress"),
        instruments: usize::try_from(args.number("instruments", 1)?)?,
        token_prefix: args.optional("token-prefix").filter(|p| !p.is_empty()),
        mode: match args.get("mode", Some("amend"))?.as_str() {
            "amend" => gwload::Mode::Amend,
            "trade" => gwload::Mode::Trade,
            other => return Err(format!("--mode must be amend or trade, not {other}").into()),
        },
        makers: match args.optional("makers") {
            Some(m) => Some(usize::try_from(m.parse::<u64>().map_err(|e| format!("--makers: {e}"))?)?),
            None => None,
        },
        orders_per_trade: args.number("orders-per-trade", 10)?,
        retail: usize::try_from(args.number("retail", 0)?)?,
        retail_order_seconds: args.number("retail-order-seconds", 10)?,
        retail_books: usize::try_from(args.number("retail-books", 2)?)?,
        retail_workers: usize::try_from(
            args.number("retail-workers", std::thread::available_parallelism().map_or(8, |n| n.get() as u64))?,
        )?,
        watchers: usize::try_from(args.number("watchers", 0)?)?,
        public_gateway: args.optional("public-gateway").filter(|a| !a.is_empty()),
    };
    gwload::run(config)
}

/// Writes a catalogue with more accounts on the end, for a load run that wants one per connection. The accounts
/// are only what `Catalogue::seed` and the gateway need of one: an id, the cents to grant it, and the dev token
/// that logs in as it.
fn catalogue(args: &Args) -> Result<(), Error> {
    let base = args.get("catalogue", None)?;
    let out = args.get("out", None)?;
    let accounts = usize::try_from(args.number("accounts", 0)?)?;
    let prefix = args.get("token-prefix", Some("dev-load-"))?;
    let first = args.number("first-id", 9_600_000)?;
    let cents = args.number("cents", 100_000_000)?;

    let text = std::fs::read_to_string(&base).map_err(|e| format!("reading {base}: {e}"))?;
    let mut doc: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("parsing {base}: {e}"))?;
    let list = doc["accounts"].as_array_mut().ok_or("the catalogue has no accounts")?;
    let taken: Vec<u64> = list.iter().filter_map(|a| a["id"].as_u64()).collect();
    for i in 0..accounts {
        let id = first + i as u64;
        if taken.contains(&id) {
            return Err(format!("account {id} is already in {base}; pass a --first-id past its accounts").into());
        }
        list.push(serde_json::json!({
            "id": id,
            "name": format!("Load {i}"),
            "tab": serde_json::Value::Null,
            "cents": cents,
            "token": format!("{prefix}{i}"),
        }));
    }
    let total = list.len();
    std::fs::write(&out, serde_json::to_string_pretty(&doc)?).map_err(|e| format!("writing {out}: {e}"))?;
    eprintln!(
        "wrote {out}: {total} accounts, {accounts} of them for load ({prefix}0..{prefix}{})",
        accounts.max(1) - 1
    );
    Ok(())
}

fn taker(args: &Args) -> Result<(), Error> {
    let config = bots::Taker {
        max_quantity: args.number("max-quantity", 5)?,
        per_mille: args.number("chance", 250)?,
        sweep_per_mille: args.number("sweep-chance", 30)?,
        interval: Duration::from_millis(args.number("interval-ms", 1000)?),
        seed: args.number("seed", 2)?,
    };
    let gateway = args.get("gateway", None)?;
    let api = client::Api::new(&gateway, &args.get("token", Some("dev-synthetic-taker"))?)
        .with_public(&args.get("public-gateway", Some(&gateway))?);
    bots::taker(&api, &config, Ids::new('t')?)
}

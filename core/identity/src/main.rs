//! `venue-identity`: the identity service of test environments. See `core/identity/README.md`.

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::process::ExitCode;
use std::thread;
use venue_identity::Settings;

const USAGE: &str = "usage:
  venue-identity --catalogue FILE --store FILE [--listen 127.0.0.1:8084] [--fsync true]

Keeps the API keys of the catalogue's accounts in the store, an append-only file, and answers the gateways' questions
about them. Every flag can also be given as VENUE_<FLAG>, for example VENUE_STORE.";

struct Args(BTreeMap<String, String>);

impl Args {
    fn parse(args: &[String]) -> Result<Self, String> {
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
    fn get(&self, key: &str, default: Option<&str>) -> Result<String, String> {
        if let Some(v) = self.0.get(key) {
            return Ok(v.clone());
        }
        let env = format!("VENUE_{}", key.to_uppercase().replace('-', "_"));
        std::env::var(&env)
            .ok()
            .or_else(|| default.map(str::to_string))
            .ok_or_else(|| format!("--{key} (or {env}) is required"))
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match Args::parse(&args).and_then(|a| run(&a)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("venue-identity: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<(), String> {
    let fsync = match args.get("fsync", Some("true"))?.as_str() {
        "true" => true,
        "false" => false,
        other => return Err(format!("--fsync must be true or false, not {other}")),
    };
    let settings = Settings { catalogue: args.get("catalogue", None)?, store: args.get("store", None)?.into(), fsync };
    let listen = args.get("listen", Some("127.0.0.1:8084"))?;
    let listener = TcpListener::bind(&listen).map_err(|e| format!("{listen}: {e}"))?;
    eprintln!("identity on {listen}, store {}", settings.store.display());
    venue_identity::start(&settings, listener)?;
    loop {
        thread::park();
    }
}

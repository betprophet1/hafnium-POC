//! `venue-store`: how the services keep their records in PostgreSQL (blueprint §18: small records that change). Each
//! service owns a database of its own, or at least a schema and a role of its own, and nothing here lets one service
//! reach another's tables. See `core/store/README.md` and the decisions in `core/store/decisions.md`.
//!
//! - [`Config`]: a database URL (`VENUE_DATABASE_URL`), the schema the service's tables live in, the pool size and TLS
//!   (`disable`, `prefer`, `require`, `verify-ca`, `verify-full`, with an optional CA file). Self-hosted and managed
//!   databases differ only in these values.
//! - [`Database`]: a small blocking pool that fits the services' threads. A connection that broke is dropped rather
//!   than handed out again, and a new one is made on the next checkout, so a database that comes back is used again
//!   without a restart.
//! - [`Database::migrate`]: the service's migrations, applied by Atlas, which keeps their history and takes its own
//!   lock. A database newer than the binary, or a migration edited after it was applied, stops the service rather
//!   than being guessed at: the first of those is Atlas's history, the second this crate's record of the SQL each
//!   applied version held, which Atlas does not keep.
//! - [`Database::transaction`]: a closure run in a transaction, committed if it returns `Ok`.
//! - [`testing`]: a fresh schema on `VENUE_TEST_DATABASE_URL` for tests, or a clear line saying they were skipped.

use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
use postgres::config::SslMode;
use postgres::{Client, Transaction};
use postgres_openssl::MakeTlsConnector;
use sha2::{Digest, Sha256};
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use postgres;

/// How long a checkout waits for a connection, and a connection attempt for the server.
const WAIT: Duration = Duration::from_secs(5);
/// A write that the network swallowed fails after this long rather than holding its caller for the kernel's default
/// of a quarter of an hour.
const TCP_USER_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest one statement may run. The services' statements are single rows and small sets.
const STATEMENT_TIMEOUT_MS: u64 = 30_000;

/// Something the database did not do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The database could not be reached or the connection broke: nothing is known to have been written, and trying
    /// again later may work.
    Unavailable(String),
    /// The database answered and refused: a constraint, a trigger, a statement it could not run, or a check of the
    /// service's own in the transaction.
    Refused(String),
}

impl Error {
    pub const fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }

    pub fn refused(message: impl Into<String>) -> Self {
        Self::Refused(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(m) => write!(f, "the database is unavailable: {m}"),
            Self::Refused(m) => write!(f, "the database refused: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<postgres::Error> for Error {
    fn from(e: postgres::Error) -> Self {
        match e.as_db_error() {
            Some(db) => Self::Refused(format!("{} ({})", db.message(), db.code().code())),
            None => Self::Unavailable(e.to_string()),
        }
    }
}

impl From<Error> for String {
    fn from(e: Error) -> Self {
        e.to_string()
    }
}

/// How a connection uses TLS, in libpq's terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tls {
    /// Never.
    Disable,
    /// If the server offers it, without checking its certificate. libpq's default.
    Prefer,
    /// Always, without checking the certificate: encrypted, not authenticated.
    Require,
    /// Always, and the certificate must chain to a trusted CA.
    VerifyCa,
    /// Always, the certificate must chain to a trusted CA and name the host. Use it wherever the network between the
    /// service and the database is not trusted, managed databases included.
    VerifyFull,
}

impl Tls {
    /// The name libpq and Atlas both know it by.
    pub fn name(self) -> &'static str {
        match self {
            Self::Disable => "disable",
            Self::Prefer => "prefer",
            Self::Require => "require",
            Self::VerifyCa => "verify-ca",
            Self::VerifyFull => "verify-full",
        }
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        Ok(match text {
            "disable" => Self::Disable,
            "prefer" => Self::Prefer,
            "require" => Self::Require,
            "verify-ca" => Self::VerifyCa,
            "verify-full" => Self::VerifyFull,
            other => {
                return Err(format!("TLS mode must be disable, prefer, require, verify-ca or verify-full, not {other}"))
            }
        })
    }
}

/// Where a service's records are and how to reach them.
#[derive(Clone)]
pub struct Config {
    /// `postgres://user:password@host:port/database`, with libpq's `sslmode` and `sslrootcert` understood too.
    pub url: String,
    /// The schema the service's tables live in, which its migrations create if the role may.
    pub schema: String,
    pub pool_size: usize,
    /// `None`: the URL's `sslmode`, else `prefer`.
    pub tls: Option<Tls>,
    /// A PEM file of the CAs to trust, for `verify-ca` and `verify-full`; `None`: the URL's `sslrootcert`, else the
    /// system's.
    pub ca_file: Option<String>,
    /// The name the service shows in `pg_stat_activity`.
    pub application: String,
}

impl Config {
    /// A configuration from a service's settings, each looked up by flag name (`database-url`, `database-schema`,
    /// `database-pool-size`, `database-tls`, `database-ca-file`), the way the services read `--flag` or
    /// `VENUE_<FLAG>`. `None` if no URL is set.
    pub fn from_settings(
        get: impl Fn(&str) -> Option<String>,
        application: &str,
        default_schema: &str,
    ) -> Result<Option<Self>, String> {
        let Some(url) = get("database-url").filter(|u| !u.is_empty()) else { return Ok(None) };
        let pool_size = match get("database-pool-size") {
            None => 4,
            Some(v) => v.parse().ok().filter(|n| (1..=64).contains(n)).ok_or("--database-pool-size must be 1 to 64")?,
        };
        let config = Self {
            url,
            schema: get("database-schema").unwrap_or_else(|| default_schema.to_string()),
            pool_size,
            tls: get("database-tls").map(|t| Tls::parse(&t)).transpose()?,
            ca_file: get("database-ca-file").filter(|f| !f.is_empty()),
            application: application.to_string(),
        };
        valid_schema(&config.schema)?;
        Ok(Some(config))
    }

    /// The URL split into the client's configuration, the TLS mode and CA file, with the parameters the client does
    /// not know (`verify-ca`, `verify-full`, `sslrootcert`) taken out.
    fn resolve(&self) -> Result<(postgres::Config, Tls, Option<String>), String> {
        let (base, query) = self.url.split_once('?').unwrap_or((self.url.as_str(), ""));
        let mut tls = None;
        let mut ca = None;
        let mut kept = Vec::new();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            match pair.split_once('=') {
                Some(("sslmode", mode)) => tls = Some(Tls::parse(mode)?),
                Some(("sslrootcert", file)) => ca = Some(file.to_string()),
                _ => kept.push(pair),
            }
        }
        let url = if kept.is_empty() { base.to_string() } else { format!("{base}?{}", kept.join("&")) };
        let mut pg: postgres::Config = url.parse().map_err(|e| format!("the database URL: {e}"))?;
        let tls = self.tls.or(tls).unwrap_or(Tls::Prefer);
        pg.ssl_mode(match tls {
            Tls::Disable => SslMode::Disable,
            Tls::Prefer => SslMode::Prefer,
            Tls::Require | Tls::VerifyCa | Tls::VerifyFull => SslMode::Require,
        });
        if pg.get_connect_timeout().is_none() {
            pg.connect_timeout(WAIT);
        }
        pg.tcp_user_timeout(TCP_USER_TIMEOUT).keepalives(true).keepalives_idle(Duration::from_secs(30));
        pg.application_name(&self.application);
        Ok((pg, tls, self.ca_file.clone().or(ca)))
    }

    /// The URL as Atlas takes it: the service's own, with the schema its tables live in and the TLS the settings ask
    /// for, so that Atlas connects exactly where the service does. Atlas reads libpq's `sslmode` and `sslrootcert`.
    pub fn atlas_url(&self) -> String {
        let (base, query) = self.url.split_once('?').unwrap_or((self.url.as_str(), ""));
        let mut kept: Vec<String> = query
            .split('&')
            // The schema and, where the settings give one, the TLS mode are this method's to say; anything else the
            // URL carries is kept as it is.
            .filter(|p| {
                let ours = p.starts_with("search_path=") || (self.tls.is_some() && p.starts_with("sslmode="));
                !p.is_empty() && !ours
            })
            .map(str::to_string)
            .collect();
        kept.push(format!("search_path={}", self.schema));
        if let Some(tls) = self.tls {
            kept.push(format!("sslmode={}", tls.name()));
        }
        if let Some(ca) = &self.ca_file {
            kept.push(format!("sslrootcert={ca}"));
        }
        format!("{base}?{}", kept.join("&"))
    }

    /// Where the database is, for logs: host, port and database, never the user or password.
    pub fn describe(&self) -> String {
        match self.resolve() {
            Ok((pg, tls, _)) => {
                let hosts: Vec<String> = pg
                    .get_hosts()
                    .iter()
                    .map(|h| match h {
                        postgres::config::Host::Tcp(name) => name.clone(),
                        #[cfg(unix)]
                        postgres::config::Host::Unix(path) => path.display().to_string(),
                    })
                    .collect();
                let port = pg.get_ports().first().copied().unwrap_or(5432);
                format!(
                    "{}:{port}/{} schema {} (TLS {tls:?})",
                    hosts.join(","),
                    pg.get_dbname().unwrap_or("?"),
                    self.schema
                )
            }
            Err(e) => format!("an unreadable database URL ({e})"),
        }
    }
}

/// A schema name the services use unquoted: lower case letters, digits and underscores, starting with a letter.
fn valid_schema(schema: &str) -> Result<(), String> {
    let ok = (1..=63).contains(&schema.len())
        && schema.starts_with(|c: char| c.is_ascii_lowercase())
        && schema.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(format!("a database schema must be lower-case letters, digits and underscores, not {schema:?}"))
    }
}

fn connector(tls: Tls, ca_file: Option<&str>) -> Result<MakeTlsConnector, String> {
    let mut builder = SslConnector::builder(SslMethod::tls_client()).map_err(|e| format!("TLS: {e}"))?;
    match tls {
        Tls::Disable | Tls::Prefer | Tls::Require => builder.set_verify(SslVerifyMode::NONE),
        Tls::VerifyCa | Tls::VerifyFull => {
            builder.set_verify(SslVerifyMode::PEER);
            if let Some(file) = ca_file {
                builder.set_ca_file(file).map_err(|e| format!("TLS: the CA file {file}: {e}"))?;
            }
        }
    }
    let mut make = MakeTlsConnector::new(builder.build());
    let check_host = tls == Tls::VerifyFull;
    make.set_callback(move |connect, _| {
        connect.set_verify_hostname(check_host);
        Ok(())
    });
    Ok(make)
}

struct Idle {
    clients: Vec<Client>,
    /// Connections open, idle or checked out.
    open: usize,
}

struct Shared {
    config: Config,
    pg: postgres::Config,
    tls: MakeTlsConnector,
    idle: Mutex<Idle>,
    freed: Condvar,
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A pool of connections to one service's schema. Cloning shares the pool.
#[derive(Clone)]
pub struct Database {
    shared: Arc<Shared>,
}

/// A checked-out connection, returned to the pool when dropped unless it broke.
pub struct Conn {
    client: Option<Client>,
    shared: Arc<Shared>,
}

impl Deref for Conn {
    type Target = Client;

    fn deref(&self) -> &Client {
        self.client.as_ref().unwrap_or_else(|| unreachable!("a connection is only taken when it is dropped"))
    }
}

impl DerefMut for Conn {
    fn deref_mut(&mut self) -> &mut Client {
        self.client.as_mut().unwrap_or_else(|| unreachable!("a connection is only taken when it is dropped"))
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        let Some(client) = self.client.take() else { return };
        let mut idle = locked(&self.shared.idle);
        if client.is_closed() {
            idle.open = idle.open.saturating_sub(1);
        } else {
            idle.clients.push(client);
        }
        drop(idle);
        self.shared.freed.notify_one();
    }
}

/// One migration of a service's schema, as Atlas keeps them (`core/store/README.md`): the version its file name
/// begins with, its name, and the SQL, embedded so that a build knows the schema it was written against. Applying
/// them is Atlas's (`Database::migrate`); a migration is never edited once released, only followed by another.
#[derive(Clone, Copy, Debug)]
pub struct Migration {
    pub version: &'static str,
    pub name: &'static str,
    pub sql: &'static str,
}

impl Database {
    /// A pool for `config`, with one connection made at once so that a wrong URL, password or TLS setting is an error
    /// now rather than on the first request.
    pub fn connect(config: &Config) -> Result<Self, Error> {
        valid_schema(&config.schema).map_err(Error::Refused)?;
        let (pg, tls, ca) = config.resolve().map_err(Error::Refused)?;
        let tls = connector(tls, ca.as_deref()).map_err(Error::Refused)?;
        let shared = Arc::new(Shared {
            config: config.clone(),
            pg,
            tls,
            idle: Mutex::new(Idle { clients: Vec::new(), open: 0 }),
            freed: Condvar::new(),
        });
        let db = Self { shared };
        drop(db.get()?);
        Ok(db)
    }

    /// `connect`, tried until it works, waiting longer after each failure (1 s doubling to 30 s) and saying on stderr
    /// why, prefixed with `who`. A refusal that waiting cannot mend (a malformed URL, a bad CA file) is returned.
    pub fn connect_retrying(config: &Config, who: &str) -> Result<Self, String> {
        let mut wait = Duration::from_secs(1);
        let mut attempt = 1u32;
        loop {
            match Self::connect(config) {
                Ok(db) => {
                    if attempt > 1 {
                        eprintln!("{who}: the database {} is reachable again", config.describe());
                    }
                    return Ok(db);
                }
                Err(Error::Refused(e)) if e.starts_with("the database URL") || e.starts_with("TLS") => {
                    return Err(e);
                }
                Err(e) => {
                    eprintln!(
                        "{who}: cannot use the database {} (attempt {attempt}): {e}; trying again in {} s",
                        config.describe(),
                        wait.as_secs()
                    );
                    thread::sleep(wait);
                    wait = (wait * 2).min(Duration::from_secs(30));
                    attempt = attempt.saturating_add(1);
                }
            }
        }
    }

    pub fn config(&self) -> &Config {
        &self.shared.config
    }

    pub fn schema(&self) -> &str {
        &self.shared.config.schema
    }

    fn open(&self) -> Result<Client, Error> {
        let shared = &self.shared;
        let mut client = shared.pg.connect(shared.tls.clone())?;
        // The schema is checked by `valid_schema`, so it needs no quoting. `public` stays on the path only for
        // extensions a managed database installs there; the service's own tables are always found first.
        let setup = format!(
            "SET search_path TO {}, public; SET statement_timeout = {STATEMENT_TIMEOUT_MS}; SET TIME ZONE 'UTC'",
            shared.config.schema
        );
        client.batch_execute(&setup)?;
        Ok(client)
    }

    /// A connection, waiting up to five seconds for one to be free.
    pub fn get(&self) -> Result<Conn, Error> {
        let shared = &self.shared;
        let size = shared.config.pool_size.max(1);
        let idle = locked(&shared.idle);
        let (mut idle, _) = shared
            .freed
            .wait_timeout_while(idle, WAIT, |i| i.clients.is_empty() && i.open >= size)
            .unwrap_or_else(PoisonError::into_inner);
        while let Some(client) = idle.clients.pop() {
            if client.is_closed() {
                idle.open = idle.open.saturating_sub(1);
                continue;
            }
            return Ok(Conn { client: Some(client), shared: Arc::clone(shared) });
        }
        if idle.open >= size {
            return Err(Error::Unavailable(format!("all {size} connections are in use")));
        }
        idle.open += 1;
        drop(idle);
        match self.open() {
            Ok(client) => Ok(Conn { client: Some(client), shared: Arc::clone(shared) }),
            Err(e) => {
                locked(&shared.idle).open -= 1;
                shared.freed.notify_one();
                Err(e)
            }
        }
    }

    /// Runs `work` in a transaction, and commits it if `work` returns `Ok`; otherwise it is rolled back.
    pub fn transaction<T>(&self, work: impl FnOnce(&mut Transaction<'_>) -> Result<T, Error>) -> Result<T, Error> {
        let mut conn = self.get()?;
        let mut tx = conn.transaction()?;
        let value = work(&mut tx)?;
        tx.commit()?;
        Ok(value)
    }

    /// Whether the database answers now.
    pub fn ping(&self) -> Result<(), Error> {
        self.get()?.simple_query("SELECT 1")?;
        Ok(())
    }

    /// Brings the schema up to the last of `migrations`, and returns the versions it applied. Idempotent, and safe to
    /// run from several processes at once: they queue on an advisory lock and later ones find nothing to do. This is
    /// what a service's `migrate` subcommand runs; a service itself only checks (`wait_for_schema`), unless told to
    /// migrate at start.
    /// Applies the migrations in `dir` with Atlas, which keeps the history in `atlas_schema_revisions` and takes its
    /// own lock, and returns the versions it applied. The schema is created first if it is missing, since Atlas writes
    /// into one that exists. A deployment does not come through here: its migrate job runs the same Atlas against the
    /// same directory (deploy/README.md). This is for tests and for `--migrate-on-start` on a laptop.
    pub fn migrate(&self, dir: &Path) -> Result<Vec<String>, Error> {
        let schema = self.schema().to_string();
        self.transaction(|tx| {
            let exists = tx.query_opt("SELECT 1 FROM pg_namespace WHERE nspname = $1", &[&schema])?.is_some();
            if !exists {
                tx.batch_execute(&format!("CREATE SCHEMA {schema}"))?;
            }
            Ok(())
        })?;
        let before = self.applied_versions()?;
        let atlas = atlas_binary()?;
        let outcome = Command::new(&atlas)
            .arg("migrate")
            .arg("apply")
            .arg("--dir")
            .arg(format!("file://{}", dir.display()))
            .arg("--url")
            .arg(self.config().atlas_url())
            .arg("--revisions-schema")
            .arg(&schema)
            .output()
            .map_err(|e| Error::refused(format!("running {}: {e}", atlas.display())))?;
        if !outcome.status.success() {
            let said = String::from_utf8_lossy(&outcome.stderr);
            let said = if said.trim().is_empty() { String::from_utf8_lossy(&outcome.stdout) } else { said };
            let said = said.trim();
            // The one failure an operator cannot act on from what Atlas says: a schema built before Atlas was
            // adopted holds tables it has no history for, and it will not touch it. Saying so plainly, with the
            // remedy, is the difference between a minute and an afternoon (deploy/README.md, migrations).
            if said.contains("not clean") {
                return Err(Error::refused(format!(
                    "schema {schema} has tables from before Atlas kept this history, so it will not apply {}.                      Tell it which migration that schema already has — the last one applied to it by hand or by an                      earlier version — and it will record that and carry on from the next:                      `atlas migrate apply --dir file://{} --url <the service's url> --revisions-schema {schema}                      --baseline <version>`, or migrations.baseline.<service> in the chart. Atlas said: {said}",
                    dir.display(),
                    dir.display()
                )));
            }
            return Err(Error::refused(format!("atlas could not apply {} to schema {schema}: {said}", dir.display())));
        }
        let after = self.applied_versions()?;
        Ok(after.into_iter().filter(|v| !before.contains(v)).collect())
    }

    /// The versions Atlas records as applied, in order. A version whose run failed or was left part-way is not one of
    /// them: Atlas keeps such a row with its error, and applying again is what mends it.
    fn applied_versions(&self) -> Result<Vec<String>, Error> {
        let schema = self.schema().to_string();
        self.transaction(|tx| {
            let table = format!("{schema}.atlas_schema_revisions");
            if !tx.query_one("SELECT to_regclass($1) IS NOT NULL", &[&table])?.get::<_, bool>(0) {
                return Ok(Vec::new());
            }
            let rows = tx.query(
                &format!(
                    "SELECT version FROM {table} WHERE applied = total AND coalesce(error, '') = '' ORDER BY version"
                ),
                &[],
            )?;
            Ok(rows.iter().map(|row| row.get::<_, String>(0)).collect())
        })
    }

    /// The SQL each applied version was built from, kept beside Atlas's history. Atlas hashes a migration directory,
    /// but hashing it again after editing a file it has already applied leaves nothing to notice: `apply`, `status`
    /// and `validate` all pass. A released migration must never be edited (decisions.md, S-2), so the first build to
    /// see a version applied records what it held, and any later build that holds something else stops here.
    ///
    /// Nothing applied means nothing to record, and the table is deliberately *not* made then. Asking a schema how
    /// current it is must not change it: `wait_for_schema` asks in a loop, which is how a service waits for the
    /// migration job on a first install — where that job cannot be a Helm hook, because PostgreSQL does not exist
    /// yet for a hook to reach (deploy/charts/venue-mini/templates/migrate.yaml), and under ArgoCD is never one at
    /// all, because that test uses `lookup` and `lookup` is empty under `helm template`. Creating this table from
    /// inside that loop left Atlas a schema holding a table it had not put there, which it refuses as unclean
    /// unless given a baseline; and because the loop kept running, the schema went back to being unclean as fast as
    /// anyone cleaned it. The service was waiting for a migration that its own waiting prevented.
    fn checked_against_this_build(&self, migrations: &[Migration], applied: &[String]) -> Result<(), Error> {
        if applied.is_empty() {
            return Ok(());
        }
        let schema = self.schema().to_string();
        self.transaction(|tx| {
            // Named with its schema, not left to `search_path`. Unqualified, this landed wherever the connection
            // happened to point: inside the service's own schema in a deployment, where the role and the schema
            // share a name, and in `public` under the test harness, where they do not — so the guard was shared by
            // every test schema at once and the tests exercised a different arrangement from the one that runs.
            tx.batch_execute(&format!(
                "CREATE TABLE IF NOT EXISTS {schema}.venue_migration_sql (
                   version text PRIMARY KEY,
                   sha256 text NOT NULL,
                   first_seen timestamptz NOT NULL DEFAULT now()
                 )"
            ))?;
            for version in applied {
                let Some(m) = migrations.iter().find(|m| m.version == *version) else { continue };
                let sum = checksum(m.sql);
                let row = tx.query_opt(
                    &format!("SELECT sha256 FROM {schema}.venue_migration_sql WHERE version = $1"),
                    &[version],
                )?;
                match row {
                    Some(row) if row.get::<_, String>(0) == sum => {}
                    Some(_) => {
                        return Err(Error::refused(format!(
                            "migration {version} ({}) of schema {schema} differs from the one applied: a released \
                             migration must never be edited, and a correction is a migration of its own",
                            m.name
                        )))
                    }
                    None => {
                        tx.execute(
                            &format!("INSERT INTO {schema}.venue_migration_sql (version, sha256) VALUES ($1, $2)"),
                            &[version, &sum],
                        )?;
                    }
                }
            }
            Ok(())
        })
    }

    /// How many of `migrations` the schema has had applied: all of them is the schema this build expects. An error if
    /// the schema holds a version this build does not know, or holds them in another order: waiting cannot mend
    /// either, and a build rolled back onto a newer schema must not write rows the newer one defined.
    pub fn schema_version(&self, migrations: &[Migration]) -> Result<usize, Error> {
        in_order(migrations)?;
        let applied = self.applied_versions()?;
        self.checked_against_this_build(migrations, &applied)?;
        for (i, version) in applied.iter().enumerate() {
            match migrations.get(i) {
                Some(m) if m.version == version => {}
                Some(m) => {
                    return Err(Error::refused(format!(
                        "schema {} has migration {version} where this build has {} ({}): the directory they were \
                         applied from is not this build's",
                        self.schema(),
                        m.version,
                        m.name
                    )))
                }
                None => {
                    return Err(Error::refused(format!(
                        "schema {} is at migration {version}, newer than this build knows (it ends at {}): run a \
                         build that has it",
                        self.schema(),
                        migrations.last().map_or("none", |m| m.version)
                    )))
                }
            }
        }
        Ok(applied.len())
    }

    /// Waits until the schema is at the last of `migrations`, as a service does at start: while it is behind, or the
    /// database cannot be reached, it says why on stderr (prefixed with `who`) and tries again with backoff. A schema
    /// ahead of this build, or with an edited migration, is an error at once: waiting cannot mend it.
    pub fn wait_for_schema(&self, migrations: &[Migration], who: &str) -> Result<(), String> {
        let mut wait = Duration::from_secs(1);
        loop {
            match self.schema_version(migrations) {
                Ok(at) if at == migrations.len() => return Ok(()),
                Ok(at) => eprintln!(
                    "{who}: schema {} is at migration {at} and this build needs {}: waiting for `{who} migrate` (the \
                     chart's migrate job) to apply the rest; trying again in {} s",
                    self.schema(),
                    migrations.len(),
                    wait.as_secs()
                ),
                Err(Error::Unavailable(e)) => {
                    eprintln!("{who}: cannot read schema {}: {e}; trying again in {} s", self.schema(), wait.as_secs());
                }
                Err(Error::Refused(e)) => return Err(e),
            }
            thread::sleep(wait);
            wait = (wait * 2).min(Duration::from_secs(30));
        }
    }
}

fn checksum(sql: &str) -> String {
    Sha256::digest(sql.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Versions in ascending order, none repeated: the order Atlas applies its files in.
fn in_order(migrations: &[Migration]) -> Result<(), Error> {
    for pair in migrations.windows(2) {
        if pair[0].version >= pair[1].version {
            return Err(Error::refused(format!(
                "migration {} ({}) does not come before {} ({})",
                pair[0].version, pair[0].name, pair[1].version, pair[1].name
            )));
        }
    }
    Ok(())
}

/// Checks that `dir` holds exactly `migrations` as files named `V<version, three digits>__<name>.sql` with the same
/// SQL, so the files a DevOps tool would run and the ones the binary embeds are the same. For each service's tests.
pub fn check_files(dir: &std::path::Path, migrations: &[Migration]) -> Result<(), String> {
    in_order(migrations).map_err(String::from)?;
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    names.sort();
    let expected: Vec<String> = migrations.iter().map(|m| format!("{}_{}.sql", m.version, m.name)).collect();
    let sum = "atlas.sum".to_string();
    let files: Vec<String> = names.iter().filter(|n| **n != sum).cloned().collect();
    if files != expected {
        return Err(format!("{} holds {files:?}, and the build embeds {expected:?}", dir.display()));
    }
    if !names.contains(&sum) {
        return Err(format!("{} has no atlas.sum: run `atlas migrate hash`", dir.display()));
    }
    let listed = std::fs::read_to_string(dir.join(&sum)).map_err(|e| format!("{sum}: {e}"))?;
    for name in &expected {
        if !listed.contains(name.as_str()) {
            return Err(format!("{sum} does not list {name}: run `atlas migrate hash`"));
        }
    }
    for (m, name) in migrations.iter().zip(&expected) {
        let text = std::fs::read_to_string(dir.join(name)).map_err(|e| format!("{name}: {e}"))?;
        if text != m.sql {
            return Err(format!("{name} differs from the migration the build embeds"));
        }
    }
    Ok(())
}

/// Where this build's migrations are: `VENUE_MIGRATIONS` when something says so — an image carries them apart from
/// the source — and otherwise the `migrations` directory beside the crate's `Cargo.toml`, which is where they live in
/// a checkout. This is for tests and `--migrate-on-start`; a deployment's migrate job names the directory itself.
#[macro_export]
macro_rules! migrations_dir {
    () => {
        std::env::var("VENUE_MIGRATIONS")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/migrations")))
    };
}

/// The Atlas binary: `VENUE_ATLAS`, or `atlas` on the PATH. It applies every migration this venue runs, in tests and
/// in a deployment alike (core/store/decisions.md, S-2).
pub fn atlas_binary() -> Result<PathBuf, Error> {
    if let Ok(named) = std::env::var("VENUE_ATLAS") {
        let path = PathBuf::from(&named);
        if path.is_file() {
            return Ok(path);
        }
        return Err(Error::refused(format!("VENUE_ATLAS is {named}, which is not a file")));
    }
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        let candidate = Path::new(dir).join("atlas");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(Error::refused(
        "no atlas binary: put one on the PATH or name it in VENUE_ATLAS (deploy/README.md, migrations)".to_string(),
    ))
}

/// Converts between the services' unsigned numbers and PostgreSQL's signed `bigint`, refusing what does not fit.
pub fn big(n: u64) -> Result<i64, Error> {
    i64::try_from(n).map_err(|_| Error::refused(format!("{n} does not fit a bigint")))
}

/// A `bigint` column as the services' unsigned numbers; a negative value is a record something else wrote.
pub fn unsigned(n: i64) -> Result<u64, Error> {
    u64::try_from(n).map_err(|_| Error::refused(format!("a negative value {n} where a count or time belongs")))
}

/// The latest time a `timestamptz` column is given: the end of the year 9999, well inside PostgreSQL's range.
const LATEST_MS: u64 = 253_402_300_799_999;

/// Wall-clock milliseconds since the Unix epoch, the services' own clock readings, as a `timestamptz` value.
pub fn time(ms: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms.min(LATEST_MS))
}

/// A `timestamptz` column as milliseconds since the Unix epoch; a time before the epoch reads as 0.
pub fn millis(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Databases for tests: each test gets a schema of its own on the server `VENUE_TEST_DATABASE_URL` names, so tests
/// run in parallel against one throwaway server (`./check` starts one in a container when Docker is usable).
pub mod testing {
    use super::{Config, Database, Migration};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// The URL tests use, if one is set.
    pub fn url() -> Option<String> {
        std::env::var("VENUE_TEST_DATABASE_URL").ok().filter(|u| !u.is_empty())
    }

    /// A configuration for a fresh schema named after `name`, or `None` with a line saying the test is skipped.
    pub fn config(name: &str) -> Option<Config> {
        let Some(url) = url() else {
            eprintln!("skipping {name}: VENUE_TEST_DATABASE_URL is not set (./check sets it when Docker is usable)");
            return None;
        };
        let tag: String = name
            .bytes()
            .map(|b| if b.is_ascii_alphanumeric() { b.to_ascii_lowercase() as char } else { '_' })
            .take(30)
            .collect();
        let schema = format!("t_{tag}_{}_{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        Some(Config { url, schema, pool_size: 4, tls: None, ca_file: None, application: format!("test {name}") })
    }

    /// A configuration for a fresh schema owned by a role of its own that is not a superuser, as a deployed service's
    /// is, so that privileges and triggers are tested the way they apply there. `None` if skipped.
    pub fn owned_config(name: &str) -> Option<Config> {
        let config = config(name)?;
        let admin = Database::connect(&Config { schema: "public".into(), ..config.clone() })
            .unwrap_or_else(|e| panic!("the test database: {e}"));
        let role = config.schema.clone();
        let mut bytes = [0u8; 12];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
            .unwrap_or_else(|e| panic!("a password for {role}: {e}"));
        let password: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        admin
            .get()
            .and_then(|mut c| {
                c.batch_execute(&format!(
                    "CREATE ROLE {role} LOGIN PASSWORD '{password}'; CREATE SCHEMA {role} AUTHORIZATION {role}"
                ))?;
                Ok(())
            })
            .unwrap_or_else(|e| panic!("a role for {role}: {e}"));
        let (scheme, rest) = config.url.split_once("://").unwrap_or_else(|| panic!("a URL with a scheme"));
        let host = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
        Some(Config { url: format!("{scheme}://{role}:{password}@{host}"), ..config })
    }

    /// A pool on a fresh schema with the migrations in `dir` applied, or `None` (skipped, and said so). Applying is
    /// Atlas's, exactly as it is in a deployment, so a test runs the migrations the venue runs.
    pub fn database(name: &str, dir: &std::path::Path) -> Option<Database> {
        let config = config(name)?;
        let db = Database::connect(&config).unwrap_or_else(|e| panic!("the test database: {e}"));
        db.migrate(dir).unwrap_or_else(|e| panic!("migrating {}: {e}", config.schema));
        Some(db)
    }

    /// A directory holding `migrations` as files, hashed as Atlas keeps them, for tests of the runner itself. Left
    /// behind on failure, so a broken run can be looked at; the temporary directory is the test's own.
    pub fn written(name: &str, migrations: &[Migration]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("venue-migrations-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        for m in migrations {
            let file = dir.join(format!("{}_{}.sql", m.version, m.name));
            std::fs::write(&file, m.sql).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        }
        let atlas = super::atlas_binary().unwrap_or_else(|e| panic!("{e}"));
        let outcome = std::process::Command::new(atlas)
            .arg("migrate")
            .arg("hash")
            .arg("--dir")
            .arg(format!("file://{}", dir.display()))
            .output()
            .unwrap_or_else(|e| panic!("atlas migrate hash: {e}"));
        assert!(outcome.status.success(), "atlas migrate hash: {}", String::from_utf8_lossy(&outcome.stderr));
        dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIGRATIONS: [Migration; 2] = [
        Migration {
            version: "20260101000001",
            name: "notes",
            sql: "CREATE TABLE notes (id bigint PRIMARY KEY, text text NOT NULL)",
        },
        Migration {
            version: "20260101000002",
            name: "notes_at",
            sql: "ALTER TABLE notes ADD COLUMN at bigint NOT NULL DEFAULT 0",
        },
    ];

    #[test]
    fn a_url_is_split_into_the_clients_settings_and_libpqs_tls_parameters() {
        let config = Config {
            url: "postgres://svc:secret@db.internal:6543/ledger?sslmode=verify-full&sslrootcert=/ca.pem&connect_timeout=3"
                .into(),
            schema: "treasury".into(),
            pool_size: 2,
            tls: None,
            ca_file: None,
            application: "test".into(),
        };
        let (pg, tls, ca) = config.resolve().expect("a URL");
        assert_eq!((tls, ca.as_deref()), (Tls::VerifyFull, Some("/ca.pem")));
        assert_eq!(pg.get_dbname(), Some("ledger"));
        assert_eq!(pg.get_connect_timeout(), Some(&Duration::from_secs(3)));
        let shown = config.describe();
        assert!(shown.contains("db.internal:6543/ledger") && !shown.contains("secret"), "{shown}");
        let overridden = Config { tls: Some(Tls::Disable), ca_file: Some("/other.pem".into()), ..config };
        let (_, tls, ca) = overridden.resolve().expect("a URL");
        assert_eq!((tls, ca.as_deref()), (Tls::Disable, Some("/other.pem")));
    }

    #[test]
    fn settings_need_a_url_and_a_plain_schema_name() {
        let settings = |pairs: &'static [(&'static str, &'static str)]| {
            move |key: &str| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| v.to_string())
        };
        assert!(Config::from_settings(settings(&[]), "t", "treasury").expect("fine").is_none());
        let config = Config::from_settings(settings(&[("database-url", "postgres://h/d")]), "t", "treasury")
            .expect("fine")
            .expect("a config");
        assert_eq!((config.schema.as_str(), config.pool_size, config.tls), ("treasury", 4, None));
        for bad in [
            &[("database-url", "postgres://h/d"), ("database-schema", "Treasury; DROP")][..],
            &[("database-url", "postgres://h/d"), ("database-pool-size", "0")],
            &[("database-url", "postgres://h/d"), ("database-tls", "sometimes")],
        ] {
            assert!(Config::from_settings(settings(bad), "t", "treasury").is_err(), "{bad:?}");
        }
    }

    #[test]
    fn migrations_apply_once_and_a_schema_newer_than_the_build_stops_it() {
        let Some(config) = testing::config("store_migrations") else { return };
        let db = Database::connect(&config).expect("a database");
        let first = testing::written("store_first", &MIGRATIONS[..1]);
        let both = testing::written("store_both", &MIGRATIONS);
        assert_eq!(db.schema_version(&MIGRATIONS).expect("readable"), 0, "nothing applied yet");
        assert_eq!(db.migrate(&first).expect("first"), vec![MIGRATIONS[0].version.to_string()]);
        assert_eq!(db.schema_version(&MIGRATIONS).expect("readable"), 1, "behind");
        assert_eq!(db.migrate(&both).expect("second"), vec![MIGRATIONS[1].version.to_string()]);
        db.wait_for_schema(&MIGRATIONS, "test").expect("current");
        assert_eq!(db.migrate(&both).expect("again"), Vec::<String>::new(), "applying again changes nothing");

        // A build that does not know the schema's latest migration must not write rows the newer one defined.
        assert!(
            matches!(db.schema_version(&MIGRATIONS[..1]), Err(Error::Refused(e)) if e.contains("newer than this build")),
            "an older build accepted a newer schema"
        );
        assert!(db.wait_for_schema(&MIGRATIONS[..1], "test").is_err(), "an older build waited instead of stopping");

        // A migration edited after it was applied. Atlas notices nothing once its directory has been hashed again,
        // so what refuses it is the build's own record of what each applied version held.
        let edited = [MIGRATIONS[0], Migration { sql: "ALTER TABLE notes ADD COLUMN at2 bigint", ..MIGRATIONS[1] }];
        assert!(
            matches!(db.schema_version(&edited), Err(Error::Refused(e)) if e.contains("never be edited")),
            "an edited migration passed for the one that was applied"
        );
        assert!(db.wait_for_schema(&edited, "test").is_err(), "a build with an edited migration started");
    }

    #[test]
    fn asking_an_unmigrated_schema_how_current_it_is_leaves_it_alone() {
        let Some(config) = testing::config("store_waiting") else { return };
        let db = Database::connect(&config).expect("a database");
        let dir = testing::written("store_waiting", &MIGRATIONS);

        // What a service does while it waits for the migration job, over and over: on a first install that job is
        // not a hook, so the two run side by side (deploy/charts/venue-mini/templates/migrate.yaml).
        for _ in 0..3 {
            assert_eq!(db.schema_version(&MIGRATIONS).expect("readable"), 0, "nothing applied yet");
        }

        // Asking must not have put anything in the schema. Atlas refuses a schema holding tables it did not write
        // unless it is given a baseline, and a service that dirties the schema it is waiting for waits for ever.
        // This schema, which is this test's alone. The guard table is named with its schema now, so it lands here
        // or nowhere — where before it went to `public` and was shared by every test schema at once, which is why
        // the suite never noticed a service dirtying the schema it was waiting for.
        let left: Vec<String> = db
            .transaction(|tx| {
                Ok(tx
                    .query(
                        "SELECT table_name FROM information_schema.tables WHERE table_schema = $1",
                        &[&config.schema],
                    )?
                    .iter()
                    .map(|r| r.get::<_, String>(0))
                    .collect())
            })
            .expect("looked");
        assert!(left.is_empty(), "asking left a table behind, which Atlas refuses as unclean: {left:?}");

        // And Atlas can still do its first apply.
        assert_eq!(db.migrate(&dir).expect("atlas applies").len(), MIGRATIONS.len());
        db.wait_for_schema(&MIGRATIONS, "test").expect("current once applied");
    }

    #[test]
    fn migrations_must_be_in_order_and_match_their_files() {
        let out_of_order = [MIGRATIONS[1], MIGRATIONS[0]];
        assert!(in_order(&out_of_order).is_err(), "versions that go backwards");
        let dir = testing::written("store_files", &MIGRATIONS);
        check_files(&dir, &MIGRATIONS).expect("the files and the build agree");
        std::fs::write(dir.join("20260101000002_notes_at.sql"), "SELECT 1").expect("written");
        assert!(check_files(&dir, &MIGRATIONS).is_err(), "a file that differs from the build");
    }

    #[test]
    fn a_transaction_commits_whole_or_not_at_all_and_the_pool_recovers_from_broken_connections() {
        let dir = testing::written("store_transactions", &MIGRATIONS);
        let Some(db) = testing::database("store_transactions", &dir) else { return };
        db.transaction(|tx| {
            tx.execute("INSERT INTO notes (id, text) VALUES (1, 'kept')", &[])?;
            Ok(())
        })
        .expect("committed");
        let failed: Result<(), Error> = db.transaction(|tx| {
            tx.execute("INSERT INTO notes (id, text) VALUES (2, 'rolled back')", &[])?;
            Err(Error::refused("the service changed its mind"))
        });
        assert!(failed.is_err());
        let duplicate = db.transaction(|tx| {
            tx.execute("INSERT INTO notes (id, text) VALUES (3, 'also rolled back')", &[])?;
            tx.execute("INSERT INTO notes (id, text) VALUES (1, 'twice')", &[])?;
            Ok(())
        });
        assert!(matches!(duplicate, Err(Error::Refused(e)) if e.contains("23505")), "a unique violation is a refusal");
        let count: i64 =
            db.get().expect("a connection").query_one("SELECT count(*) FROM notes", &[]).expect("a row").get(0);
        assert_eq!(count, 1);

        // Kill every connection of this pool from outside: the next checkout makes a new one.
        let mut conn = db.get().expect("a connection");
        let pid: i32 = conn.query_one("SELECT pg_backend_pid()", &[]).expect("a pid").get(0);
        drop(conn);
        let empty = testing::written("store_empty", &[]);
        let Some(admin) = testing::database("store_transactions_admin", &empty) else { return };
        admin.get().expect("a connection").execute("SELECT pg_terminate_backend($1)", &[&pid]).expect("terminated");
        let mut recovered = false;
        for _ in 0..3 {
            if db.ping().is_ok() {
                recovered = true;
                break;
            }
        }
        assert!(recovered, "the pool never replaced a broken connection");
    }

    #[test]
    fn an_unreachable_database_is_unavailable_not_refused() {
        let config = Config {
            url: "postgres://nobody@127.0.0.1:1/none?connect_timeout=1".into(),
            schema: "nothing".into(),
            pool_size: 1,
            tls: Some(Tls::Disable),
            ca_file: None,
            application: "test".into(),
        };
        assert!(matches!(Database::connect(&config), Err(Error::Unavailable(_))));
    }
}

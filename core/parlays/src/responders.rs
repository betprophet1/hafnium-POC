//! Who may answer requests for quote, and who is listening: the operator's list of approved responder accounts, kept
//! in a file, and the responders' subscriptions, each a gateway's session with its filters. Nothing here talks to a
//! gateway or a node, so all of it is tested on its own.
//!
//! Approval is per account. A credential that may respond (an API key's permission, when keys arrive) is a question
//! for the gateway, which authenticates; whether the account behind it is approved is this service's.

use crate::rules::{MAX_LEGS, MIN_LEGS};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use venue_link::view::Leg;
use venue_link::wire::Error;

/// The most IDs a filter may list of events, or of markets.
pub const MAX_FILTER_IDS: usize = 100;

/// Which requests a subscription is sent. Every part given must hold: a request's leg count within the bounds, at
/// least one leg on a listed event if events are listed, at least one leg on a listed market if markets are listed,
/// and at least the minimum size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filter {
    pub min_legs: usize,
    pub max_legs: usize,
    pub events: BTreeSet<u64>,
    pub instruments: BTreeSet<u64>,
    pub min_quantity: u64,
}

impl Default for Filter {
    fn default() -> Self {
        Self {
            min_legs: MIN_LEGS,
            max_legs: MAX_LEGS,
            events: BTreeSet::new(),
            instruments: BTreeSet::new(),
            min_quantity: 0,
        }
    }
}

fn ids(value: Option<&Value>, name: &str) -> Result<BTreeSet<u64>, String> {
    let Some(value) = value else { return Ok(BTreeSet::new()) };
    let list = value
        .as_array()
        .filter(|l| l.len() <= MAX_FILTER_IDS)
        .ok_or_else(|| format!("{name} must be a list of at most {MAX_FILTER_IDS} IDs"))?;
    list.iter().map(|v| v.as_u64().ok_or_else(|| format!("{name} must be a list of IDs"))).collect()
}

impl Filter {
    /// A filter as the gateway sends it: `{"minLegs", "maxLegs", "eventIds", "instrumentIds", "minQuantity"}`, every
    /// field optional and IDs as numbers. Null is no filter.
    pub fn parse(value: &Value) -> Result<Self, String> {
        if value.is_null() {
            return Ok(Self::default());
        }
        let fields = value.as_object().ok_or("filters must be an object")?;
        if let Some(unknown) = fields
            .keys()
            .find(|k| !["minLegs", "maxLegs", "eventIds", "instrumentIds", "minQuantity"].contains(&k.as_str()))
        {
            return Err(format!("unknown filter {unknown}"));
        }
        let legs = |name: &str, default: usize| match fields.get(name) {
            None => Ok(default),
            Some(v) => v
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .filter(|n| (MIN_LEGS..=MAX_LEGS).contains(n))
                .ok_or_else(|| format!("{name} must be from {MIN_LEGS} to {MAX_LEGS}")),
        };
        let filter = Self {
            min_legs: legs("minLegs", MIN_LEGS)?,
            max_legs: legs("maxLegs", MAX_LEGS)?,
            events: ids(fields.get("eventIds"), "eventIds")?,
            instruments: ids(fields.get("instrumentIds"), "instrumentIds")?,
            min_quantity: match fields.get("minQuantity") {
                None => 0,
                Some(v) => v.as_u64().ok_or("minQuantity must be a whole number of contracts")?,
            },
        };
        if filter.min_legs > filter.max_legs {
            return Err("minLegs must not be more than maxLegs".into());
        }
        Ok(filter)
    }

    pub fn matches(&self, legs: &[Leg], quantity: u64) -> bool {
        (self.min_legs..=self.max_legs).contains(&legs.len())
            && quantity >= self.min_quantity
            && (self.events.is_empty() || legs.iter().any(|l| self.events.contains(&l.event)))
            && (self.instruments.is_empty() || legs.iter().any(|l| self.instruments.contains(&l.instrument)))
    }

    /// The filter as interface 1 shows it, with every default filled in and IDs as decimal strings.
    pub fn to_json(&self) -> Value {
        let strings = |set: &BTreeSet<u64>| set.iter().map(u64::to_string).collect::<Vec<_>>();
        json!({
            "minLegs": self.min_legs, "maxLegs": self.max_legs, "eventIds": strings(&self.events),
            "instrumentIds": strings(&self.instruments), "minQuantity": self.min_quantity
        })
    }
}

/// The approved responder accounts, and the file they are kept in. The file is the whole truth: the service reads it
/// when it starts, and writes it whole, through a rename, on every change, so an operator can also edit it by hand
/// and restart the service. When there is no file yet, the list starts from the catalogue's `"responder": true`
/// accounts and is written at once.
pub struct Approvals {
    path: Option<PathBuf>,
    accounts: BTreeSet<u64>,
}

impl Approvals {
    /// Reads the list at `path`, or starts it from `seed` when there is no file there. Without a path the list lives
    /// in memory only, for tests.
    pub fn load(path: Option<&Path>, seed: impl IntoIterator<Item = u64>) -> Result<Self, Error> {
        let Some(path) = path else { return Ok(Self { path: None, accounts: seed.into_iter().collect() }) };
        if path.exists() {
            let text = fs::read_to_string(path)?;
            let value: Value =
                serde_json::from_str(&text).map_err(|e| format!("{}: not a responder list: {e}", path.display()))?;
            let accounts = value["responders"]
                .as_array()
                .ok_or_else(|| format!("{}: no responders list", path.display()))?
                .iter()
                .map(|v| v.as_u64().ok_or_else(|| format!("{}: a responder is not an account ID", path.display())))
                .collect::<Result<BTreeSet<u64>, String>>()?;
            return Ok(Self { path: Some(path.to_path_buf()), accounts });
        }
        let approvals = Self { path: Some(path.to_path_buf()), accounts: seed.into_iter().collect() };
        approvals.save()?;
        Ok(approvals)
    }

    fn save(&self) -> Result<(), Error> {
        let Some(path) = &self.path else { return Ok(()) };
        let text = format!("{}\n", json!({"responders": self.accounts}));
        let mut temporary = path.as_os_str().to_owned();
        temporary.push(".tmp");
        fs::write(&temporary, text)?;
        fs::rename(&temporary, path)?;
        Ok(())
    }

    pub fn contains(&self, account: u64) -> bool {
        self.accounts.contains(&account)
    }

    pub fn accounts(&self) -> &BTreeSet<u64> {
        &self.accounts
    }

    /// Approves an account, returning whether it was not approved already. Nothing changes if the file cannot be
    /// written.
    pub fn approve(&mut self, account: u64) -> Result<bool, Error> {
        if !self.accounts.insert(account) {
            return Ok(false);
        }
        self.save().inspect_err(|_| {
            self.accounts.remove(&account);
        })?;
        Ok(true)
    }

    /// Revokes an account's approval, returning whether it had one. Nothing changes if the file cannot be written.
    pub fn revoke(&mut self, account: u64) -> Result<bool, Error> {
        if !self.accounts.remove(&account) {
            return Ok(false);
        }
        self.save().inspect_err(|_| {
            self.accounts.insert(account);
        })?;
        Ok(true)
    }
}

/// A subscribed session: the gateway connection it came through, and the session's key on that gateway.
pub type SessionKey = (u64, u64);

struct Subscription {
    account: u64,
    filter: Filter,
}

/// The sessions that have asked for requests, by gateway connection and session. Nothing here is kept across a
/// restart: a gateway that reconnects subscribes its sessions again.
#[derive(Default)]
pub struct Subscriptions {
    by_session: BTreeMap<SessionKey, Subscription>,
}

impl Subscriptions {
    /// Subscribes a session, replacing any filter it had.
    pub fn subscribe(&mut self, session: SessionKey, account: u64, filter: Filter) {
        self.by_session.insert(session, Subscription { account, filter });
    }

    /// Unsubscribes a session, returning whether it was subscribed.
    pub fn unsubscribe(&mut self, session: SessionKey) -> bool {
        self.by_session.remove(&session).is_some()
    }

    /// Every session of a gateway connection that has gone.
    pub fn drop_gateway(&mut self, gateway: u64) {
        self.by_session.retain(|(g, _), _| *g != gateway);
    }

    /// Ends every subscription of an account, returning the sessions that had one.
    pub fn drop_account(&mut self, account: u64) -> Vec<SessionKey> {
        let ended: Vec<SessionKey> =
            self.by_session.iter().filter(|(_, s)| s.account == account).map(|(k, _)| *k).collect();
        for key in &ended {
            self.by_session.remove(key);
        }
        ended
    }

    /// How many sessions an account has subscribed.
    pub fn count(&self, account: u64) -> usize {
        self.by_session.values().filter(|s| s.account == account).count()
    }

    /// The filter a session subscribed with and its account, if it is subscribed.
    pub fn get(&self, session: SessionKey) -> Option<(u64, &Filter)> {
        self.by_session.get(&session).map(|s| (s.account, &s.filter))
    }

    /// Where a request goes: every subscribed session of an approved account other than the requester whose filter
    /// matches, grouped by gateway connection, and how many accounts that is. There is no cap: every match is sent
    /// it.
    pub fn targets(
        &self,
        approvals: &Approvals,
        requester: u64,
        legs: &[Leg],
        quantity: u64,
    ) -> (BTreeMap<u64, Vec<u64>>, usize) {
        let mut by_gateway: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        let mut accounts = BTreeSet::new();
        for ((gateway, session), s) in &self.by_session {
            if s.account == requester || !approvals.contains(s.account) || !s.filter.matches(legs, quantity) {
                continue;
            }
            by_gateway.entry(*gateway).or_default().push(*session);
            accounts.insert(s.account);
        }
        (by_gateway, accounts.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legs(spec: &[(u64, u64)]) -> Vec<Leg> {
        spec.iter().map(|&(event, instrument)| Leg { event, instrument, yes: true }).collect()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("venue-parlays-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("a scratch directory");
        dir.join("responders.json")
    }

    #[test]
    fn a_filter_selects_by_legs_events_markets_and_size() {
        let request = legs(&[(1, 100), (2, 200), (3, 300)]);
        assert!(Filter::default().matches(&request, 10));
        let parse = |v: Value| Filter::parse(&v).expect("a valid filter");
        assert!(parse(json!({"minLegs": 3, "maxLegs": 3})).matches(&request, 10));
        assert!(!parse(json!({"maxLegs": 2})).matches(&request, 10));
        assert!(parse(json!({"eventIds": [9, 2]})).matches(&request, 10));
        assert!(!parse(json!({"eventIds": [9]})).matches(&request, 10));
        assert!(parse(json!({"instrumentIds": [300]})).matches(&request, 10));
        assert!(!parse(json!({"instrumentIds": [301]})).matches(&request, 10));
        assert!(!parse(json!({"minQuantity": 20})).matches(&request, 10));
        // Every part must hold.
        assert!(!parse(json!({"eventIds": [1], "minQuantity": 20})).matches(&request, 10));
        assert_eq!(parse(Value::Null), Filter::default());
        for bad in [
            json!({"minLegs": 1}),
            json!({"maxLegs": 7}),
            json!({"minLegs": 4, "maxLegs": 3}),
            json!({"eventIds": ["1"]}),
            json!({"eventIds": (0..101).collect::<Vec<u64>>()}),
            json!({"colour": "red"}),
            json!([]),
        ] {
            assert!(Filter::parse(&bad).is_err(), "{bad}");
        }
        assert_eq!(parse(json!({"eventIds": [5]})).to_json()["eventIds"], json!(["5"]));
    }

    #[test]
    fn approvals_start_from_the_catalogue_and_survive_a_restart() {
        let path = scratch("approvals");
        let mut approvals = Approvals::load(Some(&path), [10]).expect("seeded");
        assert!(approvals.contains(10) && path.exists(), "the seed is written at once");
        assert_eq!(approvals.approve(12).ok(), Some(true));
        assert_eq!(approvals.approve(12).ok(), Some(false));
        assert_eq!(approvals.revoke(10).ok(), Some(true));
        assert_eq!(approvals.revoke(10).ok(), Some(false));
        // A restart reads the file, not the catalogue: the revoked seed stays revoked.
        let restarted = Approvals::load(Some(&path), [10]).expect("reloaded");
        assert_eq!(restarted.accounts().iter().copied().collect::<Vec<_>>(), vec![12]);
        fs::write(&path, "not json").expect("written");
        assert!(Approvals::load(Some(&path), [10]).is_err(), "a damaged file is refused, not replaced");
    }

    #[test]
    fn a_change_that_cannot_be_written_is_not_made() {
        let path = scratch("unwritable").with_file_name("no-such-dir").join("responders.json");
        let mut approvals = Approvals { path: Some(path), accounts: BTreeSet::new() };
        assert!(approvals.approve(12).is_err());
        assert!(!approvals.contains(12));
    }

    #[test]
    fn a_request_reaches_every_matching_approved_session_but_the_requesters() {
        let approvals = Approvals::load(None, [10, 11, 2]).expect("in memory");
        let mut subs = Subscriptions::default();
        subs.subscribe((1, 100), 10, Filter::default());
        subs.subscribe((2, 200), 10, Filter::default());
        subs.subscribe((1, 101), 11, Filter { min_quantity: 50, ..Filter::default() });
        // Account 2 is approved and subscribed, and is the requester: it is never sent its own request.
        subs.subscribe((1, 102), 2, Filter::default());
        // Account 12 subscribed and was revoked since; the service ends such subscriptions, but a stray is still
        // never sent anything.
        subs.subscribe((2, 201), 12, Filter::default());
        let request = legs(&[(1, 100), (2, 200)]);
        let (targets, accounts) = subs.targets(&approvals, 2, &request, 20);
        assert_eq!(targets, BTreeMap::from([(1, vec![100]), (2, vec![200])]));
        assert_eq!(accounts, 1, "two sessions of one account are one responder");
        let (targets, accounts) = subs.targets(&approvals, 2, &request, 50);
        assert_eq!((targets.get(&1), accounts), (Some(&vec![100, 101]), 2));
        let (targets, _) = subs.targets(&approvals, 10, &request, 50);
        assert_eq!(targets, BTreeMap::from([(1, vec![101, 102])]), "the requester's own sessions are left out");
    }

    #[test]
    fn subscriptions_end_with_their_session_their_gateway_or_their_approval() {
        let mut subs = Subscriptions::default();
        subs.subscribe((1, 100), 10, Filter::default());
        subs.subscribe((1, 100), 10, Filter { max_legs: 2, ..Filter::default() });
        assert_eq!(subs.get((1, 100)).map(|(_, f)| f.max_legs), Some(2), "subscribing again replaces the filter");
        subs.subscribe((1, 101), 11, Filter::default());
        subs.subscribe((2, 100), 10, Filter::default());
        assert_eq!(subs.count(10), 2);
        assert!(subs.unsubscribe((1, 101)) && !subs.unsubscribe((1, 101)));
        subs.drop_gateway(2);
        assert_eq!(subs.count(10), 1);
        assert_eq!(subs.drop_account(10), vec![(1, 100)]);
        assert_eq!(subs.count(10), 0);
    }
}

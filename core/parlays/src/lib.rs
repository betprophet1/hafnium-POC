//! Parlays for a mini venue (blueprint §6): a parlay pays $1 if every leg resolves as given. The engine lists and
//! expires parlays but tracks no legs (spec §1: leg tracking belongs to services); this crate is that service.
//!
//! `rules` says what a parlay is and what its legs call for, as pure functions of the feed's view. `responders` keeps
//! the operator's approved responders and the sessions subscribed to requests. `service` is the process: it lists a
//! parlay on its first request for quote, routes requests to subscribed approved responders and their quotes back to
//! the requester for the gateways (interface 1, `interfaces/gateway/README.md`, parlays), and sequences expiry,
//! resolution, voiding and market states as the legs trade and resolve. `operator` is the API that approves and
//! revokes responders. `README.md` beside `src` says how it restarts.

pub mod operator;
pub mod responders;
pub mod rules;
pub mod service;

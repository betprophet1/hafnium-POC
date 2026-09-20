//! `GET /v4/bootstrap`: which venues an app may point itself at (interfaces/gateway/README.md, choosing an
//! environment).
//!
//! An app is built knowing one address — this one — and asks here where it may go. That is the whole of it. Most
//! callers are told about the live venue and nothing else; a caller whose session belongs to one of the venue's own
//! people is also told which test environments exist, so that a developer can point the app they are holding at the
//! environment their branch is running in.
//!
//! **This is discovery, not authorization, and the difference matters.** What is answered here decides what a client
//! is *told about*. It cannot protect a test environment, because a URL is not a secret and anything reachable will
//! be reached: each environment authenticates its own callers, out of its own records. Withholding the list from
//! everyone else buys two smaller things, both worth having — internal addresses are not advertised, and nobody
//! wanders onto a venue whose money is imaginary while believing it is real.
//!
//! **A credential never crosses.** A session belongs to the venue that issued it, and to no other: the environments
//! keep separate identity services and separate databases, so a member of the live venue is nobody in a test one and
//! must sign in there separately. This is deliberate and is the point on which the whole arrangement rests. A test
//! environment runs a branch nobody has reviewed yet; if it could verify live credentials, then every branch could
//! be written to collect them, and review would be happening after the damage rather than before it (decision G-20).

use crate::api::{ApiError, Gateway};
use crate::http::Request;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// Where the environments are listed, and what is made of that file.
#[derive(Clone, Debug, Default)]
pub struct Environments {
    /// None where this venue has nothing to say about environments, and the route is then not offered at all.
    path: Option<PathBuf>,
}

impl Environments {
    pub fn none() -> Self {
        Self { path: None }
    }

    pub fn from_file(path: impl Into<PathBuf>) -> Self {
        Self { path: Some(path.into()) }
    }

    /// The file as it is now. Read on every request rather than at start, because the list is a ConfigMap in a
    /// cluster and environments come and go all day: a deployment that had to be restarted to notice a new branch
    /// would be restarted more often than it served anyone. The file is a few hundred bytes and this route is asked
    /// once when an app opens, so the read costs nothing worth saving.
    fn read(&self) -> Result<Vec<Environment>, ApiError> {
        let path = self.path.as_ref().ok_or_else(|| ApiError::not_found("no such route"))?;
        let text = fs::read_to_string(path).map_err(|e| {
            venue_telemetry::log::warn(
                "the environments file could not be read",
                &[("file", &path.display().to_string()), ("error", &e.to_string())],
            );
            ApiError::unavailable("the environments are not known here just now")
        })?;
        parse(&text, path)
    }
}

/// One venue an app may point itself at.
#[derive(Clone, Debug)]
struct Environment {
    id: String,
    label: String,
    url: String,
    /// Whether this is the venue whose money is real. Exactly one is, and everything else is a copy of it running a
    /// branch; an app says so where a person can see it, which is the reason this is answered rather than guessed
    /// from the name.
    live: bool,
    /// Whether this entry is the venue answering. Always offered when it is, to anyone, because naming yourself to
    /// somebody already talking to you discloses nothing — the staff gate is about *other* venues' addresses.
    ///
    /// It is what lets an app keep a session across a restart. The app asks the venue a session belongs to whether
    /// it is still on offer, and a test environment that could not say "yes, me" to its own members answered them
    /// an empty list and cost them their sign-in on every launch.
    here: bool,
}

impl Environment {
    fn json(&self) -> Value {
        json!({"id": self.id, "label": self.label, "url": self.url, "live": self.live})
    }
}

fn parse(text: &str, path: &Path) -> Result<Vec<Environment>, ApiError> {
    let unreadable = || {
        venue_telemetry::log::warn("the environments file is not what it should be", &[("file", &path.display())]);
        ApiError::unavailable("the environments are not known here just now")
    };
    let document: Value = serde_json::from_str(text).map_err(|_| unreadable())?;
    let listed = document["environments"].as_array().ok_or_else(unreadable)?;
    listed
        .iter()
        .map(|e| {
            Some(Environment {
                id: e["id"].as_str()?.to_string(),
                label: e["label"].as_str()?.to_string(),
                url: e["url"].as_str()?.to_string(),
                // Absent means not live: a file that forgets to say cannot accidentally dress a branch up as the
                // real venue.
                live: e["live"].as_bool().unwrap_or(false),
                // Absent means not this one. Only whatever writes this file may say it, and that is the deployment
                // rather than the branch running in it.
                here: e["self"].as_bool().unwrap_or(false),
            })
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(unreadable)
}

/// The answer. Anonymous callers, and members who are not the venue's own people, are told about the live venue and
/// nothing else. A refusal of the credential is not an error here: an app asks before anyone has signed in, and an
/// app holding a session that has expired should still learn where the live venue is rather than be turned away.
pub fn answer(gateway: &Gateway, request: &Request, client: &str) -> Result<Value, ApiError> {
    gateway.anonymous_rate(client)?;
    let environments = gateway.environments().read()?;
    let staff = staff_asking(gateway, request);
    let offered: Vec<Value> =
        environments.iter().filter(|e| staff || e.live || e.here).map(Environment::json).collect();
    Ok(json!({"environments": offered}))
}

/// Whether the request carries a session of one of the venue's own people. Anything else at all — no credential, a
/// credential that is not a member's, an identity service that did not answer — is false, because the failure that
/// matters is naming an internal address to somebody who should not have it.
fn staff_asking(gateway: &Gateway, request: &Request) -> bool {
    let Some(token) = crate::rest::bearer(request) else { return false };
    let Ok(credential) = gateway.identity.verify_credential(token) else { return false };
    // An API key belongs to an account rather than to a person, and no account is one of the venue's own people.
    let Some(member) = credential.member.as_deref() else { return false };
    gateway.identity.staff(member)
}

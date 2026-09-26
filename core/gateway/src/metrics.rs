//! What the gateway exports for Prometheus (deploy/observability/metrics.md, venue-gateway).
//!
//! Everything a half counts is found by index in handles made once at start, so an event loop's turn only adds to
//! atomics: no label lookup, no lock and no allocation for a metric. Per-hop latencies of the inputs a half sends are
//! HdrHistogram-backed and recorded on the ingress's and the feed's own threads, never on a loop.

use crate::api::Half;
use crate::limits::monotonic;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use venue_link::submit::Watch;
use venue_telemetry::counters::Counters;
use venue_telemetry::hdr::{Hdr, Recorder};
use venue_telemetry::histogram::Histogram;
use venue_telemetry::prometheus_client::metrics::counter::Counter;
use venue_telemetry::prometheus_client::metrics::family::Family;
use venue_telemetry::prometheus_client::metrics::gauge::Gauge;
use venue_telemetry::prometheus_client::registry::Registry;
use venue_telemetry::{FloatGauge, Labels};

const HALVES: &[&str] = &["public", "private"];

/// REST routes, as templates. `other` is last.
pub const ROUTES: &[&str] = &[
    "/healthz",
    "/v4/markets",
    "/v4/markets/{id}/book",
    "/v4/account",
    "/v4/orders",
    "/v4/orders/{id}",
    "/v4/transfers",
    "/v4/transfers/{id}",
    "/v4/rfqs",
    "/v4/rfqs/{id}",
    "/v4/rfqs/{id}/quotes",
    "/v4/keys",
    "/v4/keys/{id}",
    "other",
];

/// The template a request path belongs to, as an index into `ROUTES`, whatever its method.
pub fn route(path: &str) -> usize {
    let path = path.split('?').next().unwrap_or("");
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let template = match segments.as_slice() {
        ["healthz"] => "/healthz",
        ["v4", "markets"] => "/v4/markets",
        ["v4", "markets", _, "book"] => "/v4/markets/{id}/book",
        ["v4", "account"] => "/v4/account",
        ["v4", "orders"] => "/v4/orders",
        ["v4", "orders", _] => "/v4/orders/{id}",
        ["v4", "transfers"] => "/v4/transfers",
        ["v4", "transfers", _] => "/v4/transfers/{id}",
        ["v4", "rfqs"] => "/v4/rfqs",
        ["v4", "rfqs", _] => "/v4/rfqs/{id}",
        ["v4", "rfqs", _, "quotes"] => "/v4/rfqs/{id}/quotes",
        ["v4", "keys"] => "/v4/keys",
        ["v4", "keys", _] => "/v4/keys/{id}",
        _ => "other",
    };
    ROUTES.iter().position(|r| *r == template).unwrap_or(ROUTES.len() - 1)
}

/// HTTP statuses the gateway answers with or passes on; `other` is last.
const STATUSES: &[&str] = &[
    "200", "201", "202", "204", "400", "401", "403", "404", "405", "409", "413", "422", "429", "500", "502", "503",
    "504", "other",
];

fn status_index(status: u16) -> usize {
    let text = status.to_string();
    STATUSES.iter().position(|s| *s == text).unwrap_or(STATUSES.len() - 1)
}

/// WebSocket ops; `other` and `unreadable` last.
pub const OPS: &[&str] = &[
    "login",
    "heartbeat",
    "subscribe",
    "unsubscribe",
    "order",
    "amend",
    "cancel",
    "cancelAll",
    "requestQuote",
    "attachQuote",
    "subscribeRfqs",
    "unsubscribeRfqs",
    "other",
    "unreadable",
];
pub const OP_OTHER: usize = OPS.len() - 2;
pub const OP_UNREADABLE: usize = OPS.len() - 1;

pub fn op(name: &str) -> usize {
    OPS[..OP_OTHER].iter().position(|o| *o == name).unwrap_or(OP_OTHER)
}

const OUTCOMES: &[&str] = &["accepted", "refused"];

#[derive(Clone, Copy)]
pub enum Api {
    Rest = 0,
    Ws = 1,
}
const APIS: &[&str] = &["rest", "ws"];

const REASONS: &[&str] = &["auth", "permission", "rate_limit", "validation", "not_found", "unavailable", "other"];

/// Why a status is a refusal, as an index into `REASONS`; none for a success.
fn reason(status: u16) -> Option<usize> {
    let name = match status {
        0..=399 => return None,
        401 => "auth",
        403 => "permission",
        429 => "rate_limit",
        404 => "not_found",
        503 => "unavailable",
        400..=499 => "validation",
        _ => "other",
    };
    REASONS.iter().position(|r| *r == name)
}

#[derive(Clone, Copy)]
pub enum Limit {
    Rest = 0,
    Ws = 1,
    Logon = 2,
}
const LIMITS: &[&str] = &["rest", "ws", "logon"];

#[derive(Clone, Copy)]
pub enum Service {
    Node = 0,
    Treasury = 1,
    Identity = 2,
    Parlays = 3,
}
const SERVICES: &[&str] = &["node", "treasury", "identity", "parlays"];

#[derive(Clone, Copy)]
pub enum Outcome {
    Ok = 0,
    Error = 1,
    Busy = 2,
}
const UPSTREAM_OUTCOMES: &[&str] = &["ok", "error", "busy"];

const CLOSE_REASONS: &[&str] = &["client", "heartbeat_missed", "key_withdrawn", "too_far_behind", "other"];

/// The reason label for why a session ended, from what the event loops call it.
fn close_reason(ending: &str) -> usize {
    let name = match ending {
        "closed" => "client",
        "heartbeat missed" => "heartbeat_missed",
        "key no longer valid" => "key_withdrawn",
        "too far behind" => "too_far_behind",
        _ => "other",
    };
    CLOSE_REASONS.iter().position(|r| *r == name).unwrap_or(CLOSE_REASONS.len() - 1)
}

const HOPS: &[&str] = &["handoff_to_sequenced", "sequenced_to_reply", "sequenced_to_feed"];

/// Every metric of the process, registered once.
pub struct Metrics {
    connections_accepted: Counters,
    connections_open: Family<Labels, Gauge>,
    sessions_opened: Counters,
    sessions_open: Family<Labels, Gauge>,
    sessions_closed: Counters,
    http_requests: Counters,
    http_duration: Family<Labels, Histogram>,
    ws_messages: Counters,
    refusals: Counters,
    rate_limited: Counters,
    loop_turn: Family<Labels, Histogram>,
    feed_apply: Family<Labels, Histogram>,
    backlog: Family<Labels, Histogram>,
    slow_consumers: Counters,
    feed_sequence: Family<Labels, Gauge>,
    feed_behind: Family<Labels, FloatGauge>,
    feed_units: Counters,
    feed_reconnects: Counters,
    upstream: Counters,
    upstream_duration: Family<Labels, Histogram>,
    hops: Family<Labels, Hdr>,
    pub audit: AuditMetrics,
}

/// The audit writer's, which one process shares between its halves.
#[derive(Clone)]
pub struct AuditMetrics {
    pub queue_depth: Gauge,
    pub records: Counter,
    pub write: Histogram,
    pub fsync: Histogram,
    pub rotations: Counter,
    pub queue_full_waits: Counter,
}

/// Handles registered nowhere until `Metrics::register` registers them.
impl Default for AuditMetrics {
    fn default() -> Self {
        Self {
            queue_depth: Gauge::default(),
            records: Counter::default(),
            write: Histogram::durations(),
            fsync: Histogram::durations(),
            rotations: Counter::default(),
            queue_full_waits: Counter::default(),
        }
    }
}

fn labels(pairs: &[(&'static str, &str)]) -> Labels {
    pairs.iter().map(|(k, v)| (*k, (*v).to_string())).collect()
}

impl Metrics {
    pub fn register(registry: &mut Registry) -> Arc<Self> {
        let durations = || Family::<Labels, Histogram>::new_with_constructor(Histogram::durations as fn() -> Histogram);
        let metrics = Self {
            connections_accepted: Counters::grid(&[("half", HALVES)]),
            connections_open: Family::default(),
            sessions_opened: Counters::grid(&[("half", HALVES)]),
            sessions_open: Family::default(),
            sessions_closed: Counters::grid(&[("half", HALVES), ("reason", CLOSE_REASONS)]),
            http_requests: Counters::grid(&[("half", HALVES), ("route", ROUTES), ("status", STATUSES)]),
            http_duration: durations(),
            ws_messages: Counters::grid(&[("half", HALVES), ("op", OPS), ("outcome", OUTCOMES)]),
            refusals: Counters::grid(&[("half", HALVES), ("api", APIS), ("reason", REASONS)]),
            rate_limited: Counters::grid(&[("half", HALVES), ("limit", LIMITS)]),
            loop_turn: durations(),
            feed_apply: durations(),
            backlog: Family::<Labels, Histogram>::new_with_constructor(Histogram::counts as fn() -> Histogram),
            slow_consumers: Counters::grid(&[("half", HALVES)]),
            feed_sequence: Family::default(),
            feed_behind: Family::default(),
            feed_units: Counters::grid(&[("half", HALVES)]),
            feed_reconnects: Counters::grid(&[("half", HALVES)]),
            upstream: Counters::grid(&[("half", HALVES), ("service", SERVICES), ("outcome", UPSTREAM_OUTCOMES)]),
            upstream_duration: durations(),
            hops: Family::default(),
            audit: AuditMetrics::default(),
        };
        let m = &metrics;
        let r = registry;
        r.register(
            "venue_gateway_connections_accepted",
            "Connections an event loop took on",
            m.connections_accepted.clone(),
        );
        r.register("venue_gateway_connections_open", "Connections held by the event loops", m.connections_open.clone());
        r.register("venue_gateway_sessions_opened", "WebSocket sessions opened", m.sessions_opened.clone());
        r.register("venue_gateway_sessions_open", "WebSocket sessions open", m.sessions_open.clone());
        r.register("venue_gateway_sessions_closed", "WebSocket sessions ended, by why", m.sessions_closed.clone());
        r.register(
            "venue_gateway_http_requests",
            "REST requests answered, by route and status",
            m.http_requests.clone(),
        );
        r.register(
            "venue_gateway_http_request_duration_seconds",
            "From a whole REST request read to its response ready",
            m.http_duration.clone(),
        );
        r.register(
            "venue_gateway_ws_messages",
            "WebSocket messages from clients, by op and outcome",
            m.ws_messages.clone(),
        );
        r.register("venue_gateway_refusals", "Errors sent to clients, by reason", m.refusals.clone());
        r.register("venue_gateway_rate_limited", "Flood limit hits", m.rate_limited.clone());
        r.register("venue_gateway_loop_turn_duration_seconds", "One event loop turn", m.loop_turn.clone());
        r.register(
            "venue_gateway_feed_apply_duration_seconds",
            "Applying one feed unit to the hub: the work of the one thread per half that does it",
            m.feed_apply.clone(),
        );
        r.register(
            "venue_gateway_outbound_backlog_messages",
            "Messages queued for a WebSocket connection when its loop came to send them",
            m.backlog.clone(),
        );
        r.register(
            "venue_gateway_slow_consumer_disconnects",
            "Connections dropped for falling too far behind",
            m.slow_consumers.clone(),
        );
        r.register("venue_gateway_feed_sequence", "Sequence of the last feed unit applied", m.feed_sequence.clone());
        r.register(
            "venue_gateway_feed_behind_seconds",
            "Wall clock minus sequencedAt of the last feed unit, when applied",
            m.feed_behind.clone(),
        );
        r.register("venue_gateway_feed_units", "Feed units applied", m.feed_units.clone());
        r.register("venue_gateway_feed_reconnects", "Connections to the node's feed", m.feed_reconnects.clone());
        r.register("venue_gateway_upstream_requests", "Calls to the node and services, by outcome", m.upstream.clone());
        r.register(
            "venue_gateway_upstream_duration_seconds",
            "From handing a call over to its answer arriving",
            m.upstream_duration.clone(),
        );
        r.register(
            "venue_gateway_hop_latency_seconds",
            "Journal-derived hops of inputs this gateway sent (HdrHistogram)",
            m.hops.clone(),
        );
        r.register(
            "venue_gateway_audit_queue_depth",
            "Audit records taken and not yet written",
            m.audit.queue_depth.clone(),
        );
        r.register("venue_gateway_audit_records", "Audit records written", m.audit.records.clone());
        r.register(
            "venue_gateway_audit_write_duration_seconds",
            "Writing one batch of audit records",
            m.audit.write.clone(),
        );
        r.register(
            "venue_gateway_audit_fsync_duration_seconds",
            "Syncing the audit file after a batch",
            m.audit.fsync.clone(),
        );
        r.register("venue_gateway_audit_rotations", "Audit files rotated at midnight", m.audit.rotations.clone());
        r.register(
            "venue_gateway_audit_queue_full_waits",
            "Times an event loop waited for room on a full audit queue",
            m.audit.queue_full_waits.clone(),
        );
        Arc::new(metrics)
    }

    /// Metrics registered nowhere, for a gateway built by a test.
    pub fn unregistered() -> Arc<Self> {
        Self::register(&mut Registry::default())
    }

    /// One half's handles.
    pub fn half(self: &Arc<Self>, half: Half) -> Arc<HalfMetrics> {
        let position = match half {
            Half::Public => 0,
            Half::Private => 1,
        };
        let name = HALVES[position];
        let h = |extra: &[(&'static str, &str)]| {
            let mut l = labels(&[("half", name)]);
            l.extend(labels(extra));
            l
        };
        let hop = |hop: &str| self.hops.get_or_create_owned(&h(&[("hop", hop), ("clock", "software")]));
        Arc::new(HalfMetrics {
            half: position,
            all: Arc::clone(self),
            connections_open: self.connections_open.get_or_create_owned(&h(&[])),
            sessions_open: self.sessions_open.get_or_create_owned(&h(&[])),
            http_duration: ROUTES.iter().map(|r| self.http_duration.get_or_create_owned(&h(&[("route", r)]))).collect(),
            loop_turn: self.loop_turn.get_or_create_owned(&h(&[])),
            feed_apply: self.feed_apply.get_or_create_owned(&h(&[])),
            backlog: self.backlog.get_or_create_owned(&h(&[])),
            feed_sequence: self.feed_sequence.get_or_create_owned(&h(&[])),
            feed_behind: self.feed_behind.get_or_create_owned(&h(&[])),
            upstream_duration: SERVICES
                .iter()
                .map(|s| self.upstream_duration.get_or_create_owned(&h(&[("service", s)])))
                .collect(),
            handoff: Mutex::new(hop(HOPS[0]).recorder(MERGE_EVERY)),
            reply: Mutex::new(hop(HOPS[1]).recorder(MERGE_EVERY)),
            feed: Mutex::new(hop(HOPS[2]).recorder(MERGE_EVERY)),
        })
    }
}

/// How often a hop recorder merges into the histogram a scrape reads.
const MERGE_EVERY: Duration = Duration::from_millis(250);

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One half's view of the metrics: its label fixed, every handle ready.
pub struct HalfMetrics {
    half: usize,
    all: Arc<Metrics>,
    pub connections_open: Gauge,
    pub sessions_open: Gauge,
    http_duration: Vec<Histogram>,
    pub loop_turn: Histogram,
    /// How long one feed unit takes to apply. `feed_behind` says how stale the view is; this says what that
    /// costs, which is the headroom on the one thread per half that applies every unit (ADR-0002).
    pub feed_apply: Histogram,
    pub backlog: Histogram,
    feed_sequence: Gauge,
    feed_behind: FloatGauge,
    upstream_duration: Vec<Histogram>,
    /// Each recorded by one thread only (the ingress reader, the feed follower); the lock is never contended.
    handoff: Mutex<Recorder>,
    reply: Mutex<Recorder>,
    feed: Mutex<Recorder>,
}

impl HalfMetrics {
    pub fn connection_accepted(&self) {
        self.all.connections_accepted.inc(self.half);
        self.connections_open.inc();
    }

    pub fn connection_closed(&self) {
        self.connections_open.dec();
    }

    pub fn session_opened(&self) {
        self.all.sessions_opened.inc(self.half);
        self.sessions_open.inc();
    }

    pub fn session_closed(&self, ending: &str) {
        self.sessions_open.dec();
        if ending == "too far behind" {
            self.slow_consumer();
        }
        self.all
            .sessions_closed
            .inc(Counters::index(&[self.half, close_reason(ending)], &[HALVES.len(), CLOSE_REASONS.len()]));
    }

    /// A REST response ready for the client, and how long since the request was whole.
    pub fn http_answered(&self, route: usize, status: u16, took: Option<Duration>) {
        let sizes = [HALVES.len(), ROUTES.len(), STATUSES.len()];
        self.all.http_requests.inc(Counters::index(&[self.half, route, status_index(status)], &sizes));
        if let (Some(took), Some(h)) = (took, self.http_duration.get(route)) {
            h.observe_duration(took);
        }
        self.refused_status(Api::Rest, status);
    }

    /// An error of this status sent to a client, counted by reason; a success counts nothing.
    pub fn refused_status(&self, api: Api, status: u16) {
        if let Some(reason) = reason(status) {
            self.all
                .refusals
                .inc(Counters::index(&[self.half, api as usize, reason], &[HALVES.len(), APIS.len(), REASONS.len()]));
        }
    }

    pub fn ws_message(&self, op: usize, refused_with: Option<u16>) {
        let outcome = usize::from(refused_with.is_some());
        let sizes = [HALVES.len(), OPS.len(), OUTCOMES.len()];
        self.all.ws_messages.inc(Counters::index(&[self.half, op, outcome], &sizes));
        if let Some(status) = refused_with {
            self.refused_status(Api::Ws, status);
        }
    }

    pub fn rate_limited(&self, limit: Limit) {
        self.all.rate_limited.inc(Counters::index(&[self.half, limit as usize], &[HALVES.len(), LIMITS.len()]));
    }

    pub fn slow_consumer(&self) {
        self.all.slow_consumers.inc(self.half);
    }

    pub fn feed_connected(&self) {
        self.all.feed_reconnects.inc(self.half);
    }

    /// A feed unit applied: its sequence and when it was sequenced, against the wall clock now.
    pub fn feed_unit(&self, sequence: u64, sequenced_at: u64, now_ns: u64) {
        self.all.feed_units.inc(self.half);
        self.feed_sequence.set(i64::try_from(sequence).unwrap_or(i64::MAX));
        let behind = now_ns.saturating_sub(sequenced_at);
        self.feed_behind.set(venue_telemetry::seconds(behind));
        // A backlog replayed on connecting is history, not a hop: only units sequenced in the last minute count.
        if behind < 60_000_000_000 {
            locked(&self.feed).record(behind, monotonic());
        }
    }

    /// A call to an upstream service finished, or was refused before it was sent (`Busy`, with no duration).
    pub fn upstream(&self, service: Service, outcome: Outcome, took: Option<Duration>) {
        let sizes = [HALVES.len(), SERVICES.len(), UPSTREAM_OUTCOMES.len()];
        self.all.upstream.inc(Counters::index(&[self.half, service as usize, outcome as usize], &sizes));
        if let (Some(took), Some(h)) = (took, self.upstream_duration.get(service as usize)) {
            h.observe_duration(took);
        }
    }

    /// Timing an upstream call from now.
    pub fn started() -> Instant {
        monotonic()
    }
}

/// The ingress's watcher: round trips to the node, and the two hops each input's `sequencedAt` splits them into.
pub struct IngressWatch(pub Arc<HalfMetrics>);

impl Watch for IngressWatch {
    fn now_ns(&self) -> u64 {
        venue_telemetry::wall_ns()
    }

    fn answered(&self, queued_ns: u64, read_ns: u64, sequenced_at: Option<u64>) {
        let m = &self.0;
        m.upstream(Service::Node, Outcome::Ok, Some(Duration::from_nanos(read_ns.saturating_sub(queued_ns))));
        if let Some(at) = sequenced_at {
            let now = monotonic();
            locked(&m.handoff).record(at.saturating_sub(queued_ns), now);
            locked(&m.reply).record(read_ns.saturating_sub(at), now);
        }
    }

    fn failed(&self) {
        self.0.upstream(Service::Node, Outcome::Error, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use venue_telemetry::prometheus_client::encoding::text::encode;

    #[test]
    fn routes_are_templates_and_anything_else_is_other() {
        assert_eq!(ROUTES[route("/v4/orders/123?x=1")], "/v4/orders/{id}");
        assert_eq!(ROUTES[route("/v4/markets/10010/book")], "/v4/markets/{id}/book");
        assert_eq!(ROUTES[route("/v4/rfqs/abc/quotes")], "/v4/rfqs/{id}/quotes");
        assert_eq!(ROUTES[route("/v4/account/12/secret")], "other");
        assert_eq!(ROUTES[route("/../../etc/passwd")], "other");
        assert_eq!(OPS[op("cancelAll")], "cancelAll");
        assert_eq!(OPS[op("dropTables")], "other");
    }

    #[test]
    fn a_half_counts_under_its_own_label() {
        let mut registry = Registry::default();
        let metrics = Metrics::register(&mut registry);
        let private = metrics.half(Half::Private);
        private.http_answered(route("/v4/orders"), 429, Some(Duration::from_micros(40)));
        private.ws_message(op("order"), Some(401));
        private.session_opened();
        private.session_closed("heartbeat missed");
        IngressWatch(Arc::clone(&private)).answered(1_000_000, 1_900_000, Some(1_400_000));
        let mut text = String::new();
        encode(&mut text, &registry).expect("encoded");
        for line in [
            "venue_gateway_http_requests_total{half=\"private\",route=\"/v4/orders\",status=\"429\"} 1\n",
            "venue_gateway_refusals_total{half=\"private\",api=\"rest\",reason=\"rate_limit\"} 1\n",
            "venue_gateway_refusals_total{half=\"private\",api=\"ws\",reason=\"auth\"} 1\n",
            "venue_gateway_ws_messages_total{half=\"private\",op=\"order\",outcome=\"refused\"} 1\n",
            "venue_gateway_sessions_closed_total{half=\"private\",reason=\"heartbeat_missed\"} 1\n",
            "venue_gateway_sessions_open{half=\"private\"} 0\n",
            "venue_gateway_upstream_requests_total{half=\"private\",service=\"node\",outcome=\"ok\"} 1\n",
            "venue_gateway_hop_latency_seconds_count{half=\"private\",hop=\"handoff_to_sequenced\",clock=\"software\"} 1\n",
        ] {
            assert!(text.contains(line), "{line} in {text}");
        }
    }
}

//! What the node exports for Prometheus (deploy/observability/metrics.md, venue-node): inputs by type, the journal's
//! append and sync times, the sequence, and the feed's subscribers. Nothing here looks inside the engine, and nothing
//! times it: the node never produces a latency number for the engine (README), and the capacity the engine's tables
//! have left is derived from the journal by `venue-observer`.
//!
//! Every handle is made once at start; the pipeline only adds to atomics.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use venue_journal::*;
use venue_telemetry::histogram::Histogram;
use venue_telemetry::prometheus_client::metrics::counter::Counter;
use venue_telemetry::prometheus_client::metrics::family::Family;
use venue_telemetry::prometheus_client::metrics::gauge::Gauge;
use venue_telemetry::prometheus_client::registry::Registry;
use venue_telemetry::{FloatGauge, Labels};

/// Every journal input's name, by template ID (interfaces/sbe/venue-journal.xml). The IDs come from the generated
/// codec, so a renumbered message cannot be counted under the wrong name.
const INPUTS: &[(u16, &str)] = &[
    (new_order_codec::SBE_TEMPLATE_ID, "NewOrder"),
    (cancel_order_codec::SBE_TEMPLATE_ID, "CancelOrder"),
    (amend_order_codec::SBE_TEMPLATE_ID, "AmendOrder"),
    (mass_cancel_codec::SBE_TEMPLATE_ID, "MassCancel"),
    (batch_orders_codec::SBE_TEMPLATE_ID, "BatchOrders"),
    (set_order_group_codec::SBE_TEMPLATE_ID, "SetOrderGroup"),
    (reset_order_group_codec::SBE_TEMPLATE_ID, "ResetOrderGroup"),
    (time_tick_codec::SBE_TEMPLATE_ID, "TimeTick"),
    (engine_started_codec::SBE_TEMPLATE_ID, "EngineStarted"),
    (seed_set_codec::SBE_TEMPLATE_ID, "SeedSet"),
    (activate_protocol_codec::SBE_TEMPLATE_ID, "ActivateProtocol"),
    (sleeve_adjust_codec::SBE_TEMPLATE_ID, "SleeveAdjust"),
    (define_tick_table_codec::SBE_TEMPLATE_ID, "DefineTickTable"),
    (define_event_codec::SBE_TEMPLATE_ID, "DefineEvent"),
    (list_instrument_codec::SBE_TEMPLATE_ID, "ListInstrument"),
    (set_event_phase_codec::SBE_TEMPLATE_ID, "SetEventPhase"),
    (set_market_state_codec::SBE_TEMPLATE_ID, "SetMarketState"),
    (set_venue_mode_codec::SBE_TEMPLATE_ID, "SetVenueMode"),
    (resolve_event_codec::SBE_TEMPLATE_ID, "ResolveEvent"),
    (expire_parlay_codec::SBE_TEMPLATE_ID, "ExpireParlay"),
    (set_rule_limits_codec::SBE_TEMPLATE_ID, "SetRuleLimits"),
    (set_group_parameters_codec::SBE_TEMPLATE_ID, "SetGroupParameters"),
    (set_account_controls_codec::SBE_TEMPLATE_ID, "SetAccountControls"),
    (set_maker_protection_codec::SBE_TEMPLATE_ID, "SetMakerProtection"),
    (transfer_out_codec::SBE_TEMPLATE_ID, "TransferOut"),
    (transfer_in_codec::SBE_TEMPLATE_ID, "TransferIn"),
    (set_fee_schedule_codec::SBE_TEMPLATE_ID, "SetFeeSchedule"),
    (set_fee_override_codec::SBE_TEMPLATE_ID, "SetFeeOverride"),
    (remove_fee_override_codec::SBE_TEMPLATE_ID, "RemoveFeeOverride"),
    (set_listing_parameters_codec::SBE_TEMPLATE_ID, "SetListingParameters"),
];

/// A feed subscriber's progress: units handed to its thread, and units its thread has written to the socket.
#[derive(Default)]
pub struct Subscriber {
    pub sent: AtomicU64,
    pub written: AtomicU64,
}

pub struct NodeMetrics {
    /// By template ID; `other` last.
    inputs: Vec<(u16, Counter)>,
    other_input: Counter,
    pub ingress_refused: Counter,
    pub ingress_connections: Gauge,
    journal_append: Histogram,
    journal_fsync: Histogram,
    journal_bytes: Counter,
    sequence: Gauge,
    sequenced_at: FloatGauge,
    pub feed_published_sequence: Gauge,
    pub feed_units_published: Counter,
    feed_subscribers: Gauge,
    backlog_max: Gauge,
    backlog_sum: Gauge,
    pub subscribers_disconnected: Counter,
}

fn counter(registry: &mut Registry, name: &str, help: &str) -> Counter {
    let c = Counter::default();
    registry.register(name, help, c.clone());
    c
}

fn gauge(registry: &mut Registry, name: &str, help: &str) -> Gauge {
    let g = Gauge::default();
    registry.register(name, help, g.clone());
    g
}

fn histogram(registry: &mut Registry, name: &str, help: &str) -> Histogram {
    let h = Histogram::durations();
    registry.register(name, help, h.clone());
    h
}

impl NodeMetrics {
    pub fn register(registry: &mut Registry) -> Arc<Self> {
        let inputs_by_type = Family::<Labels, Counter>::default();
        let inputs = INPUTS
            .iter()
            .map(|&(id, name)| (id, inputs_by_type.get_or_create_owned(&vec![("type", name.into())])))
            .collect();
        let other_input = inputs_by_type.get_or_create_owned(&vec![("type", "other".into())]);
        registry.register("venue_node_inputs", "Inputs journaled, by message name", inputs_by_type);
        Arc::new(Self {
            inputs,
            other_input,
            ingress_refused: counter(
                registry,
                "venue_node_ingress_refused",
                "Ingress frames refused, which close their connection",
            ),
            ingress_connections: gauge(registry, "venue_node_ingress_connections", "Ingress connections open"),
            journal_append: histogram(
                registry,
                "venue_node_journal_append_duration_seconds",
                "Appending an input to the journal and flushing it to the operating system",
            ),
            journal_fsync: histogram(
                registry,
                "venue_node_journal_fsync_duration_seconds",
                "fsync of the journal after an input",
            ),
            journal_bytes: counter(
                registry,
                "venue_node_journal_bytes",
                "Bytes appended to the journal, framing included",
            ),
            sequence: gauge(registry, "venue_node_sequence", "Sequence of the last input journaled"),
            sequenced_at: {
                let g = FloatGauge::default();
                registry.register(
                    "venue_node_sequenced_at_seconds",
                    "sequencedAt of the last input journaled",
                    g.clone(),
                );
                g
            },
            feed_published_sequence: gauge(
                registry,
                "venue_node_feed_published_sequence",
                "Sequence of the last unit published to the feed",
            ),
            feed_units_published: counter(
                registry,
                "venue_node_feed_units_published",
                "Units published to live feed subscribers",
            ),
            feed_subscribers: gauge(registry, "venue_node_feed_subscribers", "Feed subscribers connected"),
            backlog_max: gauge(
                registry,
                "venue_node_feed_subscriber_backlog_units_max",
                "Most units any one subscriber has been sent and not yet written",
            ),
            backlog_sum: gauge(
                registry,
                "venue_node_feed_subscriber_backlog_units_sum",
                "Units sent to subscribers and not yet written, summed",
            ),
            subscribers_disconnected: counter(
                registry,
                "venue_node_feed_subscribers_disconnected",
                "Feed subscribers whose connection ended",
            ),
        })
    }

    /// One input stamped and journaled: its type, its size, and how long the journal took.
    pub fn journaled(&self, input: &[u8], sequence: u64, at: u64, append: Duration, fsync: Option<Duration>) {
        let template = input.get(2..4).map_or(0, |t| u16::from_le_bytes([t[0], t[1]]));
        self.inputs.iter().find(|(id, _)| *id == template).map_or(&self.other_input, |(_, c)| c).inc();
        self.journal_bytes.inc_by(4 + input.len() as u64);
        self.journal_append.observe_duration(append);
        if let Some(fsync) = fsync {
            self.journal_fsync.observe_duration(fsync);
        }
        self.sequence.set(i64::try_from(sequence).unwrap_or(i64::MAX));
        self.sequenced_at.set(venue_telemetry::seconds(at));
    }

    /// The feed's subscribers as they stand, taken on the tick.
    pub fn subscribers<'a>(&self, subscribers: impl Iterator<Item = &'a Subscriber>) {
        let (mut count, mut max, mut sum) = (0i64, 0i64, 0i64);
        for s in subscribers {
            let backlog = s.sent.load(Ordering::Relaxed).saturating_sub(s.written.load(Ordering::Relaxed));
            let backlog = i64::try_from(backlog).unwrap_or(i64::MAX);
            count += 1;
            max = max.max(backlog);
            sum = sum.saturating_add(backlog);
        }
        self.feed_subscribers.set(count);
        self.backlog_max.set(max);
        self.backlog_sum.set(sum);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use venue_journal::message_header_codec::ENCODED_LENGTH as HDR;
    use venue_telemetry::prometheus_client::encoding::text::encode;

    #[test]
    fn an_input_counts_under_its_name_and_moves_the_sequence() {
        let mut registry = Registry::default();
        let metrics = NodeMetrics::register(&mut registry);
        let tick = crate::sequencer::time_tick();
        assert!(tick.len() > HDR);
        metrics.journaled(&tick, 42, 1_500_000_000, Duration::from_micros(30), None);
        metrics.journaled(
            &[0, 0, 0xff, 0xff],
            43,
            1_600_000_000,
            Duration::from_micros(30),
            Some(Duration::from_millis(2)),
        );
        let subscriber = Subscriber::default();
        subscriber.sent.store(9, Ordering::Relaxed);
        subscriber.written.store(4, Ordering::Relaxed);
        metrics.subscribers([subscriber, Subscriber::default()].iter());
        let mut text = String::new();
        encode(&mut text, &registry).expect("encoded");
        for line in [
            "venue_node_inputs_total{type=\"TimeTick\"} 1\n",
            "venue_node_inputs_total{type=\"other\"} 1\n",
            "venue_node_sequence 43\n",
            "venue_node_sequenced_at_seconds 1.6\n",
            "venue_node_journal_fsync_duration_seconds_count 1\n",
            "venue_node_feed_subscribers 2\n",
            "venue_node_feed_subscriber_backlog_units_max 5\n",
        ] {
            assert!(text.contains(line), "{line} in {text}");
        }
    }
}

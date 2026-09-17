# Metrics reference

What the venue's processes export for Prometheus, and the labels each metric carries. The stack that scrapes them,
the dashboards and the alerts are in [README.md](README.md).

## Conventions

- **Where.** Every process serves `GET /metrics` in the OpenMetrics text format on an internal port of its own, never
  on a client-facing listener. The port is configuration (below); a process given none serves no metrics. In the
  chart the ports are named `metrics` and only the monitoring namespace may reach them (network policy
  `<release>-metrics`).

  | Process | Flag / environment | Chart port |
  |---|---|---|
  | `venue-node serve` | `--metrics ADDR` / `VENUE_METRICS` | 9101 |
  | `venue-gateway` | `--metrics ADDR` / `VENUE_METRICS` | 9102 |
  | `venue-observer` | `--listen ADDR` / `VENUE_LISTEN` (metrics and the operator endpoints) | 9103 |
  | FIX adapter | `--metrics-listen HOST:PORT` / `VENUE_FIX_METRICS_LISTEN` | 9104 |

- **Names.** `venue_<component>_<what>_<unit>`, with the OpenMetrics suffixes: counters end in `_total`, durations are
  in seconds, sizes in bytes, money in integer cents (`_cents`), quantities in contracts (`_contracts`). Histograms are
  classic Prometheus histograms (`_bucket`, `_sum`, `_count`).
- **Labels added by the scrape, not the process.** `namespace`, `pod`, `container`, `instance` and `job` come from
  Kubernetes service discovery; `env` (the environment, `metrics.env` in the venue-mini chart) and `profile` (the
  environment's functional profile, `metrics.profile`, blueprint §16) are added to every venue series by the
  chart's PodMonitor. A laptop or ephemeral environment is `profile="functional"`, and the latency dashboard refuses
  to plot series so labelled.
- **Bounded cardinality.** No label ever holds an account ID, order ID, client order ID, token, key ID or address.
  Label values come from fixed sets (listed with each metric); the only open-ended one, `market`, is limited to
  markets the engine currently lists, is removed when the engine releases the market (`EventReleased`, spec E-8.4),
  and is capped by `--max-market-series` (default 500), past which new markets count only in the totals.
- **Latency numbers.** The per-hop histograms below are HdrHistogram-backed (full resolution in the process, exported
  at fixed bucket bounds from 10 µs to 60 s), and each hop says with `clock` which kind of timestamp bounds it. On a
  functional profile they are not latency numbers anyone may quote (blueprint §16).

## venue-gateway

Every gateway series carries `half` (`public` or `private`) except the audit writer's, which one process shares.

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `venue_gateway_build_info` | gauge | `version` | 1, for joins and to see what runs |
| `venue_gateway_connections_accepted_total` | counter | `half` | Connections an event loop took on |
| `venue_gateway_connections_open` | gauge | `half` | Connections held by the event loops: undecided, REST and WebSocket |
| `venue_gateway_sessions_opened_total` | counter | `half` | WebSocket sessions opened |
| `venue_gateway_sessions_open` | gauge | `half` | WebSocket sessions open now |
| `venue_gateway_sessions_closed_total` | counter | `half`, `reason` | `client` (closed by the client or the network), `heartbeat_missed`, `key_withdrawn`, `too_far_behind`, `arrival_timeout` |
| `venue_gateway_http_requests_total` | counter | `half`, `route`, `status` | REST requests answered, by route template and HTTP status |
| `venue_gateway_http_request_duration_seconds` | histogram | `half`, `route` | From a whole request read to its response ready to write |
| `venue_gateway_ws_messages_total` | counter | `half`, `op`, `outcome` | WebSocket messages from clients by `op` (`login`, `heartbeat`, `subscribe`, `unsubscribe`, `order`, `amend`, `cancel`, `cancelAll`, `requestQuote`, `attachQuote`, `subscribeRfqs`, `unsubscribeRfqs`, `other`, `unreadable`); `outcome` is `accepted` or `refused` (refused at once) |
| `venue_gateway_refusals_total` | counter | `half`, `api`, `reason` | Errors sent to clients: `api` is `rest` or `ws`; `reason` is `auth` (401), `permission` (403), `rate_limit` (429), `validation` (400 and other 4xx), `not_found` (404), `unavailable` (503), `other` |
| `venue_gateway_rate_limited_total` | counter | `half`, `limit` | Flood limit hits: `limit` is `rest` (per account), `ws` (per session) or `logon` (per account) |
| `venue_gateway_loop_turn_duration_seconds` | histogram | `half` | One event loop turn: from `poll` returning to the loop's next `poll` |
| `venue_gateway_outbound_backlog_messages` | histogram | `half` | Messages the hub had queued for a WebSocket connection when its loop came to send them |
| `venue_gateway_slow_consumer_disconnects_total` | counter | `half` | Connections dropped for falling more than 4 MiB behind (`too_far_behind`) |
| `venue_gateway_feed_sequence` | gauge | `half` | Sequence of the last feed unit this half applied |
| `venue_gateway_feed_behind_seconds` | gauge | `half` | Wall clock minus `sequencedAt` of the last unit applied, at the moment it was applied |
| `venue_gateway_feed_units_total` | counter | `half` | Feed units applied |
| `venue_gateway_feed_reconnects_total` | counter | `half` | Connections to the node's feed (the first one included) |
| `venue_gateway_upstream_requests_total` | counter | `half`, `service`, `outcome` | Calls to `node` (ingress), `treasury`, `identity` and `parlays`, as the gateway saw them: `ok`, `error` (failed or not answered) or `busy` (refused before sending: too many at once or queue full) |
| `venue_gateway_upstream_duration_seconds` | histogram | `half`, `service` | From handing a call over to its answer arriving (`node`: to the node's reply on the ingress) |
| `venue_gateway_hop_latency_seconds` | histogram (HdrHistogram) | `half`, `hop`, `clock` | Journal-derived hops of an input this gateway sent (below) |
| `venue_gateway_audit_queue_depth` | gauge | | Records taken and not yet written |
| `venue_gateway_audit_records_total` | counter | | Records written |
| `venue_gateway_audit_write_duration_seconds` | histogram | | Writing one batch of records (without the sync) |
| `venue_gateway_audit_fsync_duration_seconds` | histogram | | Syncing the file after a batch (with `--audit-fsync true`) |
| `venue_gateway_audit_rotations_total` | counter | | Files closed and a new one opened at midnight UTC |
| `venue_gateway_audit_queue_full_waits_total` | counter | | Times an event loop waited for room on a full audit queue |

Route templates (`route`): `/healthz`, `/v4/markets`, `/v4/markets/{id}/book`, `/v4/account`, `/v4/orders`,
`/v4/orders/{id}`, `/v4/transfers`, `/v4/transfers/{id}`, `/v4/rfqs`, `/v4/rfqs/{id}`, `/v4/rfqs/{id}/quotes`,
`/v4/keys`, `/v4/keys/{id}`, and `other` for any path that is none of these (unreadable requests included).

## venue-node

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `venue_node_build_info` | gauge | `version`, `capacity` | 1; `capacity` is the engine's capacity profile (`small`, `launch`, `scale`) |
| `venue_node_inputs_total` | counter | `type` | Inputs journaled, by message name (`NewOrder`, `TimeTick`, `ListInstrument`, ... every input of `interfaces/sbe/venue-journal.xml`; `other` for none of them) |
| `venue_node_ingress_refused_total` | counter | | Frames refused on the ingress (not a client message or command), which close their connection |
| `venue_node_ingress_connections` | gauge | | Ingress connections open |
| `venue_node_journal_append_duration_seconds` | histogram | | Appending one input to the journal and flushing it to the operating system |
| `venue_node_journal_fsync_duration_seconds` | histogram | | `fsync` of the journal after an input (only with `--fsync`) |
| `venue_node_journal_bytes_total` | counter | | Bytes appended to the journal, framing included |
| `venue_node_sequence` | gauge | | Sequence of the last input journaled |
| `venue_node_sequenced_at_seconds` | gauge | | `sequencedAt` of that input, as seconds since the epoch |
| `venue_node_feed_published_sequence` | gauge | | Sequence of the last unit published to the feed (quiet `TimeTick`s are not published) |
| `venue_node_feed_units_published_total` | counter | | Units published to live feed subscribers |
| `venue_node_feed_subscribers` | gauge | | Feed subscribers connected |
| `venue_node_feed_subscriber_backlog_units_max` | gauge | | Most units any one subscriber has been sent and not yet written to its socket |
| `venue_node_feed_subscriber_backlog_units_sum` | gauge | | The same, summed over subscribers |
| `venue_node_feed_subscribers_disconnected_total` | counter | | Subscribers whose connection ended |

The node exports no capacity usage: the engine's tables are not readable from outside without new accessors, so usage
is derived from the journal by `venue-observer` (below). It exports no timing of the engine itself either.

## venue-observer: journal-derived

`venue-observer` follows the node's feed, the journal and the results of every input, as any downstream reader does,
and derives these from it; nothing here instruments the engine (blueprint §14, per-hop histograms from the journal).

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `venue_observer_build_info` | gauge | `version`, `capacity` | 1 |
| `venue_observer_feed_sequence` | gauge | | Sequence of the last unit applied |
| `venue_observer_feed_behind_seconds` | gauge | | Wall clock minus `sequencedAt` of the last unit, when applied |
| `venue_observer_feed_reconnects_total` | counter | | Connections to the feed |
| `venue_hop_latency_seconds` | histogram (HdrHistogram) | `hop`, `clock` | Journal-derived hops (below) |
| `venue_inputs_total` | counter | `type` | Inputs with results, by message name (quiet ticks are not on the feed) |
| `venue_order_rejections_total` | counter | `reason` | `OrderRejected` by `RejectReason` |
| `venue_command_outcomes_total` | counter | `type`, `outcome`, `reason` | `CommandApplied` by command, `APPLIED`/`REJECTED`/`DUPLICATE`, and `RejectReason` (`NONE` when applied) |
| `venue_orders_accepted_total` | counter | | `OrderAccepted` |
| `venue_order_cancels_total` | counter | `reason` | `OrderCancelled` by `CancelReason` |
| `venue_fills_total` | counter | | `Fill` results |
| `venue_traded_contracts_total` | counter | | Contracts traded |
| `venue_traded_notional_cents_total` | counter | | YES cost of what traded, in cents (spec E-2.4) |
| `venue_fees_cents_total` | counter | `role`, `kind` | Fees from fills: `role` `maker`/`taker`, `kind` `charged` (positive fee) or `rebate` (a negative one, as a positive number) |
| `venue_payouts_total` | counter | `kind` | `SettlementPayout` results by `ResolutionKind` |
| `venue_payout_cents_total` | counter | `kind` | Their cents (a void's negative returns counted as their absolute value in `venue_void_debits_cents_total`) |
| `venue_void_debits_cents_total` | counter | | Negative void returns, as a positive number |
| `venue_events_defined_total` | counter | | `DefineEvent` applied |
| `venue_events_resolved_total` | counter | `kind` | `ResolveEvent` applied (and `ExpireParlay`, as `RESOLVED`) |
| `venue_events_released_total` | counter | | `EventReleased` |
| `venue_event_phase_changes_total` | counter | `phase`, `source` | `SetEventPhase` applied, by `EventPhase` and `PhaseSource` |
| `venue_markets_listed_total` | counter | `class` | `ListInstrument` applied, by `InstrumentClass` |
| `venue_market_state_changes_total` | counter | `state` | `MarketStateChanged` by `MarketState` |
| `venue_last_event_defined_timestamp_seconds` | gauge | | Sequencer time of the last `DefineEvent`: oracle cadence as the journal saw it |
| `venue_markets` | gauge | `state`, `class` | Markets the engine lists now |
| `venue_events` | gauge | `status` | Events carried now: `scheduled`, `started`, `ended`, `resolved` |
| `venue_open_orders` | gauge | | Resting orders |
| `venue_resting_contracts` | gauge | `side` | Resting contracts, `BUY` / `SELL` |
| `venue_accounts_active` | gauge | | Accounts with a sleeve |
| `venue_capacity_used` | gauge | `resource` | `events`, `instruments`, `levels` (twice each listed book's ladder, as the engine reserves them), `orders`, `accounts` |
| `venue_capacity_limit` | gauge | `resource`, `source` | `source="profile"`: the capacity profile's table size; `source="listing"`: the sequenced `SetListingParameters` / `SetGroupParameters` limit, when one has been set |
| `venue_market_info` | gauge | `market`, `event`, `class`, `title` | 1 for each listed market, for joins (`title` from the catalogue, or empty) |
| `venue_market_traded_contracts_total` | counter | `market` | Contracts traded in the market |
| `venue_market_traded_notional_cents_total` | counter | `market` | Their YES cost |
| `venue_market_fills_total` | counter | `market` | Fills in the market |
| `venue_market_orders_accepted_total` | counter | `market` | Orders accepted in the market (order-to-trade ratio with the fills) |
| `venue_market_best_price` | gauge | `market`, `side` | Best bid (`BUY`) or ask (`SELL`) as a probability 0–1; absent while that side is empty |
| `venue_market_spread` | gauge | `market` | Best ask minus best bid, as a probability; absent unless two-sided |
| `venue_market_depth_at_touch_contracts` | gauge | `market`, `side` | Contracts at the best price |
| `venue_market_resting_contracts` | gauge | `market`, `side` | Contracts resting on the side |
| `venue_market_two_sided` | gauge | `market` | 1 while the book has a bid and an ask, else 0 (`avg_over_time` gives the time two-sided) |

### Per-hop latency: what can be measured today

The journal has one timestamp per input, `RecordHeader.sequencedAt`, stamped by the sequencer; results repeat it. So a
hop is measurable where a process on one side of it can put its own clock next to `sequencedAt`:

| Hop | Metric | Measured by | `clock` |
|---|---|---|---|
| gateway hand-off → sequenced | `venue_gateway_hop_latency_seconds{hop="handoff_to_sequenced"}` | the gateway: `sequencedAt` minus the wall clock when the gateway queued the input for the ingress | `software` |
| sequenced → reply at the gateway | `venue_gateway_hop_latency_seconds{hop="sequenced_to_reply"}` | the gateway: the wall clock when the node's reply was read, minus `sequencedAt` (journal append, engine, reply) | `software` |
| sequenced → feed at the gateway | `venue_gateway_hop_latency_seconds{hop="sequenced_to_feed"}` | the gateway's feed follower | `software` |
| sequenced → feed at a downstream reader | `venue_hop_latency_seconds{hop="sequenced_to_feed"}` | `venue-observer` | `software` |

Both clocks are the hosts' system clocks, so a hop between two hosts includes their offset (sub-millisecond under
chrony on PTP, blueprint §5; unbounded on a laptop). Not measurable without a timestamp the journal does not carry:

- **NIC receive → gateway** and **gateway → client**: needs hardware or socket timestamps at the edge (blueprint §14).
- **client receive at the gateway → hand-off**: needs the gateway to carry its receive time with the request; an edge
  change, not a journal one, left for later.
- **sequencer receive → sequenced** (the ingress queue): needs a receive timestamp in the record header.
- **sequenced → engine result** and **result → published**: need a result or publication timestamp written beside the
  result log (never by the engine, which has no clock, spec E-1.1).

Adding any of these is a schema change (interfaces/sbe) reviewed on its own, not part of this work.

### Operator endpoints

The observer also answers, on the same port, read-only JSON from its projection of the feed (never a query to the
engine): `GET /v1/markets` (every listed market with its state, event phase and best prices), `GET /v1/markets/{id}`
(one market's lifecycle and its book at the observer's sequence, `?depth=N`), `GET /v1/accounts/{id}` (an account's
sleeve, positions and open orders summary; the venue holds no personal data) and `GET /healthz`. Each answer names the
`sequence` it reflects.

## FIX adapter

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `venue_fix_build_info` | gauge | `version` | 1 |
| `venue_fix_sessions` | gauge | `state` | FIX sessions held: `active` (logged on, translating) |
| `venue_fix_logons_total` | counter | | Logons accepted |
| `venue_fix_logon_refusals_total` | counter | `reason` | Logons the adapter refused after asking the gateway: `credentials` (the gateway refused the token), `gateway_unavailable` (no answer, or 503), `rate_limit`, `invalid` (no token in Password 554), `other`. Logons Artio refuses itself (a wrong TargetCompID, a logon timeout) are not counted here; they show as Logout messages out |
| `venue_fix_messages_total` | counter | `direction`, `msg_type` | Every FIX message on Artio's streams (session-level ones, replays and refused logons included), `in` or `out`, by MsgType (35) name (`Logon`, `NewOrderSingle`, `ExecutionReport`, ... the dialect's 19 types; `other` for any other) |
| `venue_fix_rejects_total` | counter | `kind`, `reason` | Rejects the adapter sent: `kind` `session` (Reject 3), `business` (BusinessMessageReject j) or `order` (ExecutionReport rejected, OrderCancelReject 9, or a rejected OrderMassCancelReport); `reason` the FIX code of that kind as a number: SessionRejectReason (373), BusinessRejectReason (380), or OrdRejReason (103) / CxlRejReason (102) / MassCancelRejectReason (532) |
| `venue_fix_gateway_links` | gauge | `state` | Sessions' gateway WebSockets: `connected` or `connecting` |
| `venue_fix_gateway_disconnects_total` | counter | | Gateway WebSockets, once welcomed, that the gateway or the network ended rather than the adapter |
| `venue_fix_audit_queue_depth` | gauge | | Audit records queued and not yet written |
| `venue_fix_audit_records_total` | counter | | Audit records written |

The JVM's own metrics (`jvm_*`) are exported beside them.

## Logs

Every process writes one line per event to standard output. `VENUE_LOG_FORMAT=json` (the chart's setting) writes a
JSON object: `ts` (RFC 3339, UTC), `level` (`info`, `warn`, `error`), `service` (`venue-gateway`, `venue-node`,
`venue-observer`, `venue-fix`), `instance` (`VENUE_INSTANCE`, else the host name), `msg`, and fields of the event
(`half`, `peer`, `feed`, `session` and the like; never a token, secret or key). Unset or `text`, one readable line
for a terminal. The audit logs (interfaces/audit) are separate files and are not these logs.

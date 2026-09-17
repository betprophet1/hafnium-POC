package venue.fix;

import io.prometheus.metrics.core.datapoints.CounterDataPoint;
import io.prometheus.metrics.core.metrics.Counter;
import io.prometheus.metrics.core.metrics.CounterWithCallback;
import io.prometheus.metrics.core.metrics.Gauge;
import io.prometheus.metrics.core.metrics.GaugeWithCallback;
import io.prometheus.metrics.exporter.httpserver.HTTPServer;
import io.prometheus.metrics.instrumentation.jvm.JvmMetrics;
import io.prometheus.metrics.model.registry.PrometheusRegistry;

import java.io.IOException;
import java.io.UncheckedIOException;
import java.util.HashMap;
import java.util.Map;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.function.LongSupplier;

/**
 * The adapter's Prometheus metrics (deploy/observability/metrics.md, FIX adapter), in a registry of its own per
 * adapter, served on {@code --metrics-listen} when one is given.
 *
 * <p>Label values come only from fixed sets: MsgType names from the dialect, refusal reasons, FIX reject codes. Never
 * a CompID, account, order or credential. Every labelled series a hot path touches is resolved once, here, so
 * counting a message is an atomic increment with no allocation.
 */
final class Metrics implements AutoCloseable
{
    /** MsgType (35) to its name in the dialect (interfaces/fix/venue-fix44.xml). */
    private static final Map<String, String> MSG_TYPES = Map.ofEntries(
        Map.entry("0", "Heartbeat"), Map.entry("1", "TestRequest"), Map.entry("2", "ResendRequest"),
        Map.entry("3", "Reject"), Map.entry("4", "SequenceReset"), Map.entry("5", "Logout"), Map.entry("A", "Logon"),
        Map.entry("D", "NewOrderSingle"), Map.entry("G", "OrderCancelReplaceRequest"),
        Map.entry("F", "OrderCancelRequest"), Map.entry("q", "OrderMassCancelRequest"),
        Map.entry("H", "OrderStatusRequest"), Map.entry("AF", "OrderMassStatusRequest"),
        Map.entry("x", "SecurityListRequest"), Map.entry("8", "ExecutionReport"), Map.entry("9", "OrderCancelReject"),
        Map.entry("r", "OrderMassCancelReport"), Map.entry("y", "SecurityList"),
        Map.entry("j", "BusinessMessageReject"));

    /** Why a Logon was refused. */
    enum Refusal
    {
        CREDENTIALS("credentials"), GATEWAY_UNAVAILABLE("gateway_unavailable"), RATE_LIMIT("rate_limit"),
        INVALID("invalid"), OTHER("other");

        final String label;

        Refusal(final String label)
        {
            this.label = label;
        }
    }

    /** Which kind of reject a reject code belongs to. */
    enum RejectKind
    {
        SESSION("session"), BUSINESS("business"), ORDER("order");

        final String label;

        RejectKind(final String label)
        {
            this.label = label;
        }
    }

    final PrometheusRegistry registry = new PrometheusRegistry();
    private final Map<String, CounterDataPoint> messagesIn = new HashMap<>();
    private final Map<String, CounterDataPoint> messagesOut = new HashMap<>();
    private final CounterDataPoint otherIn;
    private final CounterDataPoint otherOut;
    private final Counter logons;
    private final Counter logonRefusals;
    private final Counter rejects;
    private final Counter gatewayDisconnects;
    final AtomicInteger activeSessions = new AtomicInteger();
    final AtomicInteger linksConnected = new AtomicInteger();
    final AtomicInteger linksConnecting = new AtomicInteger();
    private HTTPServer server;

    Metrics()
    {
        final String version = FixAdapter.class.getPackage().getImplementationVersion();
        Gauge.builder().name("venue_fix_build_info").help("1, labelled with the adapter's version")
            .labelNames("version").register(registry).labelValues(version != null ? version : "dev").set(1);
        final Counter messages = Counter.builder().name("venue_fix_messages_total")
            .help("FIX messages received (in) and sent (out), by MsgType (35) name")
            .labelNames("direction", "msg_type").register(registry);
        for (final String name : MSG_TYPES.values())
        {
            messagesIn.put(name, messages.labelValues("in", name));
            messagesOut.put(name, messages.labelValues("out", name));
        }
        otherIn = messages.labelValues("in", "other");
        otherOut = messages.labelValues("out", "other");
        logons = Counter.builder().name("venue_fix_logons_total").help("Logons accepted").register(registry);
        logonRefusals = Counter.builder().name("venue_fix_logon_refusals_total").help("Logons refused, by reason")
            .labelNames("reason").register(registry);
        for (final Refusal refusal : Refusal.values())
        {
            logonRefusals.labelValues(refusal.label);
        }
        rejects = Counter.builder().name("venue_fix_rejects_total")
            .help("Rejects sent: kind session (Reject 3, SessionRejectReason 373), business (BusinessMessageReject j, " +
                "BusinessRejectReason 380) or order (ExecutionReport rejected with OrdRejReason 103, " +
                "OrderCancelReject with CxlRejReason 102, OrderMassCancelReport with MassCancelRejectReason 532)")
            .labelNames("kind", "reason").register(registry);
        gatewayDisconnects = Counter.builder().name("venue_fix_gateway_disconnects_total")
            .help("Gateway WebSockets that ended under a live FIX session").register(registry);
        GaugeWithCallback.builder().name("venue_fix_sessions").help("FIX sessions held, by state")
            .labelNames("state").callback(cb -> cb.call(activeSessions.get(), "active")).register(registry);
        GaugeWithCallback.builder().name("venue_fix_gateway_links").help("Sessions' gateway WebSockets, by state")
            .labelNames("state").callback(cb ->
            {
                cb.call(linksConnected.get(), "connected");
                cb.call(linksConnecting.get(), "connecting");
            }).register(registry);
        JvmMetrics.builder().register(registry);
    }

    /** Registers the audit log's queue depth and records written, read at each scrape. */
    void audit(final AuditLog audit)
    {
        GaugeWithCallback.builder().name("venue_fix_audit_queue_depth").help("Audit records queued, not yet written")
            .callback(cb -> cb.call(audit.queueDepth())).register(registry);
        final LongSupplier written = audit::recordsWritten;
        CounterWithCallback.builder().name("venue_fix_audit_records_total").help("Audit records written")
            .callback(cb -> cb.call(written.getAsLong())).register(registry);
    }

    /** Serves {@code /metrics} on {@code host:port}. */
    void serve(final String listen)
    {
        final int colon = listen.lastIndexOf(':');
        if (colon < 0)
        {
            throw new IllegalArgumentException("--metrics-listen must be host:port, not " + listen);
        }
        try
        {
            server = HTTPServer.builder().hostname(listen.substring(0, colon))
                .port(Integer.parseInt(listen.substring(colon + 1))).registry(registry).buildAndStart();
        }
        catch (final IOException e)
        {
            throw new UncheckedIOException("metrics on " + listen, e);
        }
    }

    /** Counts one message seen on Artio's streams; {@code msgType} is the raw value of tag 35. */
    void message(final boolean inbound, final String msgType)
    {
        final String name = msgType != null ? MSG_TYPES.get(msgType) : null;
        final CounterDataPoint point = name == null ? null : (inbound ? messagesIn : messagesOut).get(name);
        if (point != null)
        {
            point.inc();
        }
        else
        {
            (inbound ? otherIn : otherOut).inc();
        }
    }

    void logon()
    {
        logons.inc();
    }

    void logonRefused(final Refusal refusal)
    {
        logonRefusals.labelValues(refusal.label).inc();
    }

    /** Counts a reject sent; {@code code} is the FIX reason code of its kind. */
    void reject(final RejectKind kind, final int code)
    {
        rejects.labelValues(kind.label, Integer.toString(code)).inc();
    }

    void gatewayDisconnected()
    {
        gatewayDisconnects.inc();
    }

    @Override
    public void close()
    {
        if (server != null)
        {
            server.close();
        }
    }
}

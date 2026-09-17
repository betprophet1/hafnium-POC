package venue.fix;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import io.aeron.logbuffer.ControlledFragmentHandler.Action;
import org.agrona.DirectBuffer;
import uk.co.real_logic.artio.Pressure;
import uk.co.real_logic.artio.decoder.SessionHeaderDecoder;
import uk.co.real_logic.artio.fields.DecimalFloat;
import uk.co.real_logic.artio.fields.UtcTimestampDecoder;
import uk.co.real_logic.artio.fields.UtcTimestampEncoder;
import uk.co.real_logic.artio.library.OnMessageInfo;
import uk.co.real_logic.artio.library.SessionHandler;
import uk.co.real_logic.artio.messages.DisconnectReason;
import uk.co.real_logic.artio.session.Session;
import uk.co.real_logic.artio.util.MutableAsciiBuffer;
import venue.fix.codecs.builder.BusinessMessageRejectEncoder;
import venue.fix.codecs.builder.ExecutionReportEncoder;
import venue.fix.codecs.builder.OrderCancelRejectEncoder;
import venue.fix.codecs.builder.OrderMassCancelReportEncoder;
import venue.fix.codecs.builder.RejectEncoder;
import venue.fix.codecs.builder.SecurityListEncoder;
import venue.fix.codecs.decoder.NewOrderSingleDecoder;
import venue.fix.codecs.decoder.OrderCancelReplaceRequestDecoder;
import venue.fix.codecs.decoder.OrderCancelRequestDecoder;
import venue.fix.codecs.decoder.OrderMassCancelRequestDecoder;
import venue.fix.codecs.decoder.SecurityListRequestDecoder;

import java.math.BigDecimal;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.time.Instant;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.Iterator;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.CompletableFuture;

/**
 * One FIX session and the gateway session behind it: FIX requests in, gateway JSON out, and the gateway's replies
 * and executions back as FIX.
 *
 * <p>Everything here runs on the adapter's polling thread: Artio calls {@link #onMessage} from inside
 * {@code FixLibrary.poll}, and the adapter calls {@link #doWork} on the same thread for what the gateway sent.
 *
 * <h2>Where execution reports come from</h2>
 * The gateway tells a session about an order twice: the reply to the request that caused a change lists the
 * account's executions from it, and the account's execution stream carries every execution again, from any session.
 * The stream and the replies travel different paths inside the gateway, so either can arrive first. The adapter
 * reports each execution once, the first time it sees it (keyed by sequence, type, order and trade), and holds a
 * streamed execution back while a request of this session is outstanding whose reply could come from an earlier
 * sequence. That keeps each order's reports in venue order: New before the fills of a crossing order, Replaced
 * before the fills an amendment causes. Executions of orders this session did not place (the account's other
 * sessions, the web page) are not reported; that is what a drop copy is for (interface 5).
 */
final class VenueSession implements SessionHandler
{
    /** How many execution keys are remembered for deduplication; far more than can be in flight at once. */
    private static final int SEEN_CAPACITY = 20_000;

    @FunctionalInterface
    private interface Outgoing
    {
        /** Encodes and sends one message; the result is Artio's position, negative when not sent. */
        long sendOn(Session session);
    }

    private enum Kind { NEW, REPLACE, CANCEL, MASS_CANCEL }

    /** A request sent to the gateway and not yet answered, with what is needed to answer the client in FIX. */
    private static final class Request
    {
        final Kind kind;
        final String msgType;
        final int refSeqNum;
        final String clOrdId;
        String origClOrdId;
        OrderState order;
        // A new order's fields, for its report if it never reaches the book.
        String symbol;
        char side;
        long orderQty;
        String price;
        char timeInForce;
        String execInst;
        // A mass cancel's type (530) and scope.
        char massCancelType;
        String massCancelScope;
        /** Whether an execution report already answered it. */
        boolean reported;
        /** The venue did not answer in time: the order may exist, and the firm may resend under the same ClOrdID. */
        boolean outcomeUnknown;

        Request(final Kind kind, final String msgType, final int refSeqNum, final String clOrdId)
        {
            this.kind = kind;
            this.msgType = msgType;
            this.refSeqNum = refSeqNum;
            this.clOrdId = clOrdId;
        }
    }

    /** One execution report, filled in field by field and encoded when it is sent. */
    private static final class Report
    {
        String orderId = "NONE";
        String clOrdId;
        String origClOrdId;
        String execId;
        char execType;
        char ordStatus;
        String symbol;
        char side;
        long orderQty;
        String price;
        char timeInForce;
        String execInst;
        long lastQty;
        String lastPx;
        int liquidity;
        long leavesQty;
        long cumQty;
        BigDecimal avgPx = BigDecimal.ZERO;
        int ordRejReason = -1;
        boolean restated;
        String text;
    }

    /** Markets per SecurityList message: a fragment stays well inside the encoder's buffer with long titles. */
    private static final int SECURITY_LIST_FRAGMENT = 40;

    /** Set by {@link #fail}: the session only logs out from then on. */
    private boolean failed;

    private final Session session;
    private final GatewayLink link;
    private final HttpClient http;
    private final String gateway;
    private final String accountId;
    private final long heartbeatNanos;

    private final MutableAsciiBuffer ascii = new MutableAsciiBuffer();
    private final NewOrderSingleDecoder newOrder = new NewOrderSingleDecoder();
    private final OrderCancelReplaceRequestDecoder replace = new OrderCancelReplaceRequestDecoder();
    private final OrderCancelRequestDecoder cancel = new OrderCancelRequestDecoder();
    private final OrderMassCancelRequestDecoder massCancel = new OrderMassCancelRequestDecoder();
    private final SecurityListRequestDecoder securityListRequest = new SecurityListRequestDecoder();
    private final UtcTimestampDecoder timestampDecoder = new UtcTimestampDecoder(true);

    private final ExecutionReportEncoder executionReport = new ExecutionReportEncoder();
    private final OrderCancelRejectEncoder cancelReject = new OrderCancelRejectEncoder();
    private final OrderMassCancelReportEncoder massCancelReport = new OrderMassCancelReportEncoder();
    private final BusinessMessageRejectEncoder businessReject = new BusinessMessageRejectEncoder();
    private final RejectEncoder sessionReject = new RejectEncoder();
    private final SecurityListEncoder securityList = new SecurityListEncoder();
    private final UtcTimestampEncoder transactTime = new UtcTimestampEncoder();
    private final DecimalFloat decimal = new DecimalFloat();

    private final ArrayDeque<Outgoing> outbound = new ArrayDeque<>();
    private final Map<String, Request> inFlight = new HashMap<>();
    /** New orders sent and not yet answered, by ClOrdID. */
    private final Map<String, Request> pendingNew = new HashMap<>();
    private final Map<String, OrderState> ordersById = new HashMap<>();
    /** Every ClOrdID an order has answered to, so OrigClOrdID finds it after a replace too. */
    private final Map<String, OrderState> ordersByClOrdId = new HashMap<>();
    /** Streamed executions not yet safe to report, in arrival (and so sequence) order. */
    private final ArrayDeque<JsonNode> held = new ArrayDeque<>();
    private final Set<String> seen = new LinkedHashSet<>();
    private final List<SecurityListFetch> securityListFetches = new ArrayList<>();

    private long lastReplySequence;
    private long nextRequestId;
    private long nextLocalExecId;
    private boolean ended;
    private boolean logoutRequested;

    private record SecurityListFetch(
        String reqId, int requestType, String symbol, CompletableFuture<HttpResponse<String>> response)
    {
    }

    /**
     * @param link the gateway session opened when the logon was authenticated, or null if it was lost before the
     *             FIX session reached the library; the session is then logged out at once.
     */
    VenueSession(final Session session, final GatewayLink link, final HttpClient http, final String gateway)
    {
        this.session = session;
        this.link = link;
        this.http = http;
        this.gateway = gateway;
        final JsonNode welcome = link != null ? link.welcome() : null;
        this.accountId = welcome != null ? welcome.path("accountId").asText() : "";
        this.lastReplySequence = welcome != null ? welcome.path("sequence").asLong() : 0;
        this.heartbeatNanos = session.heartbeatIntervalInMs() * 1_000_000L;
    }

    boolean isEnded()
    {
        return ended;
    }

    /**
     * Something failed while translating for this session. The session ends, as if its gateway session had closed, so
     * the fault stays with the firm that met it instead of stopping every session on the adapter.
     */
    void fail(final RuntimeException e)
    {
        System.err.println("fix adapter: ending " + session.compositeKey() + " after an error: " + e);
        failed = true;
        e.printStackTrace();
        if (link != null && !link.isClosed())
        {
            link.close("adapter error");
        }
    }

    // ---------------------------------------------------------------------------------------------------------------
    // FIX in

    @Override
    public Action onMessage(
        final DirectBuffer buffer,
        final int offset,
        final int length,
        final int libraryId,
        final Session session,
        final int sequenceIndex,
        final long messageType,
        final long timestampInNs,
        final long position,
        final OnMessageInfo messageInfo)
    {
        if (!messageInfo.isValid() || link == null)
        {
            return Action.CONTINUE;
        }
        relayHeartbeat();
        ascii.wrap(buffer, offset, length);
        if (messageType == NewOrderSingleDecoder.MESSAGE_TYPE)
        {
            newOrder.reset();
            newOrder.decode(ascii, 0, length);
            if (valid(newOrder.validate(), newOrder.header(), "D", newOrder.invalidTagId(), newOrder.rejectReason()))
            {
                onNewOrder();
            }
        }
        else if (messageType == OrderCancelReplaceRequestDecoder.MESSAGE_TYPE)
        {
            replace.reset();
            replace.decode(ascii, 0, length);
            if (valid(replace.validate(), replace.header(), "G", replace.invalidTagId(), replace.rejectReason()))
            {
                onReplace();
            }
        }
        else if (messageType == OrderCancelRequestDecoder.MESSAGE_TYPE)
        {
            cancel.reset();
            cancel.decode(ascii, 0, length);
            if (valid(cancel.validate(), cancel.header(), "F", cancel.invalidTagId(), cancel.rejectReason()))
            {
                onCancel();
            }
        }
        else if (messageType == OrderMassCancelRequestDecoder.MESSAGE_TYPE)
        {
            massCancel.reset();
            massCancel.decode(ascii, 0, length);
            if (valid(massCancel.validate(), massCancel.header(), "q", massCancel.invalidTagId(),
                massCancel.rejectReason()))
            {
                onMassCancel();
            }
        }
        else if (messageType == SecurityListRequestDecoder.MESSAGE_TYPE)
        {
            securityListRequest.reset();
            securityListRequest.decode(ascii, 0, length);
            if (valid(securityListRequest.validate(), securityListRequest.header(), "x",
                securityListRequest.invalidTagId(), securityListRequest.rejectReason()))
            {
                onSecurityListRequest();
            }
        }
        else if (!isSessionMessage(buffer, offset, length))
        {
            // An application message the dialect does not have. Artio has already checked the header.
            final int refSeqNum = session.lastReceivedMsgSeqNum();
            final String type = msgTypeOf(buffer, offset, length);
            send(s ->
            {
                businessReject.reset();
                businessReject.refSeqNum(refSeqNum).refMsgType(type).businessRejectReason(3)
                    .text("unsupported message type " + type);
                return s.trySend(businessReject);
            });
        }
        return Action.CONTINUE;
    }

    /**
     * Whether a decoded message may be acted on. One that is malformed (a required tag missing, a tag in the wrong
     * place) is answered with a session-level Reject (3) naming the tag, as FIX prescribes, and goes no further.
     */
    private boolean valid(
        final boolean valid, final SessionHeaderDecoder header, final String msgType, final int tag, final int reason)
    {
        if (valid)
        {
            return true;
        }
        final int refSeqNum = header.msgSeqNum();
        send(s ->
        {
            sessionReject.reset();
            sessionReject.refSeqNum(refSeqNum).refMsgType(msgType).sessionRejectReason(reason);
            if (tag > 0)
            {
                sessionReject.refTagID(tag);
            }
            return s.trySend(sessionReject);
        });
        return false;
    }

    private void onNewOrder()
    {
        final Request request = new Request(Kind.NEW, "D", newOrder.header().msgSeqNum(), newOrder.clOrdIDAsString());
        request.symbol = newOrder.symbolAsString();
        request.side = newOrder.side();
        request.orderQty = Translate.contracts(newOrder.orderQty());
        request.price = Translate.cents(newOrder.price());
        request.timeInForce = newOrder.hasTimeInForce() ? newOrder.timeInForce() : '1';
        request.execInst = newOrder.hasExecInst() ? newOrder.execInstAsString() : null;

        // What FIX can say and the venue cannot is refused here, as a rejected order, before it reaches the gateway.
        final String problem = orderProblem(request);
        if (problem != null)
        {
            rejectNew(request, Translate.OTHER, problem);
            return;
        }
        final Request earlier = pendingNew.get(request.clOrdId);
        if (earlier != null && !earlier.outcomeUnknown)
        {
            rejectNew(request, Translate.DUPLICATE_ORDER, "an order with this ClOrdID is already pending");
            return;
        }

        final ObjectNode order = GatewayLink.JSON.createObjectNode()
            .put("op", "order")
            .put("clientOrderId", request.clOrdId)
            .put("instrumentId", request.symbol)
            .put("action", request.side == '1' ? "buy" : "sell")
            // The dialect is in YES terms throughout, like the venue's books and executions (README, sides).
            .put("outcome", "yes")
            .put("quantity", request.orderQty)
            .put("timeInForce", switch (request.timeInForce)
            {
                case '3' -> "IOC";
                case '4' -> "FOK";
                case '6' -> "GTD";
                default -> "GTC";
            });
        order.putObject("price").put("cents", request.price);
        if (request.timeInForce == '6')
        {
            final long millis = timestampDecoder.decode(newOrder.expireTime(), newOrder.expireTimeLength());
            order.put("expiresAt", Instant.ofEpochMilli(millis).toString());
        }
        if (request.execInst != null)
        {
            order.put("postOnly", request.execInst.contains("6"));
            order.put("reduceOnly", request.execInst.contains("E"));
        }
        pendingNew.put(request.clOrdId, request);
        submit(request, order);
    }

    /** Why a NewOrderSingle cannot be given to the venue as it stands, or null when it can. */
    private String orderProblem(final Request request)
    {
        if (request.side != '1' && request.side != '2')
        {
            return "Side (54) must be 1 (buy YES) or 2 (sell YES)";
        }
        if (newOrder.ordType() != '2')
        {
            return "OrdType (40) must be 2 (limit)";
        }
        if (request.orderQty < 0)
        {
            return "OrderQty (38) must be a whole number of contracts";
        }
        if (Translate.decimal(newOrder.price()).signum() <= 0)
        {
            return "Price (44) must be a positive price in cents";
        }
        if ("1346".indexOf(request.timeInForce) < 0)
        {
            return "TimeInForce (59) must be 1 (GTC), 3 (IOC), 4 (FOK) or 6 (GTD)";
        }
        if ((request.timeInForce == '6') != newOrder.hasExpireTime())
        {
            return "ExpireTime (126) is required with, and only with, TimeInForce 6 (GTD)";
        }
        if (request.execInst != null)
        {
            for (final String instruction : request.execInst.split(" "))
            {
                if (!instruction.equals("6") && !instruction.equals("E"))
                {
                    return "ExecInst (18) may hold only 6 (post only) and E (reduce only)";
                }
            }
        }
        if (newOrder.hasSecurityID() && !newOrder.securityIDAsString().equals(request.symbol))
        {
            return "SecurityID (48), when given, must equal Symbol (55)";
        }
        if (newOrder.hasAccount() && !newOrder.accountAsString().equals(accountId))
        {
            return "Account (1), when given, must be the logged-on account " + accountId;
        }
        return null;
    }

    private void onReplace()
    {
        final Request request = new Request(Kind.REPLACE, "G", replace.header().msgSeqNum(), replace.clOrdIDAsString());
        request.origClOrdId = replace.origClOrdIDAsString();
        final OrderState order = findOrder(request, replace.hasOrderID() ? replace.orderIDAsString() : null);
        if (order == null)
        {
            return;
        }
        final long orderQty = Translate.contracts(replace.orderQty());
        final String problem;
        if (replace.side() != order.side || !replace.symbolAsString().equals(order.symbol))
        {
            problem = "Side (54) and Symbol (55) must be the order's";
        }
        else if (replace.ordType() != '2')
        {
            problem = "OrdType (40) must be 2 (limit)";
        }
        else if (orderQty < 0 || orderQty <= order.cumQty)
        {
            problem = "OrderQty (38) must be a whole number of contracts above CumQty " + order.cumQty;
        }
        else if (ordersByClOrdId.containsKey(request.clOrdId) || pendingNew.containsKey(request.clOrdId))
        {
            problem = "ClOrdID (11) is already in use";
        }
        else
        {
            problem = null;
        }
        if (problem != null)
        {
            cancelReject(request, order, Translate.OTHER, problem);
            return;
        }
        // FIX's OrderQty is the order's whole size, filled part included; the venue's is what should rest.
        final ObjectNode amend = GatewayLink.JSON.createObjectNode()
            .put("op", "amend")
            .put("orderId", order.orderId)
            .put("quantity", orderQty - order.cumQty);
        amend.putObject("price").put("cents", Translate.cents(replace.price()));
        order.pending = new OrderState.Pending(true, request.clOrdId, orderQty);
        request.order = order;
        submit(request, amend);
    }

    private void onCancel()
    {
        final Request request = new Request(Kind.CANCEL, "F", cancel.header().msgSeqNum(), cancel.clOrdIDAsString());
        request.origClOrdId = cancel.origClOrdIDAsString();
        final OrderState order = findOrder(request, cancel.hasOrderID() ? cancel.orderIDAsString() : null);
        if (order == null)
        {
            return;
        }
        order.pending = new OrderState.Pending(false, request.clOrdId, 0);
        request.order = order;
        submit(request, GatewayLink.JSON.createObjectNode().put("op", "cancel").put("orderId", order.orderId));
    }

    /**
     * The order a cancel or replace is for, by OrderID (37) when given and otherwise by OrigClOrdID (41). When there
     * is none it can act on, the client has already been sent an OrderCancelReject and the answer is null.
     */
    private OrderState findOrder(final Request request, final String orderId)
    {
        final OrderState order = orderId != null ? ordersById.get(orderId) : ordersByClOrdId.get(request.origClOrdId);
        if (order == null)
        {
            final boolean pending = orderId == null && pendingNew.containsKey(request.origClOrdId);
            cancelReject(request, null, pending ? Translate.ALREADY_PENDING : Translate.UNKNOWN_ORDER,
                pending ? "the order has not been acknowledged yet" : "no such order in this session");
            return null;
        }
        if (order.done)
        {
            cancelReject(request, order, Translate.TOO_LATE_TO_CANCEL, "the order is no longer live");
            return null;
        }
        if (order.pending != null)
        {
            cancelReject(request, order, Translate.ALREADY_PENDING, "a cancel or replace is already pending");
            return null;
        }
        return order;
    }

    private void onMassCancel()
    {
        final Request request =
            new Request(Kind.MASS_CANCEL, "q", massCancel.header().msgSeqNum(), massCancel.clOrdIDAsString());
        request.massCancelType = massCancel.massCancelRequestType();
        final ObjectNode cancelAll = GatewayLink.JSON.createObjectNode().put("op", "cancelAll");
        switch (request.massCancelType)
        {
            case '7' ->
            {
            }
            case '1' ->
            {
                request.massCancelScope = massCancel.symbolAsString();
                cancelAll.put("instrumentId", request.massCancelScope);
            }
            case '2' ->
            {
                // An event is the underlying of its markets: SecurityList gives each market's event in 311.
                if (!massCancel.hasUnderlyingSymbol())
                {
                    massCancelReport(request, '0', 99, 0, "UnderlyingSymbol (311) must name the event");
                    return;
                }
                request.massCancelScope = massCancel.underlyingSymbolAsString();
                cancelAll.put("eventId", request.massCancelScope);
            }
            default ->
            {
                massCancelReport(request, '0', 99, 0, "MassCancelRequestType (530) must be 1, 2 or 7");
                return;
            }
        }
        submit(request, cancelAll);
    }

    private void onSecurityListRequest()
    {
        final String reqId = securityListRequest.securityReqIDAsString();
        final int type = securityListRequest.securityListRequestType();
        final String symbol = securityListRequest.hasSymbol() ? securityListRequest.symbolAsString() : null;
        final HttpRequest get = HttpRequest.newBuilder(URI.create("http://" + gateway + "/v4/markets?resolved=false"))
            .timeout(Duration.ofSeconds(10))
            .GET()
            .build();
        securityListFetches.add(
            new SecurityListFetch(reqId, type, symbol, http.sendAsync(get, HttpResponse.BodyHandlers.ofString())));
    }

    private void submit(final Request request, final ObjectNode message)
    {
        final String id = "r" + nextRequestId++;
        message.put("id", id);
        inFlight.put(id, request);
        link.send(message);
    }

    /**
     * Keeps the gateway's heartbeat alive while the client's is. Any message the gateway receives counts, so a
     * heartbeat is only sent when nothing else has been for half the interval.
     */
    private void relayHeartbeat()
    {
        if (heartbeatNanos > 0 && System.nanoTime() - link.lastSentNanos() > heartbeatNanos / 2)
        {
            link.send(GatewayLink.JSON.createObjectNode().put("op", "heartbeat"));
        }
    }

    // ---------------------------------------------------------------------------------------------------------------
    // Gateway in

    /** Handles what the gateway has sent, sends what is waiting, and notices a gateway session that has ended. */
    int doWork()
    {
        if (ended)
        {
            return 0;
        }
        int work = 0;
        if (failed || link == null || link.isClosed())
        {
            if (!logoutRequested)
            {
                final long position = session.logoutAndDisconnect(
                    "venue gateway session closed".getBytes(StandardCharsets.US_ASCII));
                logoutRequested = !Pressure.isBackPressured(position);
                work++;
            }
            if (failed || link == null)
            {
                return work;
            }
        }
        for (JsonNode message = link.poll(); message != null; message = link.poll())
        {
            onGatewayMessage(message);
            work++;
        }
        work += releaseHeld();
        work += finishSecurityLists();
        work += flush();
        return work;
    }

    private void onGatewayMessage(final JsonNode message)
    {
        final String type = message.path("type").asText();
        switch (type)
        {
            case "reply", "error" ->
            {
                final Request request = inFlight.remove(message.path("id").asText());
                if (request == null)
                {
                    if ("error".equals(type))
                    {
                        System.err.println("fix adapter: gateway error for " + session.compositeKey() + ": " +
                            message.path("error"));
                    }
                    return;
                }
                if ("error".equals(type))
                {
                    onRefused(request, message.path("error"));
                }
                else
                {
                    onReply(request, message.path("result"));
                }
            }
            case "execution" -> held.add(message.path("execution"));
            // Account updates, payouts and releases have no place in an order session; resync means executions
            // may have been missed, which only a drop copy or GET /v4/account can make up.
            case "resync" -> System.err.println("fix adapter: gateway resync for " + session.compositeKey());
            default ->
            {
            }
        }
    }

    /** Reports the held executions that can no longer be overtaken by a reply to this session. */
    private int releaseHeld()
    {
        int released = 0;
        while (!held.isEmpty() && (inFlight.isEmpty() || sequence(held.peek()) <= lastReplySequence))
        {
            onExecution(held.poll());
            released++;
        }
        return released;
    }

    private void onReply(final Request request, final JsonNode result)
    {
        lastReplySequence = Math.max(lastReplySequence, result.path("sequence").asLong());
        final String status = result.path("status").asText();
        final JsonNode executions = result.path("executions");
        if (request.kind == Kind.MASS_CANCEL)
        {
            int cancelled = 0;
            for (final JsonNode execution : executions)
            {
                cancelled += "cancelled".equals(execution.path("type").asText()) ? 1 : 0;
            }
            // The report acknowledges the request; each cancelled order of this session then has its own report.
            massCancelReport(request, request.massCancelType, -1, cancelled, null);
        }
        for (final JsonNode execution : executions)
        {
            onExecution(execution);
        }
        final String reason = text(result, "rejectReason");
        switch (request.kind)
        {
            case NEW ->
            {
                pendingNew.remove(request.clOrdId);
                if (!request.reported)
                {
                    // Every engine outcome carries an execution; this only covers one that somehow did not.
                    final boolean duplicate = "duplicate".equals(status);
                    rejectNew(request, duplicate ? Translate.DUPLICATE_ORDER : Translate.ordRejReason(reason),
                        duplicate ? "duplicate_client_order_id" : reason);
                }
            }
            case REPLACE, CANCEL ->
            {
                final OrderState order = request.order;
                if (order.pending != null && order.pending.clOrdId().equals(request.clOrdId))
                {
                    order.pending = null;
                    if ("rejected".equals(status))
                    {
                        cancelReject(request, order, Translate.cxlRejReason(reason), reason);
                    }
                }
            }
            case MASS_CANCEL ->
            {
            }
        }
    }

    /** A request the gateway refused at its edge: nothing reached the venue. */
    private void onRefused(final Request request, final JsonNode error)
    {
        final String code = error.path("code").asText();
        final String message = error.path("message").asText();
        if ("UNAVAILABLE".equals(code))
        {
            // Not a refusal: the venue did not answer in time and the request may or may not have taken effect.
            // Rejecting it would be a guess, so say what is known and let the firm resend under the same ClOrdID.
            if (request.kind == Kind.NEW)
            {
                // Still pending: if the order did go in, its acceptance arrives on the stream and is reported, and
                // a resend under the same ClOrdID takes this one's place.
                request.outcomeUnknown = true;
            }
            if (request.order != null)
            {
                request.order.pending = null;
            }
            send(s ->
            {
                businessReject.reset();
                businessReject.refSeqNum(request.refSeqNum).refMsgType(request.msgType).businessRejectReason(4)
                    .businessRejectRefID(request.clOrdId)
                    .text("outcome unknown, the venue did not answer: resend with the same ClOrdID");
                return s.trySend(businessReject);
            });
            return;
        }
        switch (request.kind)
        {
            case NEW ->
            {
                pendingNew.remove(request.clOrdId);
                rejectNew(request, Translate.ordRejReasonForError(code), code + ": " + message);
            }
            case REPLACE, CANCEL ->
            {
                request.order.pending = null;
                cancelReject(request, request.order,
                    "NOT_FOUND".equals(code) ? Translate.UNKNOWN_ORDER : Translate.OTHER, code + ": " + message);
            }
            case MASS_CANCEL -> massCancelReport(request, '0', "NOT_FOUND".equals(code) ? 1 : 99, 0,
                code + ": " + message);
        }
    }

    /** One execution, from a reply or the stream: reported the first time it is seen, if the order is this session's. */
    private void onExecution(final JsonNode execution)
    {
        final String type = execution.path("type").asText();
        final String orderId = text(execution, "orderId");
        final String clientOrderId = text(execution, "clientOrderId");
        final String key = execution.path("sequence").asText() + '|' + type + '|' + orderId + '|' +
            text(execution, "tradeId") + '|' + clientOrderId;
        if (!remember(key))
        {
            return;
        }
        final OrderState order = orderId != null ? ordersById.get(orderId) : null;
        final String sequence = execution.path("sequence").asText();
        switch (type)
        {
            case "accepted" ->
            {
                final Request request = clientOrderId != null ? pendingNew.get(clientOrderId) : null;
                if (order != null || request == null || orderId == null)
                {
                    return;
                }
                request.reported = true;
                final OrderState accepted = new OrderState(orderId, request.clOrdId, request.symbol, request.side,
                    execution.path("quantity").asLong(), execution.path("price").path("cents").asText(),
                    request.timeInForce, request.execInst);
                ordersById.put(orderId, accepted);
                ordersByClOrdId.put(request.clOrdId, accepted);
                final Report report = reportOf(accepted, sequence + "-" + orderId + "-0", '0');
                report.ordStatus = '0';
                send(report);
            }
            case "rejected" ->
            {
                final Request request = clientOrderId != null ? pendingNew.get(clientOrderId) : null;
                if (request == null || request.reported)
                {
                    // An amend or cancel rejected by the engine is answered from its reply, as an OrderCancelReject.
                    return;
                }
                final String reason = text(execution, "reason");
                rejectNew(request, Translate.ordRejReason(reason), reason, sequence + "-R-" + request.clOrdId);
            }
            case "fill" ->
            {
                if (order == null)
                {
                    return;
                }
                final long quantity = execution.path("quantity").asLong();
                final String price = execution.path("price").path("cents").asText();
                order.fill(quantity, new BigDecimal(price), execution.path("remaining").asLong());
                final Report report = reportOf(order,
                    sequence + "-" + orderId + "-F-" + text(execution, "tradeId"), 'F');
                report.lastQty = quantity;
                report.lastPx = price;
                report.liquidity = "maker".equals(text(execution, "role")) ? 1 : 2;
                send(report);
            }
            case "cancelled" ->
            {
                if (order == null)
                {
                    return;
                }
                order.leavesQty = 0;
                order.done = true;
                final Report report = reportOf(order, sequence + "-" + orderId + "-4", '4');
                final OrderState.Pending pending = order.pending;
                if (pending != null && !pending.replace())
                {
                    // The answer to this session's OrderCancelRequest: reported under the request's ClOrdID.
                    report.clOrdId = pending.clOrdId();
                    report.origClOrdId = order.clOrdId;
                    order.pending = null;
                }
                report.text = text(execution, "reason");
                send(report);
            }
            case "amended" ->
            {
                if (order == null)
                {
                    return;
                }
                final OrderState.Pending pending = order.pending;
                String origClOrdId = null;
                if (pending != null && pending.replace())
                {
                    origClOrdId = order.clOrdId;
                    order.clOrdId = pending.clOrdId();
                    ordersByClOrdId.put(pending.clOrdId(), order);
                    order.pending = null;
                }
                order.leavesQty = execution.path("remaining").asLong();
                order.orderQty = order.cumQty + order.leavesQty;
                order.price = execution.path("price").path("cents").asText(order.price);
                final Report report = reportOf(order, sequence + "-" + orderId + "-5", '5');
                report.origClOrdId = origClOrdId;
                send(report);
            }
            case "reduced" ->
            {
                if (order == null)
                {
                    return;
                }
                order.leavesQty = execution.path("remaining").asLong();
                order.orderQty = order.cumQty + order.leavesQty;
                final Report report = reportOf(order, sequence + "-" + orderId + "-D", 'D');
                report.restated = true;
                report.text = "reduced by the venue";
                send(report);
            }
            default ->
            {
                // A kind of execution this dialect does not know yet; ignoring it keeps the session up.
            }
        }
    }

    private boolean remember(final String key)
    {
        if (!seen.add(key))
        {
            return false;
        }
        if (seen.size() > SEEN_CAPACITY)
        {
            final Iterator<String> oldest = seen.iterator();
            oldest.next();
            oldest.remove();
        }
        return true;
    }

    private int finishSecurityLists()
    {
        int finished = 0;
        for (final Iterator<SecurityListFetch> it = securityListFetches.iterator(); it.hasNext(); )
        {
            final SecurityListFetch fetch = it.next();
            if (!fetch.response().isDone())
            {
                continue;
            }
            it.remove();
            finished++;
            JsonNode markets = null;
            try
            {
                final HttpResponse<String> response = fetch.response().join();
                if (response.statusCode() == 200)
                {
                    markets = GatewayLink.JSON.readTree(response.body());
                }
            }
            catch (final Exception e)
            {
                System.err.println("fix adapter: GET /v4/markets failed: " + e);
            }
            sendSecurityList(fetch, markets);
        }
        return finished;
    }

    private void sendSecurityList(final SecurityListFetch fetch, final JsonNode markets)
    {
        record Listed(String symbol, String description, String event, boolean open, long lotStep)
        {
        }
        final List<Listed> listed = new ArrayList<>();
        if (markets != null)
        {
            for (final JsonNode event : markets.path("events"))
            {
                for (final JsonNode market : event.path("markets"))
                {
                    final String symbol = market.path("instrumentId").asText();
                    if (fetch.requestType() == 0 && !symbol.equals(fetch.symbol()))
                    {
                        continue;
                    }
                    listed.add(new Listed(symbol,
                        event.path("title").asText() + ": " + market.path("label").asText(),
                        event.path("eventId").asText(),
                        "open".equals(market.path("state").asText()),
                        market.path("lotStep").asLong(1)));
                }
            }
        }
        final int result = markets == null ? 5 : listed.isEmpty() ? 2 : 0;
        final String responseId = "SL" + nextLocalExecId++;
        // A venue whose calendar lists rounds all day has more markets than one message holds (Artio's encoder buffer
        // is 16 KiB), so the list goes out in fragments: TotNoRelatedSym is the whole count on every one, and
        // LastFragment says which is last.
        final int fragments = Math.max(1, (listed.size() + SECURITY_LIST_FRAGMENT - 1) / SECURITY_LIST_FRAGMENT);
        for (int f = 0; f < fragments; f++)
        {
            final List<Listed> part =
                listed.subList(f * SECURITY_LIST_FRAGMENT, Math.min(listed.size(), (f + 1) * SECURITY_LIST_FRAGMENT));
            final boolean last = f == fragments - 1;
            send(s ->
            {
                securityList.reset();
                securityList.securityReqID(fetch.reqId()).securityResponseID(responseId).securityRequestResult(result);
                if (!part.isEmpty())
                {
                    securityList.totNoRelatedSym(listed.size()).lastFragment(last);
                    var group = securityList.relatedSymGroup(part.size());
                    for (final Listed market : part)
                    {
                        group.instrument().symbol(market.symbol()).securityID(market.symbol()).securityIDSource("8")
                            .securityDesc(market.description()).securityStatus(market.open() ? "1" : "2");
                        group.underlyingSymbol(market.event()).roundLot(market.lotStep(), 0);
                        group = group.next();
                    }
                }
                return s.trySend(securityList);
            });
        }
    }

    // ---------------------------------------------------------------------------------------------------------------
    // FIX out

    private Report reportOf(final OrderState order, final String execId, final char execType)
    {
        final Report report = new Report();
        report.orderId = order.orderId;
        report.clOrdId = order.clOrdId;
        report.execId = execId;
        report.execType = execType;
        report.ordStatus = order.status();
        report.symbol = order.symbol;
        report.side = order.side;
        report.orderQty = order.orderQty;
        report.price = order.price;
        report.timeInForce = order.timeInForce;
        report.execInst = order.execInst;
        report.leavesQty = order.done ? 0 : order.leavesQty;
        report.cumQty = order.cumQty;
        report.avgPx = order.avgPx();
        return report;
    }

    private void rejectNew(final Request request, final int ordRejReason, final String text)
    {
        rejectNew(request, ordRejReason, text, "L-" + nextLocalExecId++);
    }

    private void rejectNew(final Request request, final int ordRejReason, final String text, final String execId)
    {
        request.reported = true;
        final Report report = new Report();
        report.clOrdId = request.clOrdId;
        report.execId = execId;
        report.execType = '8';
        report.ordStatus = '8';
        report.symbol = request.symbol;
        report.side = request.side;
        report.orderQty = Math.max(request.orderQty, 0);
        report.price = request.price;
        report.timeInForce = request.timeInForce;
        report.ordRejReason = ordRejReason;
        report.text = text;
        send(report);
    }

    private void send(final Report r)
    {
        send(s ->
        {
            final ExecutionReportEncoder er = executionReport;
            er.reset();
            er.orderID(r.orderId).execID(r.execId).execType(r.execType).ordStatus(r.ordStatus)
                .side(r.side).leavesQty(r.leavesQty, 0).cumQty(r.cumQty, 0).avgPx(fix(r.avgPx))
                .orderQty(r.orderQty, 0).ordType('2')
                .transactTime(transactTime.buffer(), transactTime.encode(System.currentTimeMillis()));
            er.instrument().symbol(r.symbol == null ? "" : r.symbol);
            if (r.clOrdId != null)
            {
                er.clOrdID(r.clOrdId);
            }
            if (r.origClOrdId != null)
            {
                er.origClOrdID(r.origClOrdId);
            }
            if (r.price != null && !r.price.isEmpty())
            {
                er.price(decimal.fromString(r.price));
            }
            if (r.timeInForce != 0)
            {
                er.timeInForce(r.timeInForce);
            }
            if (r.execInst != null)
            {
                er.execInst(r.execInst);
            }
            if (r.lastPx != null)
            {
                er.lastQty(r.lastQty, 0).lastPx(decimal.fromString(r.lastPx)).lastLiquidityInd(r.liquidity);
            }
            if (r.ordRejReason >= 0)
            {
                er.ordRejReason(r.ordRejReason);
            }
            if (r.restated)
            {
                er.execRestatementReason(99);
            }
            if (r.text != null)
            {
                er.text(r.text);
            }
            return s.trySend(er);
        });
    }

    private void cancelReject(final Request request, final OrderState order, final int reason, final String text)
    {
        final char responseTo = request.kind == Kind.CANCEL ? '1' : '2';
        final String orderId = order != null ? order.orderId : "NONE";
        final char status = order != null ? order.status() : '8';
        send(s ->
        {
            cancelReject.reset();
            cancelReject.orderID(orderId).clOrdID(request.clOrdId).origClOrdID(request.origClOrdId)
                .ordStatus(status).cxlRejResponseTo(responseTo).cxlRejReason(reason);
            if (text != null)
            {
                cancelReject.text(text);
            }
            return s.trySend(cancelReject);
        });
    }

    private void massCancelReport(
        final Request request, final char response, final int rejectReason, final int affected, final String text)
    {
        final String reportId = "MC" + nextLocalExecId++;
        send(s ->
        {
            massCancelReport.reset();
            massCancelReport.clOrdID(request.clOrdId).orderID(reportId)
                .massCancelRequestType(request.massCancelType).massCancelResponse(response)
                .totalAffectedOrders(affected)
                .transactTime(transactTime.buffer(), transactTime.encode(System.currentTimeMillis()));
            if (rejectReason >= 0)
            {
                massCancelReport.massCancelRejectReason(rejectReason);
            }
            if (request.massCancelScope != null)
            {
                if (request.massCancelType == '2')
                {
                    massCancelReport.underlyingSymbol(request.massCancelScope);
                }
                else
                {
                    massCancelReport.symbol(request.massCancelScope);
                }
            }
            if (text != null)
            {
                massCancelReport.text(text);
            }
            return s.trySend(massCancelReport);
        });
    }

    /**
     * Queues a message and sends what the session will take. Artio refuses a send it is back-pressured on rather than
     * blocking; the message then waits, in order, for the next pass.
     */
    private void send(final Outgoing message)
    {
        outbound.add(message);
        flush();
    }

    private int flush()
    {
        int sent = 0;
        while (!outbound.isEmpty())
        {
            final long position = outbound.peek().sendOn(session);
            if (position < 0 && Pressure.isBackPressured(position))
            {
                break;
            }
            if (position < 0)
            {
                // Not connected any more: nothing queued can be delivered, and the client resends on its next logon.
                outbound.clear();
                break;
            }
            outbound.poll();
            sent++;
        }
        return sent;
    }

    private static DecimalFloat fix(final BigDecimal value)
    {
        final BigDecimal nonNegativeScale = value.scale() < 0 ? value.setScale(0) : value;
        return new DecimalFloat(nonNegativeScale.unscaledValue().longValueExact(), nonNegativeScale.scale());
    }

    private static long sequence(final JsonNode execution)
    {
        return execution.path("sequence").asLong();
    }

    /** A string field, or null when it is absent or JSON null (asText would give "null"). */
    private static String text(final JsonNode node, final String field)
    {
        final JsonNode value = node.get(field);
        return value == null || value.isNull() ? null : value.asText();
    }

    private static boolean isSessionMessage(final DirectBuffer buffer, final int offset, final int length)
    {
        final String type = msgTypeOf(buffer, offset, length);
        return type.length() == 1 && "012345A".contains(type);
    }

    private static String msgTypeOf(final DirectBuffer buffer, final int offset, final int length)
    {
        final String raw = buffer.getStringWithoutLengthAscii(offset, Math.min(length, 64));
        final int start = raw.indexOf("35=");
        if (start < 0)
        {
            return "";
        }
        final int end = raw.indexOf('', start + 4);
        return end < 0 ? "" : raw.substring(start + 4, end);
    }

    // ---------------------------------------------------------------------------------------------------------------
    // Session lifecycle

    @Override
    public void onTimeout(final int libraryId, final Session session)
    {
    }

    @Override
    public void onSlowStatus(final int libraryId, final Session session, final boolean hasBecomeSlow)
    {
    }

    @Override
    public Action onDisconnect(final int libraryId, final Session session, final DisconnectReason reason)
    {
        // The FIX session is over, however it ended; ending the gateway session is what cancels its orders when it
        // logged in with cancelOnDisconnect (interfaces/gateway/README.md, sessions).
        ended = true;
        if (link != null)
        {
            link.close("fix session ended: " + reason);
        }
        return Action.CONTINUE;
    }

    @Override
    public void onSessionStart(final Session session)
    {
    }
}

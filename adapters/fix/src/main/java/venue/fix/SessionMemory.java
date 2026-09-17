package venue.fix;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ArrayNode;
import com.fasterxml.jackson.databind.node.ObjectNode;

import java.io.IOException;
import java.math.BigDecimal;
import java.net.URLEncoder;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.Iterator;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;

/**
 * What the adapter remembers of one firm's FIX session (its SenderCompID) beyond a single logon: the orders it knows,
 * the executions it has reported, the reports it could not deliver, and the gateway sequence up to which every
 * execution of the account has been dealt with. A logon hands this to its {@link VenueSession}, and logs in to the
 * gateway with {@code resumeFrom} that sequence, so that what happened while no gateway session was open (fills of
 * orders kept past a disconnect, the cancels that ended a session) is reported when the firm is back.
 *
 * <p>Kept in memory across logons, and in {@code <data>/sessions} across restarts of the adapter: written when a
 * session ends and when the adapter stops. A crash loses what changed since the last write; the next logon then resumes
 * from an earlier sequence and reports those executions again, under the same ExecIDs.
 *
 * <p>Everything but {@link #resumeFrom} is touched only by the polling thread; the logon, on the WebSocket client's
 * threads, reads only that.
 */
final class SessionMemory
{
    /** How many execution keys are remembered for deduplication; far more than can be in flight at once. */
    static final int SEEN_CAPACITY = 20_000;

    final String senderCompId;
    /** The account the memory is of; a logon to another account with the same SenderCompID starts afresh. */
    String accountId;
    /** Every execution of the account at or below this sequence has been reported or found not to concern the session. */
    volatile long resumeFrom;
    final Map<String, OrderState> ordersById = new HashMap<>();
    /** Every ClOrdID an order has answered to, so OrigClOrdID finds it after a replace too. */
    final Map<String, OrderState> ordersByClOrdId = new HashMap<>();
    final Set<String> seen = new LinkedHashSet<>();
    /** Execution reports made and not handed to the FIX session before it ended, oldest first. */
    final List<Report> undelivered = new ArrayList<>();

    SessionMemory(final String senderCompId)
    {
        this.senderCompId = senderCompId;
    }

    /** Forgets everything: the session now logs on to another account, whose sequence and orders are unrelated. */
    void reset(final String accountId)
    {
        this.accountId = accountId;
        resumeFrom = 0;
        ordersById.clear();
        ordersByClOrdId.clear();
        seen.clear();
        undelivered.clear();
    }

    /** Whether the key is new; remembering it, and forgetting the oldest beyond the capacity. */
    boolean remember(final String key)
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

    void know(final OrderState order, final String clOrdId)
    {
        ordersById.put(order.orderId, order);
        if (clOrdId != null)
        {
            ordersByClOrdId.putIfAbsent(clOrdId, order);
        }
    }

    // ---------------------------------------------------------------------------------------------------------------
    // On disk

    private ObjectNode toJson()
    {
        final ObjectNode json = GatewayLink.JSON.createObjectNode()
            .put("senderCompId", senderCompId).put("accountId", accountId).put("resumeFrom", resumeFrom);
        // Only live orders: a finished one can no longer be acted on, and its reports have been made.
        final ArrayNode orders = json.putArray("orders");
        for (final OrderState order : ordersById.values())
        {
            if (order.done)
            {
                continue;
            }
            final ObjectNode o = orders.addObject()
                .put("orderId", order.orderId).put("clOrdId", order.clOrdId).put("symbol", order.symbol)
                .put("side", String.valueOf(order.side))
                .put("timeInForce", order.timeInForce == 0 ? null : String.valueOf(order.timeInForce))
                .put("execInst", order.execInst).put("orderQty", order.orderQty).put("cumQty", order.cumQty)
                .put("leavesQty", order.leavesQty).put("price", order.price)
                .put("filledNotional", order.filledNotional.toPlainString())
                .put("baseSequence", order.baseSequence).put("stale", order.stale);
            final ArrayNode clOrdIds = o.putArray("clOrdIds");
            ordersByClOrdId.forEach((clOrdId, known) ->
            {
                if (known == order)
                {
                    clOrdIds.add(clOrdId);
                }
            });
        }
        // Only keys a resumed stream can bring again: everything at or below resumeFrom is never replayed.
        final ArrayNode keys = json.putArray("seen");
        for (final String key : seen)
        {
            final int bar = key.indexOf('|');
            if (bar > 0 && parseLong(key.substring(0, bar)) > resumeFrom)
            {
                keys.add(key);
            }
        }
        final ArrayNode reports = json.putArray("undelivered");
        undelivered.forEach(r -> reports.add(r.toJson()));
        return json;
    }

    private static SessionMemory fromJson(final JsonNode json)
    {
        final SessionMemory memory = new SessionMemory(json.path("senderCompId").asText());
        memory.accountId = Report.text(json, "accountId");
        memory.resumeFrom = json.path("resumeFrom").asLong();
        for (final JsonNode o : json.path("orders"))
        {
            final String tif = Report.text(o, "timeInForce");
            final OrderState order = new OrderState(o.path("orderId").asText(), Report.text(o, "clOrdId"),
                o.path("symbol").asText(), o.path("side").asText("1").charAt(0), o.path("orderQty").asLong(),
                Report.text(o, "price"), tif == null ? 0 : tif.charAt(0), Report.text(o, "execInst"));
            order.cumQty = o.path("cumQty").asLong();
            order.leavesQty = o.path("leavesQty").asLong();
            order.filledNotional = new BigDecimal(o.path("filledNotional").asText("0"));
            order.baseSequence = o.path("baseSequence").asLong();
            order.stale = o.path("stale").asBoolean();
            memory.ordersById.put(order.orderId, order);
            for (final JsonNode clOrdId : o.path("clOrdIds"))
            {
                memory.ordersByClOrdId.put(clOrdId.asText(), order);
            }
        }
        json.path("seen").forEach(key -> memory.seen.add(key.asText()));
        json.path("undelivered").forEach(r -> memory.undelivered.add(Report.fromJson(r)));
        return memory;
    }

    private static long parseLong(final String text)
    {
        try
        {
            return Long.parseLong(text);
        }
        catch (final NumberFormatException e)
        {
            return 0;
        }
    }

    /** Every session's memory, by SenderCompID, and where it is kept on disk. */
    static final class Store
    {
        private final Path dir;
        private final Map<String, SessionMemory> bySender = new ConcurrentHashMap<>();

        /** Reads whatever an earlier run of the adapter left in {@code dir}. */
        Store(final Path dir)
        {
            this.dir = dir;
            if (!Files.isDirectory(dir))
            {
                return;
            }
            try (var files = Files.list(dir))
            {
                for (final Path file : files.filter(f -> f.toString().endsWith(".json")).toList())
                {
                    try
                    {
                        final SessionMemory memory = fromJson(GatewayLink.JSON.readTree(file.toFile()));
                        bySender.put(memory.senderCompId, memory);
                    }
                    catch (final IOException | RuntimeException e)
                    {
                        // A memory that cannot be read costs the firm only the replay: it can still ask for status.
                        Log.warn("ignoring unreadable session memory", "file", file, "error", e);
                    }
                }
            }
            catch (final IOException e)
            {
                throw new IllegalStateException("cannot read session memory in " + dir, e);
            }
        }

        /** The gateway sequence a logon with this SenderCompID resumes from, or 0 for none. */
        long resumeFrom(final String senderCompId)
        {
            final SessionMemory memory = bySender.get(senderCompId);
            return memory == null ? 0 : memory.resumeFrom;
        }

        SessionMemory of(final String senderCompId)
        {
            return bySender.computeIfAbsent(senderCompId, SessionMemory::new);
        }

        /** Writes one session's memory, replacing the last write whole, so a crash never leaves half a file. */
        void save(final SessionMemory memory)
        {
            try
            {
                Files.createDirectories(dir);
                final String name = URLEncoder.encode(memory.senderCompId, StandardCharsets.UTF_8);
                final Path file = dir.resolve(name + ".json");
                final Path temporary = dir.resolve(name + ".json.tmp");
                GatewayLink.JSON.writeValue(temporary.toFile(), memory.toJson());
                Files.move(temporary, file, StandardCopyOption.REPLACE_EXISTING, StandardCopyOption.ATOMIC_MOVE);
            }
            catch (final IOException e)
            {
                Log.error("could not save a session's memory", "sender", memory.senderCompId, "error", e);
            }
        }
    }
}

package venue.fix;

import com.fasterxml.jackson.databind.JsonNode;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.MethodOrderer;
import org.junit.jupiter.api.Order;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.TestMethodOrder;
import uk.co.real_logic.artio.builder.Encoder;
import uk.co.real_logic.artio.fields.DecimalFloat;
import uk.co.real_logic.artio.fields.UtcTimestampEncoder;
import uk.co.real_logic.artio.session.Session;
import venue.fix.codecs.builder.NewOrderSingleEncoder;
import venue.fix.codecs.builder.OrderCancelReplaceRequestEncoder;
import venue.fix.codecs.builder.OrderCancelRequestEncoder;
import venue.fix.codecs.builder.OrderMassCancelRequestEncoder;
import venue.fix.codecs.builder.ResendRequestEncoder;
import venue.fix.codecs.builder.SecurityListRequestEncoder;

import java.io.IOException;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.List;
import java.util.Set;
import java.util.concurrent.TimeUnit;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assumptions.assumeTrue;

/**
 * The adapter end to end: a real node and gateway (scripts/venue.sh), the adapter in this JVM, and Artio initiators
 * as the firms. What a test sees is what the engine decided, translated twice.
 *
 * <p>The tests run in order and share the venue: each leaves the books as the next expects them.
 */
@TestMethodOrder(MethodOrderer.OrderAnnotation.class)
final class FixAdapterTest
{
    /** Ports: the venue takes 17430-17432 (node ingress, feed, gateway), the adapter 17440. */
    private static final int VENUE_PORTS_BASE = 17430;
    private static final int ADAPTER_PORT = 17440;
    private static final String GATEWAY = "127.0.0.1:" + (VENUE_PORTS_BASE + 2);
    /** Two binary markets the mini venue's catalogue lists and opens, and one categorical market. */
    private static final String RAIN = "100000100";
    private static final String BITCOIN = "300000100";
    private static final String ARSENAL = "200000100";

    private static final AdapterConfig ADAPTER = new AdapterConfig(
        "127.0.0.1", ADAPTER_PORT, "VENUE", GATEWAY, Path.of(System.getProperty("venue.scratch"), "adapter"), true,
        Set.of());

    private static Path root;
    private static Path scratch;
    private static FixAdapter adapter;
    private static FixClient client;
    private static Session maker;
    private static Session taker;

    private final UtcTimestampEncoder now = new UtcTimestampEncoder();

    @BeforeAll
    static void startVenue() throws Exception
    {
        root = Path.of(System.getProperty("venue.root"));
        scratch = Path.of(System.getProperty("venue.scratch"));
        final Path binaries = root.resolve("core/target/release");
        assumeTrue(Files.isExecutable(binaries.resolve("venue-gateway")) &&
            Files.isExecutable(binaries.resolve("venue-node")) && Files.isExecutable(binaries.resolve("venue-sim")),
            "venue binaries not built: run adapters/fix/test.sh, which builds them");
        run("start");
        // The adapter's session state from an earlier run would expect sequence numbers this run never sent.
        deleteRecursively(scratch.resolve("adapter"));
        adapter = FixAdapter.start(ADAPTER);
        client = new FixClient(scratch.resolve("client"));
    }

    @AfterAll
    static void stopVenue() throws Exception
    {
        if (client != null)
        {
            client.close();
        }
        if (adapter != null)
        {
            adapter.close();
        }
        if (root != null)
        {
            run("stop");
        }
    }

    private static void run(final String action) throws IOException, InterruptedException
    {
        final ProcessBuilder builder = new ProcessBuilder(
            root.resolve("adapters/fix/scripts/venue.sh").toString(), action, scratch.resolve("venue").toString())
            .inheritIO();
        builder.environment().put("VENUE_FIX_PORTS_BASE", String.valueOf(VENUE_PORTS_BASE));
        final Process process = builder.start();
        assertTrue(process.waitFor(120, TimeUnit.SECONDS), "venue.sh " + action + " finished");
        assertEquals(0, process.exitValue(), "venue.sh " + action);
    }

    // -------------------------------------------------------------------------------------------------------------

    @Test
    @Order(1)
    void aLogonWithATokenTheGatewayRefusesIsRejected()
    {
        assertNull(client.logon("BADTOKEN", "not-a-token", ADAPTER_PORT, false), "a bad token is refused");
        assertNull(client.logon("NOTOKEN", null, ADAPTER_PORT, false), "a logon without a token is refused");
    }

    @Test
    @Order(2)
    void firmsLogOnWithTheirDevTokens()
    {
        maker = client.logon("MAKER1", "dev-market-maker", ADAPTER_PORT, true);
        taker = client.logon("TAKER1", "dev-retail-customer", ADAPTER_PORT, false);
        assertNotNull(maker, "the market maker logs on");
        assertNotNull(taker, "the retail customer logs on");
    }

    @Test
    @Order(3)
    void theSecurityListNamesEveryMarketByInstrumentId()
    {
        final SecurityListRequestEncoder request = new SecurityListRequestEncoder()
            .securityReqID("markets").securityListRequestType(4);
        client.send(maker, request);
        final FixClient.Message list = client.expect(maker, "a security list", m -> "y".equals(m.type()));
        assertEquals("0", list.get(560), list.toString());
        assertTrue(list.all(55).containsAll(List.of(RAIN, BITCOIN, ARSENAL)), list.toString());
        assertEquals("1000001", list.get(311), "the event is the underlying: " + list);
    }

    @Test
    @Order(4)
    void anOrderIsPlacedReplacedAndCancelled()
    {
        client.send(maker, order("m1", RAIN, '1', 10, "40"));
        final FixClient.Message placed = report(maker, "m1", '0');
        assertEquals("0", placed.get(39));
        assertEquals("10", placed.get(151));
        assertEquals("0", placed.get(14));
        final String orderId = placed.get(37);

        final OrderCancelReplaceRequestEncoder replace = new OrderCancelReplaceRequestEncoder()
            .clOrdID("m1-r").origClOrdID("m1").side('1').ordType('2')
            .orderQty(new DecimalFloat(15)).price(new DecimalFloat(41))
            .transactTime(now.buffer(), now.encode(System.currentTimeMillis()));
        replace.instrument().symbol(RAIN);
        client.send(maker, replace);
        final FixClient.Message replaced = report(maker, "m1-r", '5');
        assertEquals("m1", replaced.get(41));
        assertEquals(orderId, replaced.get(37));
        assertEquals("15", replaced.get(38));
        assertEquals("15", replaced.get(151));
        assertEquals("41", replaced.get(44));

        client.send(maker, cancel("m1-c", "m1-r", RAIN, '1'));
        final FixClient.Message cancelled = report(maker, "m1-c", '4');
        assertEquals("4", cancelled.get(39));
        assertEquals("m1-r", cancelled.get(41));
        assertEquals("0", cancelled.get(151));

        client.send(maker, cancel("m1-c2", "m1-r", RAIN, '1'));
        final FixClient.Message tooLate = client.expect(maker, "a cancel reject", m -> "9".equals(m.type()));
        assertEquals("0", tooLate.get(102), "too late to cancel: " + tooLate);
        assertEquals("4", tooLate.get(39));

        client.send(maker, cancel("m1-c3", "no-such-order", RAIN, '1'));
        final FixClient.Message unknown = client.expect(maker, "a cancel reject", m -> "9".equals(m.type()));
        assertEquals("1", unknown.get(102), "unknown order: " + unknown);
    }

    @Test
    @Order(5)
    void whatTheVenueRefusesIsRejectedWithItsReason()
    {
        client.send(maker, order("m2", RAIN, '1', 5, "40.5"));
        final FixClient.Message offLadder = report(maker, "m2", '8');
        assertEquals("8", offLadder.get(39));
        assertTrue(offLadder.get(58).contains("PRICE_NOT_ON_LADDER"), offLadder.toString());

        client.send(maker, order("m1", RAIN, '1', 5, "40"));
        final FixClient.Message duplicate = report(maker, "m1", '8');
        assertEquals("6", duplicate.get(103), "a ClOrdID is remembered by the venue: " + duplicate);

        // A value the dialect's dictionary does not have is malformed FIX, refused at the session layer by tag.
        final NewOrderSingleEncoder market = order("m3", RAIN, '1', 5, "40");
        market.ordType('1');
        client.send(maker, market);
        final FixClient.Message unsupported = client.expect(maker, "a session reject", m -> "3".equals(m.type()));
        assertEquals("40", unsupported.get(371), unsupported.toString());
        assertEquals("5", unsupported.get(373), unsupported.toString());

        // A value the dictionary has and the venue cannot use is a rejected order.
        final NewOrderSingleEncoder expiring = order("m3", RAIN, '1', 5, "40");
        expiring.timeInForce('6');
        client.send(maker, expiring);
        final FixClient.Message noExpiry = report(maker, "m3", '8');
        assertTrue(noExpiry.get(58).contains("ExpireTime"), noExpiry.toString());
    }

    @Test
    @Order(6)
    void massCancelsReportEachOrderAndTheCount()
    {
        client.send(maker, order("m4", RAIN, '1', 5, "30"));
        report(maker, "m4", '0');
        client.send(maker, order("m5", ARSENAL, '1', 5, "20"));
        report(maker, "m5", '0');

        client.send(maker, massCancel("mc1", '1', RAIN));
        final FixClient.Message oneMarket = client.expect(maker, "a mass cancel report", m -> "r".equals(m.type()));
        assertEquals("1", oneMarket.get(531), oneMarket.toString());
        assertEquals("1", oneMarket.get(533), oneMarket.toString());
        report(maker, "m4", '4');

        client.send(maker, massCancel("mc2", '7', null));
        final FixClient.Message all = client.expect(maker, "a mass cancel report", m -> "r".equals(m.type()));
        assertEquals("7", all.get(531), all.toString());
        assertEquals("1", all.get(533), all.toString());
        report(maker, "m5", '4');
    }

    @Test
    @Order(7)
    void aFillBetweenTwoSessionsReachesBoth()
    {
        client.send(maker, order("m6", BITCOIN, '2', 10, "60"));
        final String makerOrder = report(maker, "m6", '0').get(37);

        client.send(taker, order("t1", BITCOIN, '1', 4, "60"));
        report(taker, "t1", '0');
        final FixClient.Message takerFill = report(taker, "t1", 'F');
        assertEquals("2", takerFill.get(39), "the taker's order filled: " + takerFill);
        assertEquals("4", takerFill.get(32));
        assertEquals("60", takerFill.get(31));
        assertEquals("2", takerFill.get(851), "the taker removed liquidity");

        final FixClient.Message makerFill = report(maker, "m6", 'F');
        assertEquals(makerOrder, makerFill.get(37));
        assertEquals("1", makerFill.get(39), "the maker's order is partly filled: " + makerFill);
        assertEquals("4", makerFill.get(32));
        assertEquals("6", makerFill.get(151));
        assertEquals("4", makerFill.get(14));
        assertEquals("1", makerFill.get(851), "the maker added liquidity");

        // Crossing at a better price fills at the resting order's price; IOC leaves nothing behind.
        final NewOrderSingleEncoder sweep = order("t2", BITCOIN, '1', 6, "65");
        sweep.timeInForce('3');
        client.send(taker, sweep);
        report(taker, "t2", '0');
        final FixClient.Message swept = report(taker, "t2", 'F');
        assertEquals("60", swept.get(31));
        assertEquals("2", swept.get(39));
        final FixClient.Message filled = report(maker, "m6", 'F');
        assertEquals("2", filled.get(39), "the maker's order filled: " + filled);
        assertEquals("10", filled.get(14));
        assertEquals("0", filled.get(151));
        assertEquals("60", filled.get(6));

        // Each execution is reported once, though the gateway sends it on the reply and on the stream.
        assertEquals(List.of(), client.drain(maker, Duration.ofMillis(500), m -> "8".equals(m.type())));
        assertEquals(List.of(), client.drain(taker, Duration.ofMillis(500), m -> "8".equals(m.type())));
    }

    @Test
    @Order(8)
    void endingASessionCancelsItsRestingOrders() throws Exception
    {
        client.send(maker, order("m7", BITCOIN, '1', 3, "10"));
        final String orderId = report(maker, "m7", '0').get(37);
        assertTrue(restingOrders("dev-market-maker").contains(orderId), "the order rests");

        maker.logoutAndDisconnect();
        client.pollUntil("the maker is disconnected", () -> client.isDisconnected(maker));
        client.pollUntil("the gateway cancels the session's orders", () ->
        {
            try
            {
                return !restingOrders("dev-market-maker").contains(orderId);
            }
            catch (final Exception e)
            {
                throw new AssertionError(e);
            }
        });

        taker.logoutAndDisconnect();
        client.pollUntil("the taker is disconnected", () -> client.isDisconnected(taker));
    }

    @Test
    @Order(9)
    void afterARestartASessionCarriesOnAndCanHaveEverythingResent()
    {
        adapter.close();
        adapter = FixAdapter.start(ADAPTER);

        // No ResetSeqNumFlag: the adapter remembers where the session's sequence numbers stood before the restart.
        maker = client.logon("MAKER1", "dev-market-maker", ADAPTER_PORT, true);
        assertNotNull(maker, "the market maker logs on again");

        // Everything from the first message on, answered from Artio's archive: application messages resent as
        // possible duplicates, session messages (logons, heartbeats) replaced by gap fills.
        client.send(maker, new ResendRequestEncoder().beginSeqNo(1).endSeqNo(0));
        final FixClient.Message resent = client.expect(maker, "the first order's report, resent",
            m -> "8".equals(m.type()) && "m1".equals(m.get(11)) && "0".equals(m.get(150)));
        assertEquals("Y", resent.get(43), "a resent message is flagged PossDupFlag: " + resent);
        client.expect(maker, "a gap fill", m -> "4".equals(m.type()) && "Y".equals(m.get(123)));

        maker.logoutAndDisconnect();
        client.pollUntil("the maker is disconnected", () -> client.isDisconnected(maker));
    }

    // -------------------------------------------------------------------------------------------------------------

    private static void deleteRecursively(final Path dir) throws IOException
    {
        if (Files.exists(dir))
        {
            try (var paths = Files.walk(dir))
            {
                for (final Path path : paths.sorted(java.util.Comparator.reverseOrder()).toList())
                {
                    Files.delete(path);
                }
            }
        }
    }

    /** The next execution report of this type for the ClOrdID. */
    private static FixClient.Message report(final Session session, final String clOrdId, final char execType)
    {
        return client.expect(session, "execution report " + execType + " for " + clOrdId,
            m -> "8".equals(m.type()) && clOrdId.equals(m.get(11)) && String.valueOf(execType).equals(m.get(150)));
    }

    private NewOrderSingleEncoder order(
        final String clOrdId, final String symbol, final char side, final long quantity, final String cents)
    {
        final NewOrderSingleEncoder order = new NewOrderSingleEncoder()
            .clOrdID(clOrdId).side(side).ordType('2').timeInForce('1')
            .orderQty(new DecimalFloat(quantity)).price(new DecimalFloat().fromString(cents))
            .transactTime(now.buffer(), now.encode(System.currentTimeMillis()));
        order.instrument().symbol(symbol);
        return order;
    }

    private Encoder cancel(final String clOrdId, final String origClOrdId, final String symbol, final char side)
    {
        final OrderCancelRequestEncoder cancel = new OrderCancelRequestEncoder()
            .clOrdID(clOrdId).origClOrdID(origClOrdId).side(side)
            .transactTime(now.buffer(), now.encode(System.currentTimeMillis()));
        cancel.instrument().symbol(symbol);
        return cancel;
    }

    private Encoder massCancel(final String clOrdId, final char type, final String symbol)
    {
        final OrderMassCancelRequestEncoder request = new OrderMassCancelRequestEncoder()
            .clOrdID(clOrdId).massCancelRequestType(type)
            .transactTime(now.buffer(), now.encode(System.currentTimeMillis()));
        if (symbol != null)
        {
            request.symbol(symbol);
        }
        return request;
    }

    /** The account's resting order IDs, from the gateway's REST half. */
    private static List<String> restingOrders(final String token) throws IOException, InterruptedException
    {
        final HttpResponse<String> response = HttpClient.newHttpClient().send(
            HttpRequest.newBuilder(URI.create("http://" + GATEWAY + "/v4/account"))
                .header("Authorization", "Bearer " + token).GET().build(),
            HttpResponse.BodyHandlers.ofString());
        final JsonNode account = GatewayLink.JSON.readTree(response.body());
        return account.path("orders").findValuesAsText("orderId");
    }
}

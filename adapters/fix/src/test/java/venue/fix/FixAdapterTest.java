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
import venue.fix.codecs.builder.OrderMassStatusRequestEncoder;
import venue.fix.codecs.builder.OrderStatusRequestEncoder;
import venue.fix.codecs.builder.ResendRequestEncoder;
import venue.fix.codecs.builder.SecurityListRequestEncoder;
import venue.fix.codecs.builder.TestRequestEncoder;

import java.io.IOException;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.stream.Collectors;
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
 * <p>The tests run in order and share the venue: each leaves the books as the next expects them. The venue includes an
 * identity service, so firms can log on with API keys as well as dev tokens.
 */
@TestMethodOrder(MethodOrderer.OrderAnnotation.class)
final class FixAdapterTest
{
    /**
     * Ports: the venue takes 17430-17434 (node ingress, feed, gateway, identity service, a second gateway), the adapter
     * 17440.
     */
    private static final int VENUE_PORTS_BASE = 17430;
    private static final int ADAPTER_PORT = 17440;
    private static final String GATEWAY = "127.0.0.1:" + (VENUE_PORTS_BASE + 2);
    /** Another gateway process on the same venue, serving the private half, which the adapter does not use. */
    private static final String SECOND_GATEWAY = "127.0.0.1:" + (VENUE_PORTS_BASE + 4);
    /** Two binary markets the mini venue's catalogue lists and opens, and one categorical market. */
    private static final String RAIN = "100000100";
    private static final String BITCOIN = "300000100";
    private static final String ARSENAL = "200000100";

    private static final Path ADAPTER_DATA = Path.of(System.getProperty("venue.scratch"), "adapter");
    /**
     * The audit log where it is by default, inside the data directory; unsynced, as a test can afford. KEEPER1's
     * orders outlive its sessions.
     */
    private static final AdapterConfig ADAPTER = new AdapterConfig(
        "127.0.0.1", ADAPTER_PORT, "VENUE", GATEWAY, ADAPTER_DATA, true, Set.of("KEEPER1"),
        ADAPTER_DATA.resolve("audit"), "test", false);

    private static Path root;
    private static Path scratch;
    private static FixAdapter adapter;
    private static FixClient client;
    private static Session maker;
    private static Session taker;
    /** The orders KEEPER1 placed, by ClOrdID, and the market maker's second session that takes them up. */
    private static final Map<String, String> kept = new java.util.HashMap<>();
    private static Session other;

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
    void aSessionThatComesBackIsSentTheFillsItMissed() throws Exception
    {
        Session keeper = client.logon("KEEPER1", "dev-market-maker", ADAPTER_PORT, true);
        assertNotNull(keeper, "the keeper logs on");
        client.send(keeper, order("k1", RAIN, '1', 5, "35"));
        kept.put("k1", report(keeper, "k1", '0').get(37));
        client.send(keeper, order("k2", ARSENAL, '1', 5, "20"));
        kept.put("k2", report(keeper, "k2", '0').get(37));
        client.send(keeper, order("k3", BITCOIN, '2', 5, "70"));
        kept.put("k3", report(keeper, "k3", '0').get(37));

        final Session first = keeper;
        first.logoutAndDisconnect();
        client.pollUntil("the keeper is disconnected", () -> client.isDisconnected(first));
        assertTrue(restingOrders("dev-market-maker").containsAll(kept.values()), "the keeper's orders outlive it");

        // While no gateway session is open for it, the retail customer sells into k1 over REST.
        final JsonNode sold = rest("dev-retail-customer", "POST", "/v4/orders",
            "{\"clientOrderId\": \"away1\", \"instrumentId\": \"" + RAIN + "\", \"action\": \"sell\", " +
            "\"outcome\": \"yes\", \"price\": {\"cents\": \"35\"}, \"quantity\": 2, \"timeInForce\": \"IOC\"}");
        assertEquals("accepted", sold.path("status").asText(), sold.toString());

        keeper = client.logon("KEEPER1", "dev-market-maker", ADAPTER_PORT, true);
        assertNotNull(keeper, "the keeper logs on again");
        final FixClient.Message missed = report(keeper, "k1", 'F');
        assertEquals(kept.get("k1"), missed.get(37), missed.toString());
        assertEquals("2", missed.get(32), missed.toString());
        assertEquals("35", missed.get(31), missed.toString());
        assertEquals("2", missed.get(14), missed.toString());
        assertEquals("3", missed.get(151), missed.toString());

        final Session away = keeper;
        away.logoutAndDisconnect();
        client.pollUntil("the keeper is disconnected", () -> client.isDisconnected(away));
    }

    @Test
    @Order(10)
    void massStatusTakesUpTheAccountsOpenOrdersWhichCanThenBeCancelledAndReplaced() throws Exception
    {
        // A session that never placed an order, on the account whose orders an earlier session left behind.
        other = client.logon("KEEPER2", "dev-market-maker", ADAPTER_PORT, false);
        assertNotNull(other, "the second session logs on");
        client.send(other, massStatus("ms-all", 7, null, null));
        final List<FixClient.Message> all = massStatusReports(other, "ms-all");
        final Map<String, FixClient.Message> byClOrdId =
            all.stream().collect(Collectors.toMap(m -> m.get(11), m -> m));
        assertTrue(byClOrdId.keySet().containsAll(List.of("k1", "k2", "k3")), all.toString());
        for (final FixClient.Message status : all)
        {
            assertEquals("I", status.get(150), status.toString());
            assertEquals(String.valueOf(all.size()), status.get(911), status.toString());
        }
        final FixClient.Message k1 = byClOrdId.get("k1");
        assertEquals(kept.get("k1"), k1.get(37));
        assertEquals("1", k1.get(39), "partly filled: " + k1);
        assertEquals("5", k1.get(38), k1.toString());
        assertEquals("2", k1.get(14), k1.toString());
        assertEquals("3", k1.get(151), k1.toString());
        assertEquals("35", k1.get(6), k1.toString());
        assertEquals("35", k1.get(44), k1.toString());
        assertEquals("0", byClOrdId.get("k2").get(39), byClOrdId.get("k2").toString());

        // Cancelled by the ClOrdID the earlier session placed it with.
        client.send(other, cancel("k2-c", "k2", ARSENAL, '1'));
        final FixClient.Message cancelled = report(other, "k2-c", '4');
        assertEquals("k2", cancelled.get(41), cancelled.toString());
        assertEquals(kept.get("k2"), cancelled.get(37), cancelled.toString());
        assertEquals("0", cancelled.get(151), cancelled.toString());

        // Replaced by OrderID.
        final OrderCancelReplaceRequestEncoder replace = new OrderCancelReplaceRequestEncoder()
            .clOrdID("k3-r").origClOrdID("k3").orderID(kept.get("k3")).side('2').ordType('2')
            .orderQty(new DecimalFloat(8)).price(new DecimalFloat(71))
            .transactTime(now.buffer(), now.encode(System.currentTimeMillis()));
        replace.instrument().symbol(BITCOIN);
        client.send(other, replace);
        final FixClient.Message replaced = report(other, "k3-r", '5');
        assertEquals("k3", replaced.get(41), replaced.toString());
        assertEquals("8", replaced.get(38), replaced.toString());
        assertEquals("8", replaced.get(151), replaced.toString());
        assertEquals("71", replaced.get(44), replaced.toString());

        // The order's executions are this session's to hear about from now on, whoever causes them.
        final JsonNode bought = rest("dev-retail-customer", "POST", "/v4/orders",
            "{\"clientOrderId\": \"away2\", \"instrumentId\": \"" + BITCOIN + "\", \"action\": \"buy\", " +
            "\"outcome\": \"yes\", \"price\": {\"cents\": \"71\"}, \"quantity\": 3, \"timeInForce\": \"IOC\"}");
        assertEquals("accepted", bought.path("status").asText(), bought.toString());
        final FixClient.Message fill = report(other, "k3-r", 'F');
        assertEquals("3", fill.get(32), fill.toString());
        assertEquals("3", fill.get(14), fill.toString());
        assertEquals("5", fill.get(151), fill.toString());
        assertEquals("1", fill.get(39), fill.toString());
    }

    @Test
    @Order(11)
    void orderStatusFindsAnOrderPlacedOverRestAndSaysWhenThereIsNone() throws Exception
    {
        final JsonNode placed = rest("dev-market-maker", "POST", "/v4/orders",
            "{\"clientOrderId\": \"rest1\", \"instrumentId\": \"" + RAIN + "\", \"action\": \"buy\", " +
            "\"outcome\": \"yes\", \"price\": {\"cents\": \"25\"}, \"quantity\": 3}");
        assertEquals("accepted", placed.path("status").asText(), placed.toString());
        final String restId = placed.path("orderId").asText();

        client.send(other, status("s1", null, "rest1", RAIN, '1'));
        final FixClient.Message status = client.expect(other, "the status of rest1",
            m -> "8".equals(m.type()) && "s1".equals(m.get(790)));
        assertEquals("I", status.get(150), status.toString());
        assertEquals("0", status.get(39), status.toString());
        assertEquals(restId, status.get(37), status.toString());
        assertEquals("rest1", status.get(11), status.toString());
        assertEquals("3", status.get(38), status.toString());
        assertEquals("3", status.get(151), status.toString());
        assertEquals("0", status.get(14), status.toString());
        assertEquals("25", status.get(44), status.toString());
        assertNull(status.get(911), "a single status is not one of a mass status: " + status);

        client.send(other, status("s2", "999999999", "no-such-order", RAIN, '1'));
        final FixClient.Message unknown = client.expect(other, "the status of an unknown order",
            m -> "8".equals(m.type()) && "s2".equals(m.get(790)));
        assertEquals("I", unknown.get(150), unknown.toString());
        assertEquals("8", unknown.get(39), unknown.toString());
        assertEquals("5", unknown.get(103), unknown.toString());
        assertEquals("999999999", unknown.get(37), unknown.toString());

        // By market, by event, and a market with no open orders: one report saying there are none.
        client.send(other, massStatus("ms-rain", 1, RAIN, null));
        assertEquals(Set.of("k1", "rest1"), clOrdIds(massStatusReports(other, "ms-rain")));
        client.send(other, massStatus("ms-event", 8, null, "1000001"));
        assertEquals(Set.of("k1", "rest1"), clOrdIds(massStatusReports(other, "ms-event")));
        client.send(other, massStatus("ms-none", 1, ARSENAL, null));
        final List<FixClient.Message> none = massStatusReports(other, "ms-none");
        assertEquals(1, none.size(), none.toString());
        assertEquals("0", none.get(0).get(911), none.toString());
        assertEquals("NONE", none.get(0).get(37), none.toString());

        // Known to the session now, so it can be cancelled over FIX.
        client.send(other, cancel("rest1-c", "rest1", RAIN, '1'));
        assertEquals(restId, report(other, "rest1-c", '4').get(37));

        client.send(other, massCancel("tidy", '7', null));
        client.expect(other, "a mass cancel report", m -> "r".equals(m.type()));
        other.logoutAndDisconnect();
        client.pollUntil("the second session is disconnected", () -> client.isDisconnected(other));
    }

    @Test
    @Order(12)
    void afterARestartASessionCarriesOnAndCanHaveEverythingResent()
    {
        adapter.close();
        adapter = FixAdapter.start(ADAPTER);

        // No ResetSeqNumFlag: the adapter remembers where the session's sequence numbers stood before the restart.
        maker = client.logon("MAKER1", "dev-market-maker", ADAPTER_PORT, true);
        assertNotNull(maker, "the market maker logs on again");

        // The order cancelled when the session ended before the restart is reported now: the adapter kept what the
        // session knew, and the gateway replays what happened since.
        final FixClient.Message endedWithTheSession = report(maker, "m7", '4');
        assertTrue(endedWithTheSession.get(58).contains("session_disconnect"), endedWithTheSession.toString());

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

    @Test
    @Order(13)
    void theAuditLogHoldsEveryMessageOfEverySessionBothWays() throws Exception
    {
        // A TestRequest, which the adapter's own code never sees: Artio answers it with a Heartbeat.
        taker = client.logon("TAKER1", "dev-retail-customer", ADAPTER_PORT, false);
        assertNotNull(taker, "the retail customer logs on again");
        client.send(taker, new TestRequestEncoder().testReqID("audit-probe"));
        client.expect(taker, "the heartbeat answering the test request",
            m -> "0".equals(m.type()) && "audit-probe".equals(m.get(112)));
        taker.logoutAndDisconnect();
        client.pollUntil("the taker is disconnected", () -> client.isDisconnected(taker));

        // Closing the adapter closes its audit file, as a restart did before.
        adapter.close();
        adapter = null;

        final Path dir = ADAPTER.auditDir();
        final List<Path> files;
        try (var paths = Files.list(dir))
        {
            files = paths.sorted().toList();
        }
        assertEquals(2, files.size(), "one file for each run of the adapter: " + files);
        final List<JsonNode> records = new ArrayList<>();
        for (final Path file : files)
        {
            assertTrue(file.getFileName().toString().startsWith("fix-adapter.test."), file.toString());
            assertNull(AuditLog.verify(file), "the chain of " + file + " verifies");
            final List<String> lines = Files.readAllLines(file);
            assertEquals("close", GatewayLink.JSON.readTree(lines.get(lines.size() - 1)).path("kind").asText(),
                "closed: " + file);
            assertTrue(lines.stream().noneMatch(l -> l.contains("dev-market-maker") || l.contains("dev-retail")),
                "no token is written in clear in " + file);
            for (final String line : lines)
            {
                records.add(GatewayLink.JSON.readTree(line));
            }
        }
        assertEquals(files.get(0).getFileName().toString(), records.stream()
            .filter(r -> "open".equals(r.path("kind").asText()) && r.has("after")).findFirst()
            .map(r -> r.path("after").path("file").asText()).orElse(""), "the second file follows the first");

        // A logon both ways, its token masked; an order and the report that answered it.
        final JsonNode logon = fix(records, "in", "MAKER1", "A");
        assertTrue(logon.path("msg").asText().contains("|554=***|"), logon.toString());
        assertTrue(logon.path("msg").asText().startsWith("8=FIX.4.4|"), logon.toString());
        fix(records, "out", "VENUE", "A", "|56=MAKER1|");
        final JsonNode order = fix(records, "in", "MAKER1", "D", "|11=m1|");
        final JsonNode report = fix(records, "out", "VENUE", "8", "|11=m1|", "|150=0|");
        assertEquals(order.path("conn").asLong(), report.path("conn").asLong(), "one connection");
        assertTrue(order.path("n").asLong() < report.path("n").asLong(), "the report is recorded after the order");

        // Session-level messages Artio generates: the heartbeat answering a test request, a gap fill answering a
        // resend request, and logouts.
        // Order status: the requests and their reports.
        fix(records, "in", "KEEPER2", "AF", "|584=ms-all|", "|585=7|");
        fix(records, "out", "VENUE", "8", "|150=I|", "|584=ms-all|", "|912=Y|");
        fix(records, "in", "KEEPER2", "H", "|790=s1|");
        fix(records, "out", "VENUE", "8", "|150=I|", "|790=s1|");

        fix(records, "in", "TAKER1", "1", "|112=audit-probe|");
        fix(records, "out", "VENUE", "0", "|112=audit-probe|");
        fix(records, "in", "MAKER1", "2");
        fix(records, "out", "VENUE", "4", "|123=Y|", "|56=MAKER1|");
        fix(records, "in", "MAKER1", "5");
        fix(records, "out", "VENUE", "5", "|56=MAKER1|");

        // Refused logons: the Logon, the Logout that refused it, and what the gateway said.
        final JsonNode refused = fix(records, "in", "BADTOKEN", "A", "|554=***|");
        assertEquals("AUTH_REJECT", refused.path("status").asText(), refused.toString());
        // On the same connection: an initiator that hears nothing for a while tries again on a second one, and the
        // two refusals can then be recorded in either order.
        final JsonNode logout = records.stream()
            .filter(r -> "fix".equals(r.path("kind").asText()) && "out".equals(r.path("dir").asText()) &&
                "5".equals(r.path("msgType").asText()) && r.path("conn").asLong() == refused.path("conn").asLong())
            .findFirst().orElseThrow(() -> new AssertionError("no Logout on the refused connection: " + refused));
        assertTrue(logout.path("msg").asText().contains("|56=BADTOKEN|"), logout.toString());
        assertTrue(logout.path("msg").asText().contains("58=token refused"), logout.toString());
        assertTrue(records.stream().anyMatch(r -> "gateway-login".equals(r.path("kind").asText()) &&
            "BADTOKEN".equals(r.path("sender").asText()) && r.path("refused").asText().contains("UNAUTHORIZED")),
            "the gateway's refusal is recorded");
        fix(records, "in", "NOTOKEN", "A");
        assertTrue(records.stream().anyMatch(r -> "gateway-login".equals(r.path("kind").asText()) &&
            "MAKER1".equals(r.path("sender").asText()) && !r.path("gatewaySession").asText().isEmpty()),
            "an accepted logon names the gateway session it opened");
        assertTrue(records.stream().anyMatch(r -> "disconnect".equals(r.path("kind").asText())));
    }

    @Test
    @Order(14)
    void aFirmLogsOnWithAnApiKeyWhichOnlyDoesWhatItsPermissionsAllowAndStopsWhenRevoked() throws Exception
    {
        adapter = FixAdapter.start(ADAPTER);
        final JsonNode reader = makeKey("dev-retail-customer", "fix read", "[\"read\"]");
        final JsonNode trader = makeKey("dev-retail-customer", "fix trade", "[\"read\", \"trade\"]");

        // The key ID in Username (553) and the secret in Password (554): a read-only key logs on, and cannot trade.
        final Session readOnly = client.logon(
            "KEYREAD", reader.path("keyId").asText(), reader.path("secret").asText(), ADAPTER_PORT, false);
        assertNotNull(readOnly, "a read-only key logs on");
        client.send(readOnly, order("k1", RAIN, '1', 10, "5"));
        final FixClient.Message refused = report(readOnly, "k1", '8');
        assertTrue(refused.get(58).contains("FORBIDDEN") && refused.get(58).contains("trade"), refused.toString());
        assertEquals("99", refused.get(103), refused.toString());

        // The whole credential in Password, with Username the firm's own: this key trades.
        final Session trading =
            client.logon("KEYTRADE", "desk-7", trader.path("credential").asText(), ADAPTER_PORT, false);
        assertNotNull(trading, "a trading key logs on");
        client.send(trading, order("k2", RAIN, '1', 10, "5"));
        final String k2 = report(trading, "k2", '0').get(37);

        // Status with keys: each session asks the gateway as its own key, which holds read whatever else it holds.
        // The read-only key, from Username and Password, sees the order the trading key placed on the same account.
        client.send(readOnly, massStatus("key-ms", 1, RAIN, null));
        final List<FixClient.Message> seen = massStatusReports(readOnly, "key-ms");
        assertTrue(seen.stream().anyMatch(m -> k2.equals(m.get(37)) && "0".equals(m.get(39))), seen.toString());
        client.send(trading, status("key-s", null, "k2", RAIN, '1'));
        final FixClient.Message known = client.expect(trading, "the status of k2",
            m -> "8".equals(m.type()) && "key-s".equals(m.get(790)));
        assertEquals(k2, known.get(37), known.toString());
        client.send(trading, massStatus("key-ms2", 7, null, null));
        assertTrue(clOrdIds(massStatusReports(trading, "key-ms2")).contains("k2"));
        final Path gatewayAudit = scratch.resolve("venue").resolve("gateway-audit");
        client.pollUntil("the gateway records the status calls under the reading key", () ->
            gatewayRecords(gatewayAudit).stream().anyMatch(r -> "http-request".equals(r.path("kind").asText()) &&
                r.path("line").asText().startsWith("GET /v4/account") &&
                reader.path("keyId").asText().equals(r.path("key").asText())));

        // Revoked: the session is logged out within the gateway's bound, and the key logs on no more.
        final HttpResponse<String> revoked = HttpClient.newHttpClient().send(
            HttpRequest.newBuilder(URI.create("http://" + GATEWAY + "/v4/keys/" + trader.path("keyId").asText()))
                .header("Authorization", "Bearer dev-retail-customer").DELETE().build(),
            HttpResponse.BodyHandlers.ofString());
        assertEquals(200, revoked.statusCode(), revoked.body());
        // Status requests until the session ends: each is answered as before while the gateway still remembers the
        // key, or refused as unauthorized once it does not, and the session ends either way.
        final long[] next = {0};
        final int[] asked = {0};
        client.pollUntil("the revoked key's session ends", () ->
        {
            if (client.isDisconnected(trading))
            {
                return true;
            }
            if (System.nanoTime() > next[0] &&
                trading.trySend(massStatus("rv-" + asked[0], 7, null, null)) > 0)
            {
                asked[0]++;
                next[0] = System.nanoTime() + 250_000_000L;
            }
            return false;
        });
        for (final FixClient.Message reject : client.drain(trading, Duration.ofMillis(10), m -> "j".equals(m.type())))
        {
            assertEquals("6", reject.get(380), reject.toString());
            assertTrue(reject.get(58).startsWith("UNAUTHORIZED"), reject.toString());
        }
        final HttpResponse<String> statusAfter = HttpClient.newHttpClient().send(
            HttpRequest.newBuilder(URI.create("http://" + GATEWAY + "/v4/account"))
                .header("Authorization", "Bearer " + trader.path("credential").asText()).GET().build(),
            HttpResponse.BodyHandlers.ofString());
        assertEquals(401, statusAfter.statusCode(), "the revoked key reads no status: " + statusAfter.body());
        assertNull(client.logon("KEYGONE", "desk-7", trader.path("credential").asText(), ADAPTER_PORT, false),
            "a revoked key is refused");

        readOnly.logoutAndDisconnect();
        client.pollUntil("the read-only session is disconnected", () -> client.isDisconnected(readOnly));
        adapter.close();
        adapter = null;
        final List<JsonNode> logins = new ArrayList<>();
        try (var paths = Files.list(ADAPTER.auditDir()))
        {
            for (final Path file : paths.toList())
            {
                assertNull(AuditLog.verify(file), "the chain of " + file + " verifies");
                for (final String line : Files.readAllLines(file))
                {
                    assertTrue(!line.contains(reader.path("secret").asText()) &&
                        !line.contains(trader.path("secret").asText()), "a key's secret is written in " + file);
                    final JsonNode record = GatewayLink.JSON.readTree(line);
                    if ("gateway-login".equals(record.path("kind").asText()))
                    {
                        logins.add(record);
                    }
                }
            }
        }
        assertTrue(logins.stream().anyMatch(r -> "KEYREAD".equals(r.path("sender").asText()) &&
            reader.path("keyId").asText().equals(r.path("key").asText())), "an accepted logon names its key");
        for (final JsonNode record : gatewayRecords(gatewayAudit))
        {
            final String text = record.toString();
            assertTrue(!text.contains(reader.path("secret").asText()) && !text.contains(trader.path("secret").asText()),
                "a key's secret is written in the gateway's audit log: " + text);
        }
        // Nothing the adapter keeps about its sessions holds a secret either.
        assertTrue(Files.isDirectory(ADAPTER_DATA.resolve("sessions")), "the adapter keeps its sessions' memory");
        try (var paths = Files.walk(ADAPTER_DATA.resolve("sessions")))
        {
            for (final Path file : paths.filter(Files::isRegularFile).toList())
            {
                final String text = Files.readString(file);
                assertTrue(!text.contains(reader.path("secret").asText()) &&
                    !text.contains(trader.path("secret").asText()), "a key's secret is kept in " + file);
            }
        }
    }

    /** Every record of the gateway's audit log so far; a record being written is skipped. */
    private static List<JsonNode> gatewayRecords(final Path dir)
    {
        final List<JsonNode> records = new ArrayList<>();
        try (var paths = Files.list(dir))
        {
            for (final Path file : paths.sorted().toList())
            {
                for (final String line : Files.readAllLines(file))
                {
                    try
                    {
                        records.add(GatewayLink.JSON.readTree(line));
                    }
                    catch (final IOException e)
                    {
                        // A record cut off where the writer is.
                    }
                }
            }
        }
        catch (final IOException e)
        {
            throw new java.io.UncheckedIOException(e);
        }
        return records;
    }

    /** Makes an API key for the account of the dev token, through the gateway, and returns it with its secret. */
    private static JsonNode makeKey(final String token, final String label, final String permissions)
        throws IOException, InterruptedException
    {
        final String body = "{\"label\": \"" + label + "\", \"permissions\": " + permissions + "}";
        final HttpResponse<String> response = HttpClient.newHttpClient().send(
            HttpRequest.newBuilder(URI.create("http://" + GATEWAY + "/v4/keys"))
                .header("Authorization", "Bearer " + token).POST(HttpRequest.BodyPublishers.ofString(body)).build(),
            HttpResponse.BodyHandlers.ofString());
        assertEquals(200, response.statusCode(), response.body());
        return GatewayLink.JSON.readTree(response.body());
    }

    /** The first FIX record going that way, from that SenderCompID, of that MsgType, whose message has all of those. */
    private static JsonNode fix(
        final List<JsonNode> records, final String dir, final String sender, final String msgType,
        final String... contains)
    {
        return records.stream()
            .filter(r -> "fix".equals(r.path("kind").asText()) && dir.equals(r.path("dir").asText()) &&
                sender.equals(r.path("sender").asText()) && msgType.equals(r.path("msgType").asText()) &&
                Arrays.stream(contains).allMatch(r.path("msg").asText()::contains))
            .findFirst()
            .orElseThrow(() -> new AssertionError("no " + dir + " " + msgType + " from " + sender + " containing " +
                Arrays.toString(contains) + " in the audit log"));
    }

    @Test
    @Order(15)
    void anotherGatewaySeesAndChangesTheSameOrdersAsTheAdapters() throws Exception
    {
        // The adapter's gateway is one of two: a firm on FIX and a program on the other gateway trade one account.
        adapter = FixAdapter.start(ADAPTER);
        final Session desk = client.logon("SPLIT1", "dev-market-maker", ADAPTER_PORT, false);
        assertNotNull(desk, "the desk logs on");

        // Placed over FIX, seen and cancelled through the other gateway, straight away: the other gateway waits for
        // its own feed to deliver the order rather than saying there is none. The desk hears the cancel.
        client.send(desk, order("g1", RAIN, '1', 4, "12"));
        final String g1 = report(desk, "g1", '0').get(37);
        final JsonNode cancelled = rest(SECOND_GATEWAY, "dev-market-maker", "DELETE", "/v4/orders/" + g1, null);
        assertEquals("cancelled", cancelled.path("status").asText(), cancelled.toString());
        assertEquals(g1, report(desk, "g1", '4').get(37));
        assertTrue(!restingOrdersOn(SECOND_GATEWAY, "dev-market-maker").contains(g1), "the other gateway shows g1");

        // Placed through the other gateway: the adapter's order status, answered from its own gateway's snapshot, says
        // the same once that gateway has had it from the feed.
        final JsonNode placed = rest(SECOND_GATEWAY, "dev-market-maker", "POST", "/v4/orders",
            "{\"clientOrderId\": \"g2\", \"instrumentId\": \"" + RAIN + "\", \"action\": \"buy\", " +
            "\"outcome\": \"yes\", \"price\": {\"cents\": \"13\"}, \"quantity\": 6}");
        assertEquals("accepted", placed.path("status").asText(), placed.toString());
        final String g2 = placed.path("orderId").asText();
        client.pollUntil("the adapter's gateway has g2", () -> restingOrdersOn(GATEWAY, "dev-market-maker").contains(g2));
        client.send(desk, status("g2-s", g2, "g2", RAIN, '1'));
        final FixClient.Message status = client.expect(desk, "the status of g2",
            m -> "8".equals(m.type()) && "g2-s".equals(m.get(790)));
        assertEquals(g2, status.get(37), status.toString());
        assertEquals("0", status.get(39), status.toString());
        assertEquals("6", status.get(38), status.toString());
        assertEquals("6", status.get(151), status.toString());
        assertEquals("13", status.get(44), status.toString());

        // Cancelled over FIX: gone on the other gateway too.
        client.send(desk, cancel("g2-c", "g2", RAIN, '1'));
        assertEquals(g2, report(desk, "g2-c", '4').get(37));
        client.pollUntil("the other gateway no longer shows g2",
            () -> !restingOrdersOn(SECOND_GATEWAY, "dev-market-maker").contains(g2));

        desk.logoutAndDisconnect();
        client.pollUntil("the desk is disconnected", () -> client.isDisconnected(desk));
        adapter.close();
        adapter = null;
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

    private Encoder status(
        final String reqId, final String orderId, final String clOrdId, final String symbol, final char side)
    {
        final OrderStatusRequestEncoder request = new OrderStatusRequestEncoder()
            .ordStatusReqID(reqId).clOrdID(clOrdId).side(side);
        if (orderId != null)
        {
            request.orderID(orderId);
        }
        request.instrument().symbol(symbol);
        return request;
    }

    private static Encoder massStatus(final String reqId, final int type, final String symbol, final String event)
    {
        final OrderMassStatusRequestEncoder request = new OrderMassStatusRequestEncoder()
            .massStatusReqID(reqId).massStatusReqType(type);
        if (symbol != null)
        {
            request.symbol(symbol);
        }
        if (event != null)
        {
            request.underlyingSymbol(event);
        }
        return request;
    }

    /** Every report answering a mass status request, up to the one with LastRptRequested (912) = Y. */
    private static List<FixClient.Message> massStatusReports(final Session session, final String reqId)
    {
        final List<FixClient.Message> reports = new ArrayList<>();
        while (reports.isEmpty() || !"Y".equals(reports.get(reports.size() - 1).get(912)))
        {
            reports.add(client.expect(session, "a status report for " + reqId,
                m -> "8".equals(m.type()) && reqId.equals(m.get(584))));
        }
        return reports;
    }

    private static Set<String> clOrdIds(final List<FixClient.Message> reports)
    {
        return reports.stream().map(m -> m.get(11)).collect(Collectors.toSet());
    }

    /** A request to the gateway's REST half as the account the token is of, and the JSON it answered with. */
    private static JsonNode rest(final String token, final String method, final String path, final String body)
        throws IOException, InterruptedException
    {
        return rest(GATEWAY, token, method, path, body);
    }

    /** The same, to the gateway at {@code gateway}. */
    private static JsonNode rest(
        final String gateway, final String token, final String method, final String path, final String body)
        throws IOException, InterruptedException
    {
        final HttpResponse<String> response = HttpClient.newHttpClient().send(
            HttpRequest.newBuilder(URI.create("http://" + gateway + path))
                .header("Authorization", "Bearer " + token).header("Content-Type", "application/json")
                .method(method, body == null ? HttpRequest.BodyPublishers.noBody() :
                    HttpRequest.BodyPublishers.ofString(body))
                .build(),
            HttpResponse.BodyHandlers.ofString());
        return GatewayLink.JSON.readTree(response.body());
    }

    /** The account's resting order IDs, from the gateway's REST half. */
    private static List<String> restingOrders(final String token) throws IOException, InterruptedException
    {
        return restingOrders(GATEWAY, token);
    }

    /** The same, as the gateway at {@code gateway} sees them. */
    private static List<String> restingOrdersOn(final String gateway, final String token)
    {
        try
        {
            return restingOrders(gateway, token);
        }
        catch (final Exception e)
        {
            throw new AssertionError(e);
        }
    }

    private static List<String> restingOrders(final String gateway, final String token)
        throws IOException, InterruptedException
    {
        final HttpResponse<String> response = HttpClient.newHttpClient().send(
            HttpRequest.newBuilder(URI.create("http://" + gateway + "/v4/account"))
                .header("Authorization", "Bearer " + token).GET().build(),
            HttpResponse.BodyHandlers.ofString());
        final JsonNode account = GatewayLink.JSON.readTree(response.body());
        return account.path("orders").findValuesAsText("orderId");
    }
}

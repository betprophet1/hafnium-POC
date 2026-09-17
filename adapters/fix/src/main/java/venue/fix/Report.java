package venue.fix;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ObjectNode;

import java.math.BigDecimal;

/**
 * One execution report, filled in field by field and encoded when it is sent. Plain data, so that a report a session
 * could not deliver before it ended can be kept ({@link SessionMemory}) and sent on its next logon.
 */
final class Report
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
    // Order status (150=I) only.
    String ordStatusReqId;
    String massStatusReqId;
    int totNumReports = -1;
    boolean lastRptRequested;

    ObjectNode toJson()
    {
        final ObjectNode json = GatewayLink.JSON.createObjectNode()
            .put("orderId", orderId).put("clOrdId", clOrdId).put("origClOrdId", origClOrdId).put("execId", execId)
            .put("execType", String.valueOf(execType)).put("ordStatus", String.valueOf(ordStatus))
            .put("symbol", symbol).put("side", String.valueOf(side)).put("orderQty", orderQty).put("price", price)
            .put("timeInForce", timeInForce == 0 ? null : String.valueOf(timeInForce)).put("execInst", execInst)
            .put("lastQty", lastQty).put("lastPx", lastPx).put("liquidity", liquidity).put("leavesQty", leavesQty)
            .put("cumQty", cumQty).put("avgPx", avgPx.toPlainString()).put("ordRejReason", ordRejReason)
            .put("restated", restated).put("text", text);
        return json;
    }

    static Report fromJson(final JsonNode json)
    {
        final Report r = new Report();
        r.orderId = json.path("orderId").asText("NONE");
        r.clOrdId = text(json, "clOrdId");
        r.origClOrdId = text(json, "origClOrdId");
        r.execId = text(json, "execId");
        r.execType = json.path("execType").asText("0").charAt(0);
        r.ordStatus = json.path("ordStatus").asText("0").charAt(0);
        r.symbol = text(json, "symbol");
        r.side = json.path("side").asText("1").charAt(0);
        r.orderQty = json.path("orderQty").asLong();
        r.price = text(json, "price");
        final String tif = text(json, "timeInForce");
        r.timeInForce = tif == null ? 0 : tif.charAt(0);
        r.execInst = text(json, "execInst");
        r.lastQty = json.path("lastQty").asLong();
        r.lastPx = text(json, "lastPx");
        r.liquidity = json.path("liquidity").asInt();
        r.leavesQty = json.path("leavesQty").asLong();
        r.cumQty = json.path("cumQty").asLong();
        r.avgPx = new BigDecimal(json.path("avgPx").asText("0"));
        r.ordRejReason = json.path("ordRejReason").asInt(-1);
        r.restated = json.path("restated").asBoolean();
        r.text = text(json, "text");
        return r;
    }

    /** A string field, or null when it is absent or JSON null. */
    static String text(final JsonNode node, final String field)
    {
        final JsonNode value = node.get(field);
        return value == null || value.isNull() ? null : value.asText();
    }
}

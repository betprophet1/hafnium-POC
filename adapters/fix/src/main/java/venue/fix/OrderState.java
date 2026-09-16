package venue.fix;

import java.math.BigDecimal;
import java.math.RoundingMode;

/**
 * What a FIX session knows about one of its orders. The venue reports each change on its own (executions carry the
 * change and what remains); FIX reports the whole order every time (OrderQty, CumQty, LeavesQty, AvgPx), so the
 * adapter keeps the running totals the venue does not send.
 */
final class OrderState
{
    final String orderId;
    final String symbol;
    /** Side (54): '1' buy YES, '2' sell YES. */
    final char side;
    final char timeInForce;
    /** ExecInst (18) as the client sent it, or null. */
    final String execInst;

    /** The ClOrdID the order currently answers to: the latest accepted replace's. */
    String clOrdId;
    /** OrderQty (38): what has filled plus what still rests. */
    long orderQty;
    long cumQty;
    long leavesQty;
    /** The limit price in cents, as the venue quotes it. */
    String price;
    /** Sum of quantity times price over fills, for AvgPx. */
    BigDecimal filledNotional = BigDecimal.ZERO;
    boolean done;

    /** A cancel or replace sent for this order and not yet answered, or null. */
    Pending pending;

    /** A change the client asked for, remembered so the venue's answer can be reported under the right ClOrdIDs. */
    record Pending(boolean replace, String clOrdId, long orderQty)
    {
    }

    OrderState(
        final String orderId,
        final String clOrdId,
        final String symbol,
        final char side,
        final long orderQty,
        final String price,
        final char timeInForce,
        final String execInst)
    {
        this.orderId = orderId;
        this.clOrdId = clOrdId;
        this.symbol = symbol;
        this.side = side;
        this.orderQty = orderQty;
        this.leavesQty = orderQty;
        this.price = price;
        this.timeInForce = timeInForce;
        this.execInst = execInst;
    }

    /** OrdStatus (39) for an order that is still live. */
    char liveStatus()
    {
        return cumQty > 0 ? '1' : '0';
    }

    /** OrdStatus (39) as it stands. */
    char status()
    {
        if (!done)
        {
            return liveStatus();
        }
        return leavesQty == 0 && cumQty == orderQty ? '2' : '4';
    }

    void fill(final long quantity, final BigDecimal pricePerContract, final long remaining)
    {
        cumQty += quantity;
        leavesQty = remaining;
        filledNotional = filledNotional.add(pricePerContract.multiply(BigDecimal.valueOf(quantity)));
        if (remaining == 0)
        {
            done = true;
        }
    }

    BigDecimal avgPx()
    {
        return cumQty == 0 ? BigDecimal.ZERO :
            filledNotional.divide(BigDecimal.valueOf(cumQty), 4, RoundingMode.HALF_EVEN).stripTrailingZeros();
    }
}

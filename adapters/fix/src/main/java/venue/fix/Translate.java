package venue.fix;

import uk.co.real_logic.artio.fields.ReadOnlyDecimalFloat;

import java.math.BigDecimal;

/**
 * The fixed parts of the dialect (interfaces/fix/README.md): how FIX values become the gateway's, and how the
 * gateway's reasons become FIX reject codes. Nothing here holds state.
 */
final class Translate
{
    // OrdRejReason (103)
    static final int UNKNOWN_SYMBOL = 1;
    static final int EXCHANGE_CLOSED = 2;
    static final int ORDER_EXCEEDS_LIMIT = 3;
    static final int DUPLICATE_ORDER = 6;
    static final int INCORRECT_QUANTITY = 13;
    static final int OTHER = 99;

    // CxlRejReason (102)
    static final int TOO_LATE_TO_CANCEL = 0;
    static final int UNKNOWN_ORDER = 1;
    static final int ALREADY_PENDING = 3;

    private Translate()
    {
    }

    /**
     * A FIX price or quantity as an exact decimal. DecimalFloat keeps the digits the client sent, so nothing is
     * rounded on the way to the gateway, which refuses a price between ladder rows rather than rounding it.
     */
    static BigDecimal decimal(final ReadOnlyDecimalFloat value)
    {
        return BigDecimal.valueOf(value.value(), value.scale());
    }

    /** A price in cents as the gateway's {@code cents} form: plain digits, no exponent, no trailing zeros. */
    static String cents(final ReadOnlyDecimalFloat price)
    {
        final BigDecimal exact = decimal(price).stripTrailingZeros();
        return (exact.scale() < 0 ? exact.setScale(0) : exact).toPlainString();
    }

    /**
     * A whole number of contracts, or -1 when the quantity is not one. The venue trades whole contracts; a fraction
     * is refused rather than truncated.
     */
    static long contracts(final ReadOnlyDecimalFloat quantity)
    {
        final BigDecimal exact = decimal(quantity);
        try
        {
            final long whole = exact.longValueExact();
            return whole > 0 ? whole : -1;
        }
        catch (final ArithmeticException notWhole)
        {
            return -1;
        }
    }

    /** OrdRejReason for an engine rejection, given in lower snake case as the gateway reports it. */
    static int ordRejReason(final String engineReason)
    {
        return switch (engineReason)
        {
            case "unknown_instrument" -> UNKNOWN_SYMBOL;
            case "market_not_open", "market_closed", "market_resolved", "venue_mode", "closed_only_mode",
                "event_started" -> EXCHANGE_CLOSED;
            case "insufficient_sleeve", "position_limit", "rule_limit", "spend_cap_reached",
                "spend_cap_below_one_contract", "order_capacity", "reduce_only_no_position" -> ORDER_EXCEEDS_LIMIT;
            case "duplicate_client_order_id" -> DUPLICATE_ORDER;
            case "quantity_not_on_lot_step" -> INCORRECT_QUANTITY;
            default -> OTHER;
        };
    }

    /** OrdRejReason for a refusal at the gateway's edge, by its error code. */
    static int ordRejReasonForError(final String code)
    {
        return switch (code)
        {
            case "NOT_FOUND" -> UNKNOWN_SYMBOL;
            default -> OTHER;
        };
    }

    /** CxlRejReason for an engine rejection of a cancel or an amendment. */
    static int cxlRejReason(final String engineReason)
    {
        return "unknown_order".equals(engineReason) ? UNKNOWN_ORDER : OTHER;
    }
}

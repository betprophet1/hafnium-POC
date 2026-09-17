package venue.fix;

import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.databind.node.ObjectNode;

import java.io.PrintStream;
import java.time.Instant;
import java.time.temporal.ChronoUnit;

/**
 * The adapter's operational log (deploy/observability/metrics.md, logs): one line per event on standard output. With
 * {@code VENUE_LOG_FORMAT=json} each line is a JSON object ({@code ts}, {@code level}, {@code service},
 * {@code instance}, {@code msg} and the event's fields), for a log collector; otherwise a line a person reads.
 *
 * <p>Separate from the audit log ({@link AuditLog}), which records FIX traffic in its own hash-chained files. Nothing
 * logged here may carry a Password (554), token or key secret.
 */
final class Log
{
    static final String SERVICE = "venue-fix";

    private static final boolean JSON = "json".equalsIgnoreCase(System.getenv("VENUE_LOG_FORMAT"));
    private static final String INSTANCE = instance();
    private static final PrintStream OUT = System.out;

    private Log()
    {
    }

    private static String instance()
    {
        final String instance = System.getenv("VENUE_INSTANCE");
        if (instance != null && !instance.isBlank())
        {
            return instance;
        }
        final String hostname = System.getenv("HOSTNAME");
        return hostname != null && !hostname.isBlank() ? hostname : "fix-adapter";
    }

    static void info(final String message, final Object... fields)
    {
        write("info", message, fields);
    }

    static void warn(final String message, final Object... fields)
    {
        write("warn", message, fields);
    }

    static void error(final String message, final Object... fields)
    {
        write("error", message, fields);
    }

    /** A throwable's stack trace as one string, for a field. */
    static String stack(final Throwable e)
    {
        final java.io.StringWriter trace = new java.io.StringWriter();
        e.printStackTrace(new java.io.PrintWriter(trace));
        return trace.toString();
    }

    /** {@code fields} are alternating names and values. */
    private static void write(final String level, final String message, final Object... fields)
    {
        final String ts = Instant.now().truncatedTo(ChronoUnit.MICROS).toString();
        final String line;
        if (JSON)
        {
            final ObjectNode node = GatewayLink.JSON.createObjectNode()
                .put("ts", ts).put("level", level).put("service", SERVICE).put("instance", INSTANCE).put("msg", message);
            for (int i = 0; i + 1 < fields.length; i += 2)
            {
                node.put(String.valueOf(fields[i]), String.valueOf(fields[i + 1]));
            }
            try
            {
                line = GatewayLink.JSON.writeValueAsString(node);
            }
            catch (final JsonProcessingException e)
            {
                throw new IllegalStateException(e);
            }
        }
        else
        {
            final StringBuilder text = new StringBuilder(ts).append(' ').append(level).append(' ').append(SERVICE)
                .append(": ").append(message);
            for (int i = 0; i + 1 < fields.length; i += 2)
            {
                text.append(' ').append(fields[i]).append('=').append(fields[i + 1]);
            }
            line = text.toString();
        }
        synchronized (OUT)
        {
            OUT.println(line);
            OUT.flush();
        }
    }
}

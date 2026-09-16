package venue.fix;

import java.nio.file.Path;
import java.util.HashMap;
import java.util.Map;
import java.util.Set;

/**
 * What one adapter process is told at startup. Every flag can also be given as an environment variable,
 * {@code VENUE_FIX_<FLAG>}, the way the Rust binaries read {@code VENUE_<FLAG>}.
 *
 * @param listenHost          where FIX clients connect; 127.0.0.1 unless deliberately exposed
 * @param listenPort          the FIX acceptor's port
 * @param compId              the venue's CompID: clients send it as TargetCompID
 * @param gateway             the gateway's {@code host:port}; the adapter uses {@code /v4/ws_private} and
 *                            {@code GET /v4/markets} on it
 * @param dataDir             Artio's session state and message archive, and the embedded Aeron directory
 * @param cancelOnDisconnect  whether sessions log in to the gateway with {@code cancelOnDisconnect}
 * @param keepOrdersFor       SenderCompIDs whose orders outlive their session, whatever the default
 */
public record AdapterConfig(
    String listenHost,
    int listenPort,
    String compId,
    String gateway,
    Path dataDir,
    boolean cancelOnDisconnect,
    Set<String> keepOrdersFor)
{
    static final String USAGE = """
        usage:
          venue-fix-adapter [--listen 127.0.0.1:17420] [--comp-id VENUE] [--gateway 127.0.0.1:8081]
                            [--data ./fix-data] [--cancel-on-disconnect true] [--keep-orders-for COMPID,...]

        FIX 4.4 on --listen, translated to the gateway's private WebSocket. A session logs on with its dev token in
        Password (554). Every flag can also be given as VENUE_FIX_<FLAG>, for example VENUE_FIX_GATEWAY.""";

    /** Whether a session with this SenderCompID asks the gateway to cancel its orders when it ends. */
    public boolean cancelOnDisconnectFor(final String senderCompId)
    {
        return cancelOnDisconnect && !keepOrdersFor.contains(senderCompId);
    }

    public static AdapterConfig parse(final String[] args)
    {
        final Map<String, String> flags = new HashMap<>();
        for (int i = 0; i < args.length; i++)
        {
            if (!args[i].startsWith("--") || i + 1 >= args.length)
            {
                throw new IllegalArgumentException("unexpected argument " + args[i] + "\n" + USAGE);
            }
            flags.put(args[i].substring(2), args[++i]);
        }
        final String listen = value(flags, "listen", "127.0.0.1:17420");
        final int colon = listen.lastIndexOf(':');
        if (colon < 0)
        {
            throw new IllegalArgumentException("--listen must be host:port, not " + listen);
        }
        final String keep = value(flags, "keep-orders-for", "");
        final AdapterConfig config = new AdapterConfig(
            listen.substring(0, colon),
            Integer.parseInt(listen.substring(colon + 1)),
            value(flags, "comp-id", "VENUE"),
            value(flags, "gateway", "127.0.0.1:8081"),
            Path.of(value(flags, "data", "./fix-data")),
            Boolean.parseBoolean(value(flags, "cancel-on-disconnect", "true")),
            keep.isBlank() ? Set.of() : Set.of(keep.split(",")));
        // Every flag read above was taken out of the map, so whatever is left was never going to be used.
        if (!flags.isEmpty())
        {
            throw new IllegalArgumentException("unknown flags " + flags.keySet() + "\n" + USAGE);
        }
        return config;
    }

    private static String value(final Map<String, String> flags, final String name, final String fallback)
    {
        final String flag = flags.remove(name);
        if (flag != null)
        {
            return flag;
        }
        final String env = System.getenv("VENUE_FIX_" + name.toUpperCase().replace('-', '_'));
        return env != null ? env : fallback;
    }
}

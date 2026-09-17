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
 *                            {@code GET /v4/account} on it
 * @param publicGateway       where the adapter reads {@code GET /v4/markets}, the public half's: the gateway itself
 *                            unless that one serves only the private half (interfaces/gateway/README.md)
 * @param dataDir             Artio's session state and message archive, and the embedded Aeron directory
 * @param cancelOnDisconnect  whether sessions log in to the gateway with {@code cancelOnDisconnect}
 * @param keepOrdersFor       SenderCompIDs whose orders outlive their session, whatever the default
 * @param auditDir            where the audit log of every FIX message is written (interfaces/audit/README.md)
 * @param auditInstance       this adapter's instance name in the audit log's file names
 * @param auditFsync          whether the audit log syncs what it writes; off only for tests
 */
public record AdapterConfig(
    String listenHost,
    int listenPort,
    String compId,
    String gateway,
    String publicGateway,
    Path dataDir,
    boolean cancelOnDisconnect,
    Set<String> keepOrdersFor,
    Path auditDir,
    String auditInstance,
    boolean auditFsync)
{
    static final String USAGE = """
        usage:
          venue-fix-adapter [--listen 127.0.0.1:17420] [--comp-id VENUE] [--gateway 127.0.0.1:8081]
                            [--public-gateway GATEWAY] [--data ./fix-data] [--cancel-on-disconnect true]
                            [--keep-orders-for COMPID,...]
                            [--audit-dir DATA/audit] [--audit-instance HOSTNAME] [--audit-fsync true]

        FIX 4.4 on --listen, translated to the gateway's private WebSocket. The security list comes from
        GET /v4/markets on --public-gateway, which defaults to --gateway. A session logs on with its dev token in
        Password (554). Every FIX message in and out is written to the audit log in --audit-dir, by default the audit
        directory inside --data. Every flag can also be given as VENUE_FIX_<FLAG>, for example VENUE_FIX_GATEWAY.""";

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
        final Path data = Path.of(value(flags, "data", "./fix-data"));
        final String hostname = System.getenv("HOSTNAME");
        final String gateway = value(flags, "gateway", "127.0.0.1:8081");
        final AdapterConfig config = new AdapterConfig(
            listen.substring(0, colon),
            Integer.parseInt(listen.substring(colon + 1)),
            value(flags, "comp-id", "VENUE"),
            gateway,
            value(flags, "public-gateway", gateway),
            data,
            Boolean.parseBoolean(value(flags, "cancel-on-disconnect", "true")),
            keep.isBlank() ? Set.of() : Set.of(keep.split(",")),
            Path.of(value(flags, "audit-dir", data.resolve("audit").toString())),
            value(flags, "audit-instance", hostname == null || hostname.isBlank() ? "fix-adapter" : hostname),
            Boolean.parseBoolean(value(flags, "audit-fsync", "true")));
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

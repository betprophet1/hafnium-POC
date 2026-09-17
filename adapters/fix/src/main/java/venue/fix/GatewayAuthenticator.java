package venue.fix;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import uk.co.real_logic.artio.decoder.AbstractLogonDecoder;
import uk.co.real_logic.artio.messages.DisconnectReason;
import uk.co.real_logic.artio.validation.AbstractAuthenticationProxy;
import uk.co.real_logic.artio.validation.AuthenticationProxy;
import uk.co.real_logic.artio.validation.AuthenticationStrategy;
import venue.fix.codecs.builder.LogoutEncoder;

import java.net.URI;
import java.net.http.HttpClient;
import java.time.Duration;
import java.util.Map;
import java.util.concurrent.CompletionException;
import java.util.concurrent.ConcurrentHashMap;

/**
 * Decides a FIX Logon by logging in to the gateway with the token the client sent, so the gateway stays the only
 * judge of credentials (interfaces/gateway/README.md, authentication).
 *
 * <p>Artio calls this on its framer thread, which serves every connection, so nothing here waits: the login runs on
 * the WebSocket client's threads and answers Artio through the {@link AuthenticationProxy} when the gateway has. A
 * session that is accepted already has its gateway session open; the polling thread collects it by connection ID when
 * Artio hands the FIX session over ({@link #take}).
 *
 * <p>Each decision is recorded in the audit log as a {@code gateway-login} record, and every connection's end as a
 * {@code disconnect} record (interfaces/audit/README.md): the FIX messages themselves are recorded by
 * {@link FixAuditor}, but only this sees what the gateway said and the firm's address.
 */
final class GatewayAuthenticator implements AuthenticationStrategy
{
    /** How long a Logon may wait on the gateway before it is refused. */
    private static final Duration LOGIN_TIMEOUT = Duration.ofSeconds(10);
    /** How long a refused client has to read the Logout before its connection is closed. */
    private static final long REJECT_LINGER_MS = 500;

    private final AdapterConfig config;
    private final HttpClient http;
    private final AuditLog audit;
    private final SessionMemory.Store memories;
    private final Metrics metrics;
    private final Map<Long, GatewayLink> accepted = new ConcurrentHashMap<>();

    GatewayAuthenticator(
        final AdapterConfig config, final HttpClient http, final AuditLog audit, final SessionMemory.Store memories,
        final Metrics metrics)
    {
        this.metrics = metrics;
        this.config = config;
        this.http = http;
        this.audit = audit;
        this.memories = memories;
    }

    @Override
    public boolean authenticate(final AbstractLogonDecoder logon)
    {
        // Artio calls authenticateAsync, which is overridden; this synchronous form would block its framer thread.
        throw new UnsupportedOperationException("authentication is asynchronous");
    }

    @Override
    public void authenticateAsync(final AbstractLogonDecoder logon, final AuthenticationProxy proxy)
    {
        // A dev token or an API key's credential in Password (554), or a key ID in Username (553) and its secret in
        // Password (Credentials).
        final String token = Credentials.of(
            logon.hasUsername() ? logon.usernameAsString() : null, logon.hasPassword() ? logon.passwordAsString() : "");
        final String sender = logon.header().senderCompIDAsString();
        if (token.isEmpty())
        {
            recordLogin(proxy, sender, audit.fields().put("refused", "no token in Password (554)"));
            metrics.logonRefused(Metrics.Refusal.INVALID);
            reject(proxy, "Password (554) must carry the account's token");
            return;
        }
        final ObjectNode login = GatewayLink.JSON.createObjectNode()
            .put("op", "login")
            .put("token", token)
            .put("cancelOnDisconnect", config.cancelOnDisconnectFor(sender));
        // The gateway's heartbeat is a dead-man's switch on this adapter: if it stops relaying the client's
        // heartbeats for three intervals, the gateway cancels the session's orders and closes it. The FIX session
        // layer (Artio) is what notices a silent client, and ends the session, which closes the gateway session too.
        final int heartBtInt = logon.heartBtInt();
        if (heartBtInt > 0)
        {
            login.put("heartbeatSeconds", Math.min(300, heartBtInt * 3));
        }
        // The account's executions since the session last dealt with them are replayed before anything new, so what
        // happened while it had no gateway session is reported now (SessionMemory).
        final long resumeFrom = memories.resumeFrom(sender);
        if (resumeFrom > 0)
        {
            login.put("resumeFrom", Long.toString(resumeFrom));
        }
        final URI uri = URI.create("ws://" + config.gateway() + "/v4/ws_private");
        GatewayLink.open(http, uri, login, LOGIN_TIMEOUT, metrics).whenComplete((link, failure) ->
        {
            if (failure == null)
            {
                final JsonNode welcome = link.welcome();
                final ObjectNode outcome = audit.fields()
                    .put("accountId", welcome.path("accountId").asText())
                    .put("gatewaySession", welcome.path("sessionId").asText());
                if (welcome.hasNonNull("keyId"))
                {
                    outcome.put("key", welcome.path("keyId").asText());
                }
                recordLogin(proxy, sender, outcome);
                accepted.put(proxy.connectionId(), link);
                try
                {
                    proxy.accept();
                    metrics.logon();
                }
                catch (final IllegalStateException timedOut)
                {
                    // Artio gave up on this logon first; nobody will collect the gateway session.
                    accepted.remove(proxy.connectionId());
                    link.close("logon abandoned");
                }
                return;
            }
            final Throwable cause = failure instanceof CompletionException && failure.getCause() != null ?
                failure.getCause() : failure;
            recordLogin(proxy, sender, audit.fields().put("refused", cause instanceof GatewayLink.Refused refusedBy ?
                refusedBy.getMessage() : "gateway unavailable: " + cause));
            if (cause instanceof GatewayLink.Refused refused)
            {
                metrics.logonRefused(switch (refused.code)
                {
                    case "UNAUTHORIZED", "FORBIDDEN" -> Metrics.Refusal.CREDENTIALS;
                    case "RATE_LIMITED" -> Metrics.Refusal.RATE_LIMIT;
                    case "BAD_REQUEST" -> Metrics.Refusal.INVALID;
                    case "UNAVAILABLE" -> Metrics.Refusal.GATEWAY_UNAVAILABLE;
                    default -> Metrics.Refusal.OTHER;
                });
                reject(proxy, "UNAUTHORIZED".equals(refused.code) ? "token refused" : refused.getMessage());
            }
            else
            {
                metrics.logonRefused(Metrics.Refusal.GATEWAY_UNAVAILABLE);
                Log.warn("gateway login failed", "sender", sender, "error", cause);
                reject(proxy, "venue gateway unavailable");
            }
        });
    }

    /** Records what became of one Logon: {@code outcome} is the gateway's welcome or the reason it was refused. */
    private void recordLogin(final AbstractAuthenticationProxy proxy, final String sender, final ObjectNode outcome)
    {
        final ObjectNode fields = audit.fields()
            .put("conn", proxy.connectionId())
            .put("sender", sender)
            .put("peer", peer(proxy.remoteAddress()));
        fields.setAll(outcome);
        audit.record(AuditLog.nowMicros(), "gateway-login", fields);
    }

    /** The firm's address as {@code host:port}: Artio gives it the way InetSocketAddress prints, with a leading '/'. */
    private static String peer(final String remoteAddress)
    {
        return remoteAddress != null && remoteAddress.startsWith("/") ? remoteAddress.substring(1) : remoteAddress;
    }

    private static void reject(final AuthenticationProxy proxy, final String text)
    {
        try
        {
            // A Logout with the reason, rather than a silent close, so the firm can tell a bad token from an outage.
            proxy.reject(new LogoutEncoder().text(text), REJECT_LINGER_MS);
        }
        catch (final IllegalStateException alreadyDecided)
        {
            // Artio timed the logon out first; the client has already been disconnected.
        }
    }

    /** The gateway session opened for an accepted logon, handed over exactly once. */
    GatewayLink take(final long connectionId)
    {
        return accepted.remove(connectionId);
    }

    @Override
    public void onDisconnect(final long sessionId, final long connectionId, final DisconnectReason reason)
    {
        audit.record(AuditLog.nowMicros(), "disconnect", audit.fields()
            .put("conn", connectionId).put("session", sessionId).put("reason", String.valueOf(reason)));
        // A connection that went away between being accepted and being handed to the library still has a gateway
        // session open; closing it is what cancels anything it could have left behind.
        final GatewayLink orphan = accepted.remove(connectionId);
        if (orphan != null)
        {
            orphan.close("fix connection closed");
        }
    }
}

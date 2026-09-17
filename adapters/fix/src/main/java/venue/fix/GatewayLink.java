package venue.fix;

import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ObjectNode;

import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.WebSocket;
import java.time.Duration;
import java.util.Queue;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;
import java.util.concurrent.ConcurrentLinkedQueue;
import java.util.concurrent.TimeUnit;

/**
 * One private session on the gateway ({@code /v4/ws_private}), standing in for one FIX session.
 *
 * <p>The JDK's WebSocket client calls back on threads of its own, while everything FIX happens on the adapter's
 * single polling thread. So this class never interprets what the gateway says after login: it parses each frame and
 * leaves it in {@link #inbox} for the polling thread to take, in arrival order. Sends may come from any thread and are
 * chained, because the JDK client refuses a send while the previous one is still being written.
 */
final class GatewayLink implements WebSocket.Listener
{
    static final ObjectMapper JSON = new ObjectMapper();

    /** A login the gateway refused, with its error code (for example {@code UNAUTHORIZED}). */
    static final class Refused extends RuntimeException
    {
        private static final long serialVersionUID = 1L;
        final String code;

        Refused(final String code, final String message)
        {
            super(code + ": " + message);
            this.code = code;
        }
    }

    private final Queue<JsonNode> inbox = new ConcurrentLinkedQueue<>();
    private final CompletableFuture<JsonNode> welcome = new CompletableFuture<>();
    private final StringBuilder partial = new StringBuilder();
    private CompletableFuture<WebSocket> sending;
    private volatile boolean closed;
    private volatile long lastSentNanos = System.nanoTime();
    /** The credential the session logged in with, which the same account's REST calls carry too. */
    private String token;

    private GatewayLink()
    {
    }

    /**
     * Opens a session and logs in. Completes with the link once the gateway has welcomed it, or exceptionally with
     * {@link Refused} when it refuses the login, or with whatever stopped the connection.
     */
    static CompletableFuture<GatewayLink> open(
        final HttpClient http, final URI uri, final ObjectNode login, final Duration timeout)
    {
        final GatewayLink link = new GatewayLink();
        link.token = login.path("token").asText();
        final CompletableFuture<WebSocket> connected =
            http.newWebSocketBuilder().connectTimeout(timeout).buildAsync(uri, link);
        synchronized (link)
        {
            link.sending = connected;
        }
        link.send(login);
        return link.welcome
            .orTimeout(timeout.toMillis(), TimeUnit.MILLISECONDS)
            .handle((w, failure) ->
            {
                if (failure != null)
                {
                    link.close("login failed");
                    throw failure instanceof RuntimeException r ? r : new IllegalStateException(failure);
                }
                return link;
            });
    }

    /** The gateway's welcome: accountId, sessionId and the sequence the session starts from. */
    JsonNode welcome()
    {
        return welcome.getNow(null);
    }

    /** The token the session logged in with, for {@code GET /v4/account} as the same account. */
    String token()
    {
        return token;
    }

    /** The next message the gateway sent after the welcome, or null when there is none waiting. */
    JsonNode poll()
    {
        return inbox.poll();
    }

    boolean isClosed()
    {
        return closed;
    }

    /** Nanoseconds (System.nanoTime) at which something was last handed to the gateway. */
    long lastSentNanos()
    {
        return lastSentNanos;
    }

    void send(final JsonNode message)
    {
        final String text;
        try
        {
            text = JSON.writeValueAsString(message);
        }
        catch (final JsonProcessingException e)
        {
            throw new IllegalStateException(e);
        }
        lastSentNanos = System.nanoTime();
        synchronized (this)
        {
            sending = sending.thenCompose(ws -> ws.sendText(text, true));
            sending.whenComplete((ws, failure) ->
            {
                if (failure != null)
                {
                    closed = true;
                }
            });
        }
    }

    /**
     * Ends the gateway session. Its end is what makes the gateway cancel the session's orders when it logged in with
     * cancelOnDisconnect, so a close that cannot be sent politely is forced.
     */
    void close(final String reason)
    {
        final CompletableFuture<WebSocket> last;
        synchronized (this)
        {
            last = sending;
        }
        last.thenCompose(ws -> ws.sendClose(WebSocket.NORMAL_CLOSURE, reason))
            .orTimeout(2, TimeUnit.SECONDS)
            .whenComplete((ws, failure) ->
            {
                closed = true;
                last.thenAccept(WebSocket::abort);
            });
    }

    @Override
    public CompletionStage<?> onText(final WebSocket ws, final CharSequence data, final boolean last)
    {
        partial.append(data);
        if (last)
        {
            final String text = partial.toString();
            partial.setLength(0);
            try
            {
                final JsonNode message = JSON.readTree(text);
                if (!welcome.isDone())
                {
                    onLoginAnswer(message);
                }
                else
                {
                    inbox.add(message);
                }
            }
            catch (final JsonProcessingException e)
            {
                System.err.println("fix adapter: gateway sent something that is not JSON: " + text);
            }
        }
        ws.request(1);
        return null;
    }

    private void onLoginAnswer(final JsonNode message)
    {
        final String type = message.path("type").asText();
        if ("welcome".equals(type))
        {
            welcome.complete(message);
        }
        else if ("error".equals(type))
        {
            final JsonNode error = message.path("error");
            welcome.completeExceptionally(new Refused(error.path("code").asText(), error.path("message").asText()));
        }
        else
        {
            welcome.completeExceptionally(new IllegalStateException("the gateway answered login with " + type));
        }
    }

    @Override
    public CompletionStage<?> onClose(final WebSocket ws, final int statusCode, final String reason)
    {
        closed = true;
        welcome.completeExceptionally(new IllegalStateException("the gateway closed the session: " + reason));
        return null;
    }

    @Override
    public void onError(final WebSocket ws, final Throwable error)
    {
        closed = true;
        welcome.completeExceptionally(error);
    }
}

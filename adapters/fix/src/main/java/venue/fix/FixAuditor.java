package venue.fix;

import io.aeron.Aeron;
import org.agrona.DirectBuffer;
import org.agrona.concurrent.IdleStrategy;
import org.agrona.concurrent.SleepingMillisIdleStrategy;
import uk.co.real_logic.artio.ArtioLogHeader;
import uk.co.real_logic.artio.CommonConfiguration;
import uk.co.real_logic.artio.engine.EngineConfiguration;
import uk.co.real_logic.artio.engine.logger.FixMessageLogger;
import uk.co.real_logic.artio.messages.FixMessageDecoder;
import uk.co.real_logic.artio.messages.MessageStatus;

import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * Records every FIX message the adapter receives or sends, of every session, in the audit log.
 *
 * <p>The adapter's own code sees only part of a session's traffic: Artio's engine answers Logon, heartbeats, test
 * requests, resends and sequence resets itself, refuses logons before any library has the session, and replays from
 * its archive. What does see everything is the pair of Aeron streams between the engine and its libraries, which every
 * message crosses in one direction or the other: inbound messages from the engine to the libraries, outbound ones from
 * the libraries to the engine, and what the engine sends on its own account (replays, and messages for sessions no
 * library holds) on its replay stream. Artio's {@link FixMessageLogger} subscribes to all three and hands their
 * messages over in timestamp order; this passes each to the audit log.
 *
 * <p>It runs on a thread of its own and touches nothing of the library's, so the polling thread never waits on it. If
 * the audit log's queue fills (a slow disk), this thread waits, its subscriptions stop being read, and back-pressure
 * reaches Artio's engine through Aeron: sessions slow down, and no message goes unrecorded.
 */
final class FixAuditor implements AutoCloseable
{
    private final AuditLog audit;
    private final FixMessageLogger logger;
    private final Thread thread;
    private final CountDownLatch stopped = new CountDownLatch(1);
    private final int inboundStream;
    private volatile boolean running = true;

    FixAuditor(final AuditLog audit, final String aeronDir, final String libraryChannel)
    {
        this.audit = audit;
        this.inboundStream = CommonConfiguration.DEFAULT_INBOUND_LIBRARY_STREAM;
        final FixMessageLogger.Configuration configuration = new FixMessageLogger.Configuration()
            .fixMessageConsumer(this::onMessage)
            .context(new Aeron.Context().aeronDirectoryName(aeronDir))
            .libraryAeronChannel(libraryChannel)
            .inboundStreamId(CommonConfiguration.DEFAULT_INBOUND_LIBRARY_STREAM)
            .outboundStreamId(CommonConfiguration.DEFAULT_OUTBOUND_LIBRARY_STREAM)
            .outboundReplayStreamId(EngineConfiguration.DEFAULT_OUTBOUND_REPLAY_STREAM)
            // Messages wait here only until the other streams have caught up to their timestamps: a megabyte to
            // start with rather than Artio's 256, and room to grow when a burst arrives on one stream.
            .compactionSize(1024 * 1024)
            .maximumBufferSize(64 * 1024 * 1024);
        this.logger = new FixMessageLogger(configuration);
        this.thread = new Thread(this::run, "fix-adapter-audit");
        this.thread.setDaemon(true);
        this.thread.start();
    }

    private void run()
    {
        final IdleStrategy idle = new SleepingMillisIdleStrategy(1);
        try
        {
            while (running)
            {
                idle.idle(logger.doWork());
            }
        }
        catch (final RuntimeException e)
        {
            // Messages would go unrecorded from here on, which is the audit log failing.
            audit.fail("reading FIX messages for the audit log: " + e);
        }
        finally
        {
            stopped.countDown();
        }
    }

    private void onMessage(
        final FixMessageDecoder message, final DirectBuffer buffer, final int offset, final int length,
        final ArtioLogHeader header)
    {
        final MessageStatus status = message.status();
        final long session = message.session();
        final long connection = message.connection();
        final long timestampNanos = message.timestamp();
        final String raw = message.body();
        final var fields = audit.fields()
            .put("dir", header.streamId() == inboundStream ? "in" : "out")
            .put("conn", connection)
            .put("session", session)
            .put("sender", AuditLog.fixField(raw, "49"))
            .put("target", AuditLog.fixField(raw, "56"))
            .put("msgType", AuditLog.fixField(raw, "35"))
            .put("seq", AuditLog.fixField(raw, "34"))
            .put("msg", AuditLog.fixMessage(raw));
        if (status != MessageStatus.OK)
        {
            fields.put("status", String.valueOf(status));
        }
        audit.record(timestampNanos / 1_000, "fix", fields);
    }

    /**
     * Stops once whatever is still on the streams has been recorded. Called after the engine has closed, while the
     * media driver still holds the streams.
     */
    @Override
    public void close()
    {
        running = false;
        try
        {
            stopped.await(10, TimeUnit.SECONDS);
        }
        catch (final InterruptedException e)
        {
            Thread.currentThread().interrupt();
        }
        // A few empty polls in a row: nothing more is arriving.
        for (int empty = 0; empty < 20; )
        {
            if (logger.doWork() == 0)
            {
                empty++;
                try
                {
                    Thread.sleep(5);
                }
                catch (final InterruptedException e)
                {
                    Thread.currentThread().interrupt();
                    break;
                }
            }
            else
            {
                empty = 0;
            }
        }
        // Hands over anything still held for ordering, then closes the Aeron client.
        logger.onClose();
    }
}

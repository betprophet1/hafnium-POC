package venue.fix;

import io.aeron.archive.Archive;
import io.aeron.archive.ArchiveThreadingMode;
import io.aeron.archive.ArchivingMediaDriver;
import io.aeron.archive.client.AeronArchive;
import io.aeron.driver.MediaDriver;
import io.aeron.driver.ThreadingMode;
import io.aeron.logbuffer.ControlledFragmentHandler.Action;
import org.agrona.CloseHelper;
import org.agrona.DirectBuffer;
import org.agrona.concurrent.SleepingMillisIdleStrategy;
import uk.co.real_logic.artio.Reply;
import uk.co.real_logic.artio.builder.Encoder;
import uk.co.real_logic.artio.engine.EngineConfiguration;
import uk.co.real_logic.artio.engine.FixEngine;
import uk.co.real_logic.artio.library.FixLibrary;
import uk.co.real_logic.artio.library.LibraryConfiguration;
import uk.co.real_logic.artio.library.OnMessageInfo;
import uk.co.real_logic.artio.library.SessionConfiguration;
import uk.co.real_logic.artio.library.SessionHandler;
import uk.co.real_logic.artio.messages.DisconnectReason;
import uk.co.real_logic.artio.session.Session;
import venue.fix.codecs.FixDictionaryImpl;

import java.nio.file.Path;
import java.time.Duration;
import java.util.AbstractMap;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.function.BooleanSupplier;
import java.util.function.Predicate;

/**
 * A FIX firm for tests: an Artio initiator with engine and library of its own, as a market maker would run. Artio's
 * library is single-threaded, so the test thread polls it itself while it waits for something.
 */
final class FixClient implements AutoCloseable
{
    private static final String CHANNEL = "aeron:ipc?term-length=8m";
    private static final Duration WAIT = Duration.ofSeconds(20);

    /** A received message: its fields in wire order, repeated tags included. */
    record Message(List<Map.Entry<Integer, String>> fields)
    {
        String get(final int tag)
        {
            return fields.stream().filter(f -> f.getKey() == tag).map(Map.Entry::getValue).findFirst().orElse(null);
        }

        List<String> all(final int tag)
        {
            return fields.stream().filter(f -> f.getKey() == tag).map(Map.Entry::getValue).toList();
        }

        String type()
        {
            return get(35);
        }

        @Override
        public String toString()
        {
            final StringBuilder text = new StringBuilder();
            fields.forEach(f -> text.append(f.getKey()).append('=').append(f.getValue()).append('|'));
            return text.toString();
        }
    }

    private final ArchivingMediaDriver driver;
    private final FixEngine engine;
    private final FixLibrary library;
    private final Map<Long, List<Message>> inboxes = new HashMap<>();
    private final Map<Long, Boolean> disconnected = new HashMap<>();

    FixClient(final Path dir)
    {
        final String aeronDir = dir.resolve("aeron").toString();
        driver = ArchivingMediaDriver.launch(
            new MediaDriver.Context()
                .aeronDirectoryName(aeronDir)
                .threadingMode(ThreadingMode.SHARED)
                .sharedIdleStrategy(new SleepingMillisIdleStrategy(1))
                .ipcTermBufferLength(8 * 1024 * 1024)
                .dirDeleteOnStart(true)
                .dirDeleteOnShutdown(true),
            new Archive.Context()
                .aeronDirectoryName(aeronDir)
                .archiveDir(dir.resolve("archive").toFile())
                .threadingMode(ArchiveThreadingMode.SHARED)
                .idleStrategySupplier(() -> new SleepingMillisIdleStrategy(1))
                .controlChannelEnabled(false)
                .archiveClientContext(new AeronArchive.Context().controlResponseChannel("aeron:ipc"))
                .recordingEventsEnabled(false)
                .replicationChannel("aeron:udp?endpoint=127.0.0.1:0")
                .deleteArchiveOnStart(true));
        final EngineConfiguration configuration = new EngineConfiguration()
            .libraryAeronChannel(CHANNEL)
            .logFileDir(dir.resolve("artio").toString())
            .monitoringFile(dir.resolve("artio-monitoring").toString())
            .deleteLogFileDirOnStart(true)
            // Only initiates, but Artio still wants to know which dictionary an accepted session would speak.
            .acceptorfixDictionary(FixDictionaryImpl.class)
            .framerIdleStrategy(new SleepingMillisIdleStrategy(1))
            .archiverIdleStrategy(new SleepingMillisIdleStrategy(1));
        configuration.aeronContext().aeronDirectoryName(aeronDir);
        configuration.aeronArchiveContext()
            .aeronDirectoryName(aeronDir)
            .controlRequestChannel("aeron:ipc")
            .controlResponseChannel("aeron:ipc");
        engine = FixEngine.launch(configuration);
        final LibraryConfiguration libraryConfiguration = new LibraryConfiguration()
            .libraryAeronChannels(List.of(CHANNEL))
            .sessionAcquireHandler((session, info) -> new Handler());
        libraryConfiguration.aeronContext().aeronDirectoryName(aeronDir);
        library = FixLibrary.connect(libraryConfiguration);
        pollUntil("the client library connects", library::isConnected);
    }

    /**
     * Logs on and returns the session, or null when the venue refused the logon. A persistent session carries its
     * sequence numbers on from its last logon, as a firm does through the day; any other starts again from 1.
     */
    Session logon(final String senderCompId, final String token, final int adapterPort, final boolean persistent)
    {
        return logon(senderCompId, senderCompId, token, adapterPort, persistent);
    }

    /** As {@link #logon(String, String, int, boolean)}, with Username (553) and Password (554) given apart. */
    Session logon(
        final String senderCompId, final String username, final String token, final int adapterPort,
        final boolean persistent)
    {
        final SessionConfiguration.Builder builder = SessionConfiguration.builder()
            .address("127.0.0.1", adapterPort)
            .senderCompId(senderCompId)
            .targetCompId("VENUE")
            .fixDictionary(FixDictionaryImpl.class)
            .resetSeqNum(!persistent)
            .sequenceNumbersPersistent(persistent)
            .timeoutInMs(15_000);
        if (token != null)
        {
            builder.credentials(username, token);
        }
        final Reply<Session> reply = library.initiate(builder.build());
        pollUntil("the logon is answered", () -> !reply.isExecuting());
        if (!reply.hasCompleted())
        {
            return null;
        }
        final Session session = reply.resultIfPresent();
        inboxes.computeIfAbsent(session.id(), id -> new ArrayList<>());
        // A session logging on again has the ID it had before: it is connected until it says otherwise.
        disconnected.remove(session.id());
        return session;
    }

    void send(final Session session, final Encoder message)
    {
        pollUntil("the message is sent", () -> session.trySend(message) > 0);
    }

    /** The first message received on the session that matches, taken out of its inbox. */
    Message expect(final Session session, final String what, final Predicate<Message> matching)
    {
        final List<Message> inbox = inboxes.computeIfAbsent(session.id(), id -> new ArrayList<>());
        final Message[] found = new Message[1];
        try
        {
            pollUntil(what, () ->
            {
                for (int i = 0; i < inbox.size(); i++)
                {
                    if (matching.test(inbox.get(i)))
                    {
                        found[0] = inbox.remove(i);
                        return true;
                    }
                }
                return false;
            });
        }
        catch (final AssertionError e)
        {
            throw new AssertionError(e.getMessage() + "; received instead: " + inbox, e);
        }
        return found[0];
    }

    /** Polls for a moment and returns everything received on the session that matches, without waiting for any. */
    List<Message> drain(final Session session, final Duration forHowLong, final Predicate<Message> matching)
    {
        final long until = System.nanoTime() + forHowLong.toNanos();
        pollUntil("the quiet period ends", () -> System.nanoTime() > until);
        final List<Message> inbox = inboxes.computeIfAbsent(session.id(), id -> new ArrayList<>());
        final List<Message> matched = inbox.stream().filter(matching).toList();
        inbox.removeAll(matched);
        return matched;
    }

    boolean isDisconnected(final Session session)
    {
        return disconnected.getOrDefault(session.id(), false);
    }

    void pollUntil(final String what, final BooleanSupplier done)
    {
        final long deadline = System.nanoTime() + WAIT.toNanos();
        while (!done.getAsBoolean())
        {
            if (System.nanoTime() > deadline)
            {
                throw new AssertionError("timed out waiting until " + what);
            }
            if (library.poll(10) == 0)
            {
                try
                {
                    Thread.sleep(1);
                }
                catch (final InterruptedException e)
                {
                    Thread.currentThread().interrupt();
                    throw new AssertionError("interrupted", e);
                }
            }
        }
    }

    @Override
    public void close()
    {
        CloseHelper.closeAll(library, engine, driver);
    }

    private final class Handler implements SessionHandler
    {
        @Override
        public Action onMessage(
            final DirectBuffer buffer, final int offset, final int length, final int libraryId, final Session session,
            final int sequenceIndex, final long messageType, final long timestampInNs, final long position,
            final OnMessageInfo messageInfo)
        {
            final List<Map.Entry<Integer, String>> fields = new ArrayList<>();
            for (final String field : buffer.getStringWithoutLengthAscii(offset, length).split(""))
            {
                final int equals = field.indexOf('=');
                if (equals > 0)
                {
                    fields.add(new AbstractMap.SimpleEntry<>(
                        Integer.parseInt(field.substring(0, equals)), field.substring(equals + 1)));
                }
            }
            inboxes.computeIfAbsent(session.id(), id -> new ArrayList<>()).add(new Message(fields));
            return Action.CONTINUE;
        }

        @Override
        public void onTimeout(final int libraryId, final Session session)
        {
        }

        @Override
        public void onSlowStatus(final int libraryId, final Session session, final boolean hasBecomeSlow)
        {
        }

        @Override
        public Action onDisconnect(final int libraryId, final Session session, final DisconnectReason reason)
        {
            disconnected.put(session.id(), true);
            return Action.CONTINUE;
        }

        @Override
        public void onSessionStart(final Session session)
        {
        }
    }
}

package venue.fix;

import io.aeron.archive.Archive;
import io.aeron.archive.ArchiveThreadingMode;
import io.aeron.archive.ArchivingMediaDriver;
import io.aeron.archive.client.AeronArchive;
import io.aeron.driver.MediaDriver;
import io.aeron.driver.ThreadingMode;
import org.agrona.CloseHelper;
import org.agrona.concurrent.IdleStrategy;
import org.agrona.concurrent.ShutdownSignalBarrier;
import org.agrona.concurrent.SleepingMillisIdleStrategy;
import uk.co.real_logic.artio.engine.EngineConfiguration;
import uk.co.real_logic.artio.engine.FixEngine;
import uk.co.real_logic.artio.messages.InitialAcceptedSessionOwner;
import uk.co.real_logic.artio.library.FixLibrary;
import uk.co.real_logic.artio.library.LibraryConfiguration;
import uk.co.real_logic.artio.library.SessionHandler;
import uk.co.real_logic.artio.session.Session;
import uk.co.real_logic.artio.validation.MessageValidationStrategy;
import uk.co.real_logic.artio.validation.SessionPersistenceStrategy;
import venue.fix.codecs.FixDictionaryImpl;

import java.net.http.HttpClient;
import java.nio.file.Path;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * The FIX 4.4 adapter (interface 4): an Artio acceptor whose sessions are translated to the gateway's private
 * WebSocket. See README.md beside this project for why it talks to the gateway rather than to the sequencer group.
 *
 * <p>Three pieces run in one process. An embedded Aeron media driver with an archive carries Artio's internal streams
 * and keeps every message sent, which is what resend requests are answered from. Artio's engine owns the TCP
 * connections and the session layer: logon, heartbeats, sequence numbers, resend and gap fill. One Artio library,
 * polled by a single thread here, receives the application messages and holds the translation state; one thread
 * means no locks anywhere in the translation.
 */
public final class FixAdapter implements AutoCloseable
{
    /** Artio's internal library channel. IPC, with small terms: this lane is about correctness, not throughput. */
    private static final String LIBRARY_CHANNEL = "aeron:ipc?term-length=8m";
    private static final int TERM_LENGTH = 8 * 1024 * 1024;

    private final AdapterConfig config;
    private final HttpClient http = HttpClient.newBuilder().connectTimeout(Duration.ofSeconds(5)).build();
    private final GatewayAuthenticator authenticator;
    private final List<VenueSession> sessions = new ArrayList<>();
    private final CountDownLatch stopped = new CountDownLatch(1);
    private volatile boolean running = true;

    private ArchivingMediaDriver driver;
    private FixEngine engine;
    private FixLibrary library;
    private Thread poller;

    private FixAdapter(final AdapterConfig config)
    {
        this.config = config;
        this.authenticator = new GatewayAuthenticator(config, http);
    }

    public static void main(final String[] args)
    {
        final AdapterConfig config;
        try
        {
            config = AdapterConfig.parse(args);
        }
        catch (final IllegalArgumentException e)
        {
            System.err.println(e.getMessage());
            System.exit(64);
            return;
        }
        final FixAdapter adapter = start(config);
        System.err.printf("fix adapter: FIX 4.4 on %s:%d as %s, gateway %s%n",
            config.listenHost(), config.listenPort(), config.compId(), config.gateway());
        try (ShutdownSignalBarrier barrier = new ShutdownSignalBarrier())
        {
            barrier.await();
        }
        finally
        {
            adapter.close();
        }
    }

    /** Starts every piece and returns once FIX clients can log on. {@link #close} stops them all. */
    public static FixAdapter start(final AdapterConfig config)
    {
        final FixAdapter adapter = new FixAdapter(config);
        try
        {
            adapter.launch();
            return adapter;
        }
        catch (final RuntimeException e)
        {
            adapter.close();
            throw e;
        }
    }

    private void launch()
    {
        final Path data = config.dataDir().toAbsolutePath();
        final String aeronDir = data.resolve("aeron").toString();
        // Every idle loop sleeps rather than spins: the adapter shares its machine, and a millisecond is nothing
        // beside the gateway round trip every order makes anyway.
        final MediaDriver.Context driverContext = new MediaDriver.Context()
            .aeronDirectoryName(aeronDir)
            .threadingMode(ThreadingMode.SHARED)
            .sharedIdleStrategy(new SleepingMillisIdleStrategy(1))
            .ipcTermBufferLength(TERM_LENGTH)
            .publicationTermBufferLength(TERM_LENGTH)
            .dirDeleteOnStart(true)
            .dirDeleteOnShutdown(true);
        // The archive is kept across restarts: it holds the messages a client may ask to have resent.
        final Archive.Context archiveContext = new Archive.Context()
            .aeronDirectoryName(aeronDir)
            .archiveDir(data.resolve("archive").toFile())
            .threadingMode(ArchiveThreadingMode.SHARED)
            .idleStrategySupplier(() -> new SleepingMillisIdleStrategy(1))
            .controlChannelEnabled(false)
            .archiveClientContext(new AeronArchive.Context().controlResponseChannel("aeron:ipc"))
            .recordingEventsEnabled(false)
            .replicationChannel("aeron:udp?endpoint=127.0.0.1:0")
            .deleteArchiveOnStart(false);
        driver = ArchivingMediaDriver.launch(driverContext, archiveContext);

        final EngineConfiguration engineConfiguration = new EngineConfiguration()
            .bindTo(config.listenHost(), config.listenPort())
            .libraryAeronChannel(LIBRARY_CHANNEL)
            .logFileDir(data.resolve("artio").toString())
            .monitoringFile(data.resolve("artio-monitoring").toString())
            .acceptorfixDictionary(FixDictionaryImpl.class)
            .authenticationStrategy(authenticator)
            // Clients must address the venue by its CompID.
            .messageValidationStrategy(MessageValidationStrategy.targetCompId(config.compId()))
            // Sequence numbers survive reconnects and restarts; a client starts afresh with ResetSeqNumFlag (141).
            .sessionPersistenceStrategy(SessionPersistenceStrategy.alwaysPersistent())
            // One library takes every session as it logs on, so no session is ever left with the engine.
            .initialAcceptedSessionOwner(InitialAcceptedSessionOwner.SOLE_LIBRARY)
            .framerIdleStrategy(new SleepingMillisIdleStrategy(1))
            .archiverIdleStrategy(new SleepingMillisIdleStrategy(1))
            .authenticationTimeoutInMs(15_000);
        engineConfiguration.aeronContext().aeronDirectoryName(aeronDir);
        engineConfiguration.aeronArchiveContext()
            .aeronDirectoryName(aeronDir)
            .controlRequestChannel("aeron:ipc")
            .controlResponseChannel("aeron:ipc");
        engine = FixEngine.launch(engineConfiguration);

        final LibraryConfiguration libraryConfiguration = new LibraryConfiguration()
            .libraryAeronChannels(List.of(LIBRARY_CHANNEL))
            .sessionAcquireHandler((session, info) -> onAcquired(session))
            .libraryName("venue-fix-adapter");
        libraryConfiguration.aeronContext().aeronDirectoryName(aeronDir);
        library = FixLibrary.connect(libraryConfiguration);
        final long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(30);
        while (!library.isConnected())
        {
            if (System.nanoTime() > deadline)
            {
                throw new IllegalStateException("the FIX library could not connect to its engine");
            }
            library.poll(10);
            Thread.onSpinWait();
        }

        poller = new Thread(this::poll, "fix-adapter-poller");
        poller.start();
    }

    private SessionHandler onAcquired(final Session session)
    {
        final VenueSession venueSession =
            new VenueSession(session, authenticator.take(session.connectionId()), http, config.gateway());
        sessions.add(venueSession);
        System.err.println("fix adapter: " + session.compositeKey() + " logged on");
        return venueSession;
    }

    /** The one thread that touches the library and every session's translation state. */
    private void poll()
    {
        final IdleStrategy idle = new SleepingMillisIdleStrategy(1);
        try
        {
            while (running)
            {
                int work = library.poll(20);
                for (final VenueSession session : sessions)
                {
                    try
                    {
                        work += session.doWork();
                    }
                    catch (final RuntimeException e)
                    {
                        session.fail(e);
                        work++;
                    }
                }
                sessions.removeIf(VenueSession::isEnded);
                idle.idle(work);
            }
        }
        catch (final RuntimeException e)
        {
            System.err.println("fix adapter: poller stopped: " + e);
            e.printStackTrace();
        }
        finally
        {
            stopped.countDown();
        }
    }

    @Override
    public void close()
    {
        running = false;
        if (poller != null)
        {
            try
            {
                stopped.await(10, TimeUnit.SECONDS);
            }
            catch (final InterruptedException e)
            {
                Thread.currentThread().interrupt();
            }
        }
        CloseHelper.closeAll(library, engine, driver);
    }
}

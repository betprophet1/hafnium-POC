package venue.fix;

import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ObjectNode;

import java.io.IOException;
import java.io.RandomAccessFile;
import java.io.UncheckedIOException;
import java.nio.ByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.time.Instant;
import java.time.LocalDate;
import java.time.ZoneOffset;
import java.time.format.DateTimeFormatter;
import java.util.ArrayList;
import java.util.HexFormat;
import java.util.List;
import java.util.Set;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.TimeUnit;
import java.util.function.Consumer;
import java.util.stream.Stream;

/**
 * An edge audit log (interfaces/audit/README.md): one file per UTC day of hash-chained JSON lines, written by a thread
 * of its own so that nothing serving a client waits on the disk.
 *
 * <p>{@link #record} puts a record on a bounded queue and returns. The writer thread takes everything queued, chains
 * and writes each record with one write, and syncs the file once for the lot. A full queue makes the caller wait for
 * room rather than drop a record; room that does not appear within {@link #STALL_SECONDS}, and any failure to write,
 * stops the process (the format document says why).
 */
final class AuditLog implements AutoCloseable
{
    static final String FORMAT = "venue-audit/1";
    static final String ZEROS = "0".repeat(64);
    /** How long a caller waits for room on a full queue before the disk is taken to have hung. */
    static final long STALL_SECONDS = 10;
    /** Records the queue holds: seconds of a busy adapter's traffic, a few megabytes at most. */
    static final int CAPACITY = 65_536;
    /** The exit status of a process whose audit log failed: EX_IOERR. */
    static final int EXIT_IO_ERROR = 74;

    private static final ObjectMapper JSON = new ObjectMapper();
    private static final long MICROS_PER_DAY = 86_400_000_000L;
    private static final DateTimeFormatter TIMESTAMP =
        DateTimeFormatter.ofPattern("uuuu-MM-dd'T'HH:mm:ss.SSSSSS'Z'").withZone(ZoneOffset.UTC);
    private static final int BATCH = 1024;

    private record Entry(long micros, String kind, ObjectNode fields)
    {
    }

    /** Tells the writer thread to close the file and stop. */
    private static final Entry STOP = new Entry(0, "stop", null);

    private final Path dir;
    private final String component;
    private final String instance;
    private final boolean fsync;
    private final Consumer<String> onFailure;
    private final BlockingQueue<Entry> queue = new ArrayBlockingQueue<>(CAPACITY);
    private final Thread writer;

    // Touched by the writer thread only, once started.
    private FileChannel channel;
    private String fileName;
    private long fileDay;
    private long next;
    private String prev;

    private AuditLog(
        final Path dir, final String component, final String instance, final boolean fsync,
        final Consumer<String> onFailure)
    {
        this.dir = dir;
        this.component = component;
        this.instance = sanitise(instance);
        this.fsync = fsync;
        this.onFailure = onFailure;
        this.writer = new Thread(this::write, component + "-audit-writer");
        this.writer.setDaemon(true);
    }

    /**
     * Opens the log in {@code dir}: seals files an earlier process of this component and instance left open, opens a
     * new file, and starts the writer. Throws if the directory or the file cannot be written, so a process that cannot
     * keep its audit log does not start.
     */
    static AuditLog open(final Path dir, final String component, final String instance, final boolean fsync)
    {
        return open(dir, component, instance, fsync, AuditLog::halt);
    }

    static AuditLog open(
        final Path dir, final String component, final String instance, final boolean fsync,
        final Consumer<String> onFailure)
    {
        final AuditLog log = new AuditLog(dir, component, instance, fsync, onFailure);
        try
        {
            Files.createDirectories(dir);
            final long now = nowMicros();
            final ObjectNode after = log.sealEarlierFiles(now);
            log.openFile(now, after);
        }
        catch (final IOException e)
        {
            throw new UncheckedIOException("audit log in " + dir + " cannot be written", e);
        }
        log.writer.start();
        return log;
    }

    /** The instance name as it appears in file names: letters, digits, '-' and '_'. */
    static String sanitise(final String instance)
    {
        final String name = instance == null || instance.isBlank() ? "local" : instance;
        return name.replaceAll("[^A-Za-z0-9_-]", "-");
    }

    /** Wall-clock microseconds since the epoch, for timestamps only. */
    static long nowMicros()
    {
        final Instant now = Instant.now();
        return now.getEpochSecond() * 1_000_000L + now.getNano() / 1_000;
    }

    static String timestamp(final long micros)
    {
        return TIMESTAMP.format(Instant.ofEpochSecond(Math.floorDiv(micros, 1_000_000L),
            Math.floorMod(micros, 1_000_000L) * 1_000L));
    }

    /** The file this log is writing now. */
    Path currentFile()
    {
        synchronized (this)
        {
            return dir.resolve(fileName);
        }
    }

    /**
     * Queues a record of {@code kind} taken at {@code micros}. Waits while the queue is full; if it stays full past
     * {@link #STALL_SECONDS}, the log has failed.
     */
    void record(final long micros, final String kind, final ObjectNode fields)
    {
        final Entry entry = new Entry(micros, kind, fields);
        try
        {
            if (!queue.offer(entry))
            {
                if (!queue.offer(entry, STALL_SECONDS, TimeUnit.SECONDS))
                {
                    onFailure.accept(
                        "the audit queue stayed full for " + STALL_SECONDS + " s: the disk is not taking writes");
                }
            }
        }
        catch (final InterruptedException e)
        {
            Thread.currentThread().interrupt();
            onFailure.accept("interrupted while waiting to queue an audit record");
        }
    }

    /** The log cannot go on: in the adapter, the process stops. */
    void fail(final String reason)
    {
        onFailure.accept(reason);
    }

    ObjectNode fields()
    {
        return JSON.createObjectNode();
    }

    /** Writes everything queued, closes the file with a close record, and stops the writer. */
    @Override
    public void close()
    {
        if (!writer.isAlive())
        {
            return;
        }
        try
        {
            queue.put(STOP);
            writer.join(TimeUnit.SECONDS.toMillis(STALL_SECONDS));
        }
        catch (final InterruptedException e)
        {
            Thread.currentThread().interrupt();
        }
    }

    // ---------------------------------------------------------------------------------------------------------------
    // The writer thread

    private void write()
    {
        final List<Entry> batch = new ArrayList<>(BATCH);
        try
        {
            while (true)
            {
                // Wakes at least once a second, so that a quiet log still rotates soon after midnight.
                final Entry first = queue.poll(1, TimeUnit.SECONDS);
                if (first == null)
                {
                    rotateIfPastMidnight(nowMicros());
                    continue;
                }
                batch.add(first);
                queue.drainTo(batch, BATCH - 1);
                boolean stop = false;
                for (final Entry entry : batch)
                {
                    if (entry == STOP)
                    {
                        stop = true;
                        break;
                    }
                    rotateIfPastMidnight(entry.micros);
                    append(entry.micros, entry.kind, entry.fields);
                }
                batch.clear();
                sync();
                if (stop)
                {
                    closeFile(nowMicros(), "shutdown");
                    return;
                }
            }
        }
        catch (final IOException | RuntimeException e)
        {
            onFailure.accept("audit log write failed: " + e);
        }
        catch (final InterruptedException e)
        {
            onFailure.accept("audit writer interrupted");
        }
    }

    private void rotateIfPastMidnight(final long micros) throws IOException
    {
        final long day = Math.floorDiv(micros, MICROS_PER_DAY);
        if (day > fileDay)
        {
            final String closed = fileName;
            final String last = closeFile(micros, "rotate");
            openFile(micros, JSON.createObjectNode().put("file", closed).put("last", last));
        }
    }

    private void openFile(final long micros, final ObjectNode after) throws IOException
    {
        final long day = Math.floorDiv(micros, MICROS_PER_DAY);
        final String date = LocalDate.ofEpochDay(day).toString();
        final String prefix = component + "." + instance + "." + date + ".";
        int segment = 0;
        for (final Path path : ownFiles())
        {
            final String name = path.getFileName().toString();
            if (name.startsWith(prefix))
            {
                final String digits = name.substring(prefix.length(), name.length() - ".jsonl".length());
                segment = Math.max(segment, Integer.parseInt(digits) + 1);
            }
        }
        final String name = prefix + String.format("%04d", segment) + ".jsonl";
        final FileChannel opened = FileChannel.open(dir.resolve(name),
            StandardOpenOption.CREATE_NEW, StandardOpenOption.WRITE, StandardOpenOption.APPEND);
        synchronized (this)
        {
            channel = opened;
            fileName = name;
        }
        fileDay = day;
        next = 0;
        prev = ZEROS;
        final ObjectNode open = JSON.createObjectNode()
            .put("format", FORMAT).put("file", name).put("pid", ProcessHandle.current().pid());
        if (after != null)
        {
            open.set("after", after);
        }
        append(micros, "open", open);
        sync();
        if (fsync)
        {
            // A new file is durable only once its directory entry is.
            try (FileChannel directory = FileChannel.open(dir, StandardOpenOption.READ))
            {
                directory.force(true);
            }
        }
    }

    /** Closes the current file with a close record, and returns that record's hash. */
    private String closeFile(final long micros, final String reason) throws IOException
    {
        append(micros, "close", JSON.createObjectNode().put("reason", reason).put("records", next));
        sync();
        channel.close();
        return prev;
    }

    private void append(final long micros, final String kind, final ObjectNode fields) throws IOException
    {
        final byte[] line = line(micros, next, component, instance, kind, fields, prev);
        final ByteBuffer buffer = ByteBuffer.allocate(line.length + 1).put(line).put((byte)'\n').flip();
        while (buffer.hasRemaining())
        {
            channel.write(buffer);
        }
        prev = sha256(line);
        next++;
    }

    private void sync() throws IOException
    {
        if (fsync)
        {
            channel.force(false);
        }
    }

    private static byte[] line(
        final long micros, final long n, final String component, final String instance, final String kind,
        final ObjectNode fields, final String prev)
    {
        final ObjectNode record = JSON.createObjectNode()
            .put("ts", timestamp(micros)).put("n", n).put("component", component).put("instance", instance)
            .put("kind", kind);
        if (fields != null)
        {
            record.setAll(fields);
        }
        record.put("prev", prev);
        try
        {
            return JSON.writeValueAsBytes(record);
        }
        catch (final JsonProcessingException e)
        {
            throw new IllegalStateException(e);
        }
    }

    static String sha256(final byte[] bytes)
    {
        try
        {
            return HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(bytes));
        }
        catch (final NoSuchAlgorithmException e)
        {
            throw new IllegalStateException(e);
        }
    }

    // ---------------------------------------------------------------------------------------------------------------
    // Files an earlier process left

    private List<Path> ownFiles() throws IOException
    {
        final String prefix = component + "." + instance + ".";
        try (Stream<Path> paths = Files.list(dir))
        {
            return paths
                .filter(p -> p.getFileName().toString().startsWith(prefix))
                .filter(p -> p.getFileName().toString().matches(".*\\.\\d{4}-\\d{2}-\\d{2}\\.\\d{4}\\.jsonl"))
                .sorted()
                .toList();
        }
    }

    /**
     * Appends a close record to each file of this component and instance that has none and whose last line is whole,
     * and says what the newest of them ended with, for the new file's {@code after}.
     */
    private ObjectNode sealEarlierFiles(final long micros) throws IOException
    {
        ObjectNode after = null;
        for (final Path path : ownFiles())
        {
            final String name = path.getFileName().toString();
            final byte[] last = lastLine(path);
            JsonNode record = null;
            if (last != null)
            {
                try
                {
                    record = JSON.readTree(last);
                }
                catch (final IOException e)
                {
                    record = null;
                }
            }
            if (record == null || !record.path("n").canConvertToLong())
            {
                System.err.println("fix adapter: audit file " + path + " ends in a cut-off record; left as it is");
                after = JSON.createObjectNode().put("file", name).put("sealed", false);
                continue;
            }
            if ("close".equals(record.path("kind").asText()))
            {
                after = JSON.createObjectNode().put("file", name).put("last", sha256(last));
                continue;
            }
            final long n = record.path("n").asLong() + 1;
            final byte[] close = line(micros, n, component, instance, "close",
                JSON.createObjectNode().put("reason", "recovered").put("records", n), sha256(last));
            try (FileChannel file = FileChannel.open(path, StandardOpenOption.WRITE, StandardOpenOption.APPEND))
            {
                final ByteBuffer buffer = ByteBuffer.allocate(close.length + 1).put(close).put((byte)'\n').flip();
                while (buffer.hasRemaining())
                {
                    file.write(buffer);
                }
                if (fsync)
                {
                    file.force(false);
                }
            }
            System.err.println("fix adapter: sealed audit file " + path + ", left open by an earlier process");
            after = JSON.createObjectNode().put("file", name).put("last", sha256(close));
        }
        return after;
    }

    /** A file's last line without its newline, or null when the file is empty or its last line has no newline. */
    static byte[] lastLine(final Path path) throws IOException
    {
        try (RandomAccessFile file = new RandomAccessFile(path.toFile(), "r"))
        {
            final long length = file.length();
            if (length == 0)
            {
                return null;
            }
            file.seek(length - 1);
            if (file.read() != '\n')
            {
                return null;
            }
            long start = length - 1;
            final byte[] chunk = new byte[64 * 1024];
            while (start > 0)
            {
                final int size = (int)Math.min(chunk.length, start);
                file.seek(start - size);
                file.readFully(chunk, 0, size);
                int i = size - 1;
                while (i >= 0 && chunk[i] != '\n')
                {
                    i--;
                }
                if (i >= 0)
                {
                    start = start - size + i + 1;
                    break;
                }
                start -= size;
            }
            final byte[] line = new byte[(int)(length - 1 - start)];
            file.seek(start);
            file.readFully(line);
            return line;
        }
    }

    // ---------------------------------------------------------------------------------------------------------------
    // Checking a file (interfaces/audit/README.md, verifying)

    /** Null when the file verifies, or the first thing wrong with it, naming its line from 1. */
    static String verify(final Path path) throws IOException
    {
        final byte[] data = Files.readAllBytes(path);
        if (data.length == 0 || data[data.length - 1] != '\n')
        {
            return "the file is empty or its last line is cut off";
        }
        String prev = ZEROS;
        int start = 0;
        long n = 0;
        String header = null;
        boolean closed = false;
        for (int i = 0; i < data.length; i++)
        {
            if (data[i] != '\n')
            {
                continue;
            }
            final byte[] line = java.util.Arrays.copyOfRange(data, start, i);
            final String where = "line " + (n + 1) + ": ";
            start = i + 1;
            if (closed)
            {
                return where + "a record after the close record";
            }
            final JsonNode record = JSON.readTree(line);
            if (!prev.equals(record.path("prev").asText()))
            {
                return where + "prev is not the hash of the line before";
            }
            if (record.path("n").asLong(-1) != n)
            {
                return where + "n is not " + n;
            }
            final String owner = record.path("component").asText() + "/" + record.path("instance").asText();
            final String kind = record.path("kind").asText();
            if (n == 0)
            {
                if (!"open".equals(kind) || !FORMAT.equals(record.path("format").asText()) ||
                    !path.getFileName().toString().equals(record.path("file").asText()))
                {
                    return where + "not an open record naming this file";
                }
                header = owner;
            }
            else if (!owner.equals(header))
            {
                return where + "component or instance differs from the open record";
            }
            if ("close".equals(kind))
            {
                if (record.path("records").asLong(-1) != n)
                {
                    return where + "the close record miscounts";
                }
                closed = true;
            }
            prev = sha256(line);
            n++;
        }
        return null;
    }

    // ---------------------------------------------------------------------------------------------------------------
    // Masking (interfaces/audit/README.md, masking)

    /** FIX fields whose values are secrets: Password, NewPassword, EncryptedPassword, EncryptedNewPassword. */
    static final Set<String> SECRET_TAGS = Set.of("554", "925", "1402", "1404");

    /** A FIX message as it is recorded: SOH shown as '|', and secret values replaced by ***. */
    static String fixMessage(final String raw)
    {
        final StringBuilder out = new StringBuilder(raw.length());
        int start = 0;
        while (start < raw.length())
        {
            int end = raw.indexOf('', start);
            final boolean terminated = end >= 0;
            if (!terminated)
            {
                end = raw.length();
            }
            final String field = raw.substring(start, end);
            final int equals = field.indexOf('=');
            if (equals > 0 && SECRET_TAGS.contains(field.substring(0, equals)))
            {
                out.append(field, 0, equals + 1).append("***");
            }
            else
            {
                out.append(field);
            }
            if (terminated)
            {
                out.append('|');
            }
            start = end + 1;
        }
        return out.toString();
    }

    /** A field of a FIX message by tag, or null. */
    static String fixField(final String raw, final String tag)
    {
        final String soh = "";
        final String wanted = tag + "=";
        int start = raw.startsWith(wanted) ? 0 : raw.indexOf(soh + wanted);
        if (start < 0)
        {
            return null;
        }
        if (start > 0 || !raw.startsWith(wanted))
        {
            start += 1;
        }
        start += wanted.length();
        final int end = raw.indexOf(soh, start);
        return raw.substring(start, end < 0 ? raw.length() : end);
    }

    /** What a process does when its audit log fails: says why, and stops at once (the format document says why). */
    private static void halt(final String reason)
    {
        System.err.println("audit log failed, stopping: " + reason);
        System.err.flush();
        Runtime.getRuntime().halt(EXIT_IO_ERROR);
    }
}

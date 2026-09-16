//! Every connection served from a few event loops, rather than from a thread each.
//!
//! A thread per connection could wait on one thing only — its socket — so it could not notice a book change or a
//! reply without waking to look: every connection woke on an interval whether anything had happened or not. A
//! thousand connections doing nothing cost a quarter of a core, and five thousand cost the machine. Here each loop
//! owns its connections through epoll (`mio`) and sleeps until a socket has something, or until the hub or the
//! ingress says a message or a reply is waiting for one of them. An idle connection costs its buffers and nothing
//! else.
//!
//! Anything that tells a loop about a connection — the hub queueing a message, the ingress answering a request —
//! puts the connection's token on that loop's ready list and wakes it, unless a wake is already on its way. A book
//! reaching five thousand subscribers costs each loop one wakeup, not five thousand.
//!
//! A connection arrives undecided. Its first bytes say what it is: a WebSocket upgrade becomes a session (`ws.rs`),
//! anything else one REST request (`rest.rs`), answered — at once, or when the node answers — and closed. This file
//! only reads, writes and waits.

use crate::api::{Gateway, Pending};
use crate::hub::Outbox;
use crate::limits::monotonic;
use crate::ws::{self, Session};
use crate::{http, rest};
use mio::net::TcpStream as Stream;
use mio::{Events, Interest, Poll, Token, Waker};
use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::handshake::server::{NoCallback, ServerHandshake};
use tungstenite::handshake::MidHandshake;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{HandshakeError, Message, WebSocket};
use venue_link::submit::Notify;

/// The token of a loop's own waker, which no connection can have.
const WAKER: Token = Token(usize::MAX);

/// How often a loop checks its connections' heartbeats and deadlines. A missed heartbeat is noticed within this of
/// being missed, and a loop with nothing at all to do wakes this often and no more.
const SWEEP: Duration = Duration::from_millis(100);

/// Passes a connection gets per turn. A pass that answers requests can make room for more, which the next pass reads;
/// the bound stops one busy client holding its loop while others wait.
const PASSES: usize = 8;

/// Bytes read from a socket at a time.
const READ_BUFFER: usize = 4 * 1024;

/// How long a connection has to say what it is, complete a WebSocket handshake, or send a whole REST request. A
/// client that trickles its bytes holds only its buffers while it does, but it does not hold them for ever.
const ARRIVAL: Duration = Duration::from_secs(10);

/// What every WebSocket upgrade request starts with, and nothing else the gateway serves does.
const WS: &[u8] = b"GET /v4/ws";

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What other threads leave for one loop.
struct Shared {
    waker: Waker,
    /// Connections that have something waiting: a message queued for them, or a reply ready.
    ready: Mutex<Vec<usize>>,
    /// Connections accepted for this loop and not yet taken on.
    incoming: Mutex<Vec<TcpStream>>,
    /// Whether a wake is already on its way.
    woken: AtomicBool,
}

impl Shared {
    fn nudge(&self) {
        if !self.woken.swap(true, Ordering::SeqCst) {
            let _ = self.waker.wake();
        }
    }

    fn ready(&self, token: usize) {
        locked(&self.ready).push(token);
        self.nudge();
    }
}

/// The loops, and a way to give one of them a connection.
pub struct Reactors {
    loops: Vec<Arc<Shared>>,
    next: AtomicUsize,
}

impl Reactors {
    /// Starts `threads` loops serving connections for `gateway`. A WebSocket client more than `max_behind` bytes
    /// behind on what it has been sent is disconnected.
    pub fn start(gateway: &Arc<Gateway>, threads: usize, max_behind: usize) -> std::io::Result<Self> {
        let mut loops = Vec::new();
        for _ in 0..threads.max(1) {
            let poll = Poll::new()?;
            let waker = Waker::new(poll.registry(), WAKER)?;
            let shared = Arc::new(Shared {
                waker,
                ready: Mutex::new(Vec::new()),
                incoming: Mutex::new(Vec::new()),
                woken: AtomicBool::new(false),
            });
            let (gateway, own) = (Arc::clone(gateway), Arc::clone(&shared));
            thread::spawn(move || run(poll, &own, &gateway, max_behind));
            loops.push(shared);
        }
        Ok(Self { loops, next: AtomicUsize::new(0) })
    }

    /// Hands a newly accepted connection to one of the loops.
    pub fn adopt(&self, stream: TcpStream) {
        let shared = &self.loops[self.next.fetch_add(1, Ordering::Relaxed) % self.loops.len()];
        locked(&shared.incoming).push(stream);
        shared.nudge();
    }
}

/// One REST request's connection: what has arrived of the request, the node's answer if it went there, and the
/// response while it is being written.
struct Http {
    stream: Stream,
    request: Vec<u8>,
    pending: Option<Pending>,
    response: Vec<u8>,
    written: usize,
}

enum Socket {
    /// Accepted; its first bytes have not said what it is yet.
    Undecided(Stream),
    Handshake(MidHandshake<ServerHandshake<Stream, NoCallback>>),
    Open(WebSocket<Stream>),
    Http(Http),
}

struct Conn {
    socket: Option<Socket>,
    /// Opened once a WebSocket handshake completes; until then the outbox waits here for it.
    session: Option<Session>,
    outbox: Option<Outbox>,
    queue: Receiver<String>,
    notify: Notify,
    /// Whether the socket may hold input not yet read. The loop is told when new input arrives, not that input is
    /// still there, so a session that stopped reading at its limit remembers it has more to read.
    unread: bool,
    /// When the connection was accepted, for `ARRIVAL`.
    accepted: Instant,
}

/// One loop, for ever.
fn run(mut poll: Poll, shared: &Arc<Shared>, gateway: &Arc<Gateway>, max_behind: usize) {
    let mut events = Events::with_capacity(1024);
    let mut conns: Vec<Option<Conn>> = Vec::new();
    let mut free: Vec<usize> = Vec::new();
    let mut swept = monotonic();
    loop {
        if let Err(e) = poll.poll(&mut events, Some(SWEEP)) {
            if e.kind() != ErrorKind::Interrupted {
                eprintln!("gateway: event loop: {e}");
            }
            continue;
        }
        let mut due: Vec<usize> = Vec::new();
        for event in events.iter().filter(|e| e.token() != WAKER) {
            let token = event.token().0;
            // New input: the connection is told it has something to read again, however it stopped last time.
            if event.is_readable() || event.is_read_closed() || event.is_error() {
                if let Some(conn) = conns.get_mut(token).and_then(Option::as_mut) {
                    conn.unread = true;
                }
            }
            due.push(token);
        }
        // Cleared before the lists are taken, so anything added after this point wakes the loop again.
        shared.woken.store(false, Ordering::SeqCst);
        let arrived = std::mem::take(&mut *locked(&shared.incoming));
        for stream in arrived {
            if let Some(token) = take_on(&poll, shared, &mut conns, &mut free, stream) {
                due.push(token);
            }
        }
        due.append(&mut locked(&shared.ready));
        due.sort_unstable();
        due.dedup();
        for token in due {
            let alive = match conns.get_mut(token).and_then(Option::as_mut) {
                Some(conn) => serve(gateway, conn, max_behind),
                None => continue,
            };
            if !alive {
                close_slot(gateway, &mut conns, &mut free, token);
            }
        }
        let now = monotonic();
        if now.duration_since(swept) >= SWEEP {
            swept = now;
            sweep(gateway, &mut conns, &mut free, now);
        }
    }
}

/// How a WebSocket connection's socket buffers.
///
/// tungstenite allocates each connection a 128 KiB read buffer up front, which made an idle connection cost
/// ~130 kB; a client's frames are small, and the buffer grows for one that is not.
///
/// Everything bound for a client waits in its write buffer until the socket takes it, and by default that buffer
/// has no limit, so a client that stopped reading — gone quiet, or simply slow — grew the gateway's memory for as
/// long as it stayed connected. `max_behind` is that limit: past it a write is refused and the connection is
/// dropped (`queue`). It must leave room for tungstenite's own batching, 128 KiB, and a message.
fn socket_config(max_behind: usize) -> WebSocketConfig {
    WebSocketConfig::default().read_buffer_size(READ_BUFFER).max_write_buffer_size(max_behind)
}

/// Registers a newly accepted connection, undecided. None if it could not be taken on.
fn take_on(
    poll: &Poll,
    shared: &Arc<Shared>,
    conns: &mut Vec<Option<Conn>>,
    free: &mut Vec<usize>,
    stream: TcpStream,
) -> Option<usize> {
    stream.set_nonblocking(true).ok()?;
    let _ = stream.set_nodelay(true);
    let mut stream = Stream::from_std(stream);
    let token = free.pop().unwrap_or_else(|| {
        conns.push(None);
        conns.len() - 1
    });
    if poll.registry().register(&mut stream, Token(token), Interest::READABLE | Interest::WRITABLE).is_err() {
        free.push(token);
        return None;
    }
    let notify: Notify = {
        let shared = Arc::clone(shared);
        Arc::new(move || shared.ready(token))
    };
    let (tx, queue) = mpsc::channel();
    conns[token] = Some(Conn {
        socket: Some(Socket::Undecided(stream)),
        session: None,
        outbox: Some(Outbox::new(tx, Arc::clone(&notify))),
        queue,
        notify,
        unread: true,
        accepted: monotonic(),
    });
    Some(token)
}

/// Does everything a connection has waiting, and says whether it is still worth keeping.
fn serve(gateway: &Arc<Gateway>, conn: &mut Conn, max_behind: usize) -> bool {
    if matches!(conn.socket, Some(Socket::Undecided(_))) {
        let Some(Socket::Undecided(stream)) = conn.socket.take() else { return false };
        let mut head = [0u8; WS.len()];
        conn.socket = match stream.peek(&mut head) {
            Ok(0) => return false,
            // A prefix of an upgrade so far: the next bytes decide.
            Ok(n) if n < WS.len() && head[..n] == WS[..n] => {
                conn.socket = Some(Socket::Undecided(stream));
                return true;
            }
            Ok(n) if n == WS.len() && head == *WS => {
                match tungstenite::accept_with_config(stream, Some(socket_config(max_behind))) {
                    Ok(ws) => Some(Socket::Open(ws)),
                    Err(HandshakeError::Interrupted(mid)) => Some(Socket::Handshake(mid)),
                    Err(HandshakeError::Failure(_)) => return false,
                }
            }
            Ok(_) => Some(Socket::Http(Http {
                stream,
                request: Vec::new(),
                pending: None,
                response: Vec::new(),
                written: 0,
            })),
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                conn.socket = Some(Socket::Undecided(stream));
                return true;
            }
            Err(_) => return false,
        };
    }
    if let Some(Socket::Http(request)) = conn.socket.as_mut() {
        return serve_http(gateway, request, &conn.notify);
    }
    serve_ws(gateway, conn)
}

/// A REST connection: read until the request is whole, answer it — now, or when the node has — write the response,
/// and let the connection go.
fn serve_http(gateway: &Gateway, http: &mut Http, notify: &Notify) -> bool {
    if http.pending.is_none() && http.response.is_empty() {
        let mut chunk = [0u8; READ_BUFFER];
        loop {
            match http.stream.read(&mut chunk) {
                // The client stopped sending before its request was whole.
                Ok(0) => return false,
                Ok(n) => http.request.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => return false,
            }
        }
        match http::parse(&http.request) {
            Ok(None) => return true,
            Ok(Some(request)) => match rest::route(gateway, &request, notify) {
                rest::Routed::Now(answer) => http.response = rest::response(answer),
                rest::Routed::Later(pending) => http.pending = Some(pending),
            },
            Err(reason) => http.response = rest::unreadable(&reason),
        }
    }
    if let Some(pending) = &http.pending {
        let Some(answer) = gateway.finish(pending) else { return true };
        http.response = rest::response(answer);
        http.pending = None;
    }
    while http.written < http.response.len() {
        match http.stream.write(&http.response[http.written..]) {
            Ok(0) => return false,
            Ok(n) => http.written += n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return true,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
    // Answered in full: one request a connection, so the connection is done.
    false
}

/// A WebSocket connection: finish its handshake, then read, answer and send.
fn serve_ws(gateway: &Arc<Gateway>, conn: &mut Conn) -> bool {
    if matches!(conn.socket, Some(Socket::Handshake(_))) {
        let Some(Socket::Handshake(mid)) = conn.socket.take() else { return false };
        match mid.handshake() {
            Ok(ws) => conn.socket = Some(Socket::Open(ws)),
            Err(HandshakeError::Interrupted(mid)) => {
                conn.socket = Some(Socket::Handshake(mid));
                return true;
            }
            Err(HandshakeError::Failure(_)) => return false,
        }
    }
    let Some(Socket::Open(socket)) = conn.socket.as_mut() else { return false };
    if conn.session.is_none() {
        let Some(outbox) = conn.outbox.take() else { return false };
        conn.session = Session::open(gateway, outbox, Arc::clone(&conn.notify));
    }
    let Some(session) = conn.session.as_mut() else { return false };
    for _ in 0..PASSES {
        let mut answered = false;
        // What the client sent, until there is nothing more or the session may take no more.
        while conn.unread && !session.at_limit() {
            match socket.read() {
                Ok(Message::Text(text)) => {
                    session.heard();
                    for reply in ws::handle(gateway, session, text.as_str()) {
                        if !queue(socket, reply) {
                            return false;
                        }
                    }
                }
                Ok(Message::Binary(_)) => {
                    if !queue(socket, ws::binary_refused()) {
                        return false;
                    }
                }
                Ok(Message::Close(_)) => return false,
                Ok(_) => session.heard(),
                Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => conn.unread = false,
                Err(_) => return false,
            }
        }
        // Replies in the order they were asked for.
        for reply in ws::ready_replies(gateway, session) {
            answered = true;
            if !queue(socket, reply) {
                return false;
            }
        }
        // Messages the hub queued.
        while let Ok(message) = conn.queue.try_recv() {
            if !queue(socket, message) {
                return false;
            }
        }
        // Another pass only if answering made room for input still waiting.
        if !(answered && conn.unread) {
            break;
        }
    }
    flushed(socket)
}

/// Closes the connections of logged-in clients silent past their heartbeat, and of clients that have not managed to
/// say what they want within `ARRIVAL`.
fn sweep(gateway: &Arc<Gateway>, conns: &mut [Option<Conn>], free: &mut Vec<usize>, now: Instant) {
    for token in 0..conns.len() {
        let Some(conn) = conns[token].as_mut() else { continue };
        let arriving = match &conn.socket {
            Some(Socket::Undecided(_) | Socket::Handshake(_)) => true,
            Some(Socket::Http(http)) => http.pending.is_none() && http.response.is_empty(),
            _ => false,
        };
        if arriving {
            if now.duration_since(conn.accepted) > ARRIVAL {
                close_slot(gateway, conns, free, token);
            }
            continue;
        }
        let Some(session) = conn.session.as_mut() else { continue };
        if !session.heartbeat_missed(now) {
            continue;
        }
        ws::heartbeat_expired(gateway, session);
        if let Some(Socket::Open(socket)) = conn.socket.as_mut() {
            let _ = socket.close(None);
            let _ = socket.flush();
        }
        close_slot(gateway, conns, free, token);
    }
}

/// Forgets a connection: dropping its socket closes it and takes it out of the loop, and its session ends.
fn close_slot(gateway: &Arc<Gateway>, conns: &mut [Option<Conn>], free: &mut Vec<usize>, token: usize) {
    if let Some(conn) = conns[token].take() {
        if let Some(session) = &conn.session {
            ws::end(gateway, session);
        }
        free.push(token);
    }
}

/// Queues a message for the client, and says whether the connection is still worth keeping. A would-block is not a
/// failure: the message stays in the socket's own buffer and goes out on a later flush, when the loop is told the
/// socket can take more.
fn queue(socket: &mut WebSocket<Stream>, message: String) -> bool {
    match socket.write(Message::text(message)) {
        Ok(()) => true,
        Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => true,
        // A slow consumer: further behind than a connection may be (`socket_config`). Dropped rather than buffered
        // for without limit; a client that reconnects starts again from a fresh image.
        Err(tungstenite::Error::WriteBufferFull(_)) => {
            let peer = socket.get_ref().peer_addr().map_or_else(|_| "a client".to_string(), |a| a.to_string());
            eprintln!("gateway: disconnected {peer}: too far behind on what it has been sent");
            false
        }
        Err(_) => false,
    }
}

/// Pushes what is queued towards the client, tolerating a would-block for the same reason.
fn flushed(socket: &mut WebSocket<Stream>) -> bool {
    match socket.flush() {
        Ok(()) => true,
        Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => true,
        Err(_) => false,
    }
}

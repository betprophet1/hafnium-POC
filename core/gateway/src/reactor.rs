//! WebSocket connections served from a few event loops, rather than from a thread each.
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
//! The protocol is `ws.rs`; this only reads, writes and waits.

use crate::api::Gateway;
use crate::hub::Outbox;
use crate::limits::monotonic;
use crate::ws::{self, Session};
use mio::net::TcpStream as Stream;
use mio::{Events, Interest, Poll, Token, Waker};
use std::io::ErrorKind;
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

/// How often a loop checks its connections' heartbeats. A missed heartbeat is noticed within this of being missed,
/// and a loop with nothing at all to do wakes this often and no more.
const SWEEP: Duration = Duration::from_millis(100);

/// Passes a connection gets per turn. A pass that answers requests can make room for more, which the next pass reads;
/// the bound stops one busy client holding its loop while others wait.
const PASSES: usize = 8;

/// Bytes read from a socket at a time.
const READ_BUFFER: usize = 4 * 1024;

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
    /// Starts `threads` loops serving connections for `gateway`.
    pub fn start(gateway: &Arc<Gateway>, threads: usize) -> std::io::Result<Self> {
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
            thread::spawn(move || run(poll, &own, &gateway));
            loops.push(shared);
        }
        Ok(Self { loops, next: AtomicUsize::new(0) })
    }

    /// Hands a connection whose request is a WebSocket upgrade to one of the loops, which does the handshake.
    pub fn adopt(&self, stream: TcpStream) {
        let shared = &self.loops[self.next.fetch_add(1, Ordering::Relaxed) % self.loops.len()];
        locked(&shared.incoming).push(stream);
        shared.nudge();
    }
}

enum Socket {
    Handshake(MidHandshake<ServerHandshake<Stream, NoCallback>>),
    Open(WebSocket<Stream>),
}

struct Conn {
    socket: Option<Socket>,
    /// Opened once the handshake completes; until then the outbox waits here for it.
    session: Option<Session>,
    outbox: Option<Outbox>,
    queue: Receiver<String>,
    notify: Notify,
    /// Whether the socket may hold input not yet read. The loop is told when new input arrives, not that input is
    /// still there, so a session that stopped reading at its limit remembers it has more to read.
    unread: bool,
}

/// One loop, for ever.
fn run(mut poll: Poll, shared: &Arc<Shared>, gateway: &Arc<Gateway>) {
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
                Some(conn) => serve(gateway, conn),
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

/// How a connection's socket buffers. tungstenite allocates each connection a 128 KiB read buffer up front, which
/// made an idle connection cost ~130 kB; a client's frames are small, and the buffer grows for one that is not.
fn socket_config() -> WebSocketConfig {
    WebSocketConfig::default().read_buffer_size(READ_BUFFER)
}

/// Registers a new connection and starts its handshake. None if it could not be taken on.
fn take_on(
    poll: &Poll,
    shared: &Arc<Shared>,
    conns: &mut Vec<Option<Conn>>,
    free: &mut Vec<usize>,
    stream: TcpStream,
) -> Option<usize> {
    stream.set_nonblocking(true).ok()?;
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(None);
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
    let socket = match tungstenite::accept_with_config(stream, Some(socket_config())) {
        Ok(ws) => Socket::Open(ws),
        Err(HandshakeError::Interrupted(mid)) => Socket::Handshake(mid),
        Err(HandshakeError::Failure(_)) => {
            free.push(token);
            return None;
        }
    };
    conns[token] = Some(Conn {
        socket: Some(socket),
        session: None,
        outbox: Some(Outbox::new(tx, Arc::clone(&notify))),
        queue,
        notify,
        unread: true,
    });
    Some(token)
}

/// Does everything a connection has waiting, and says whether it is still worth keeping.
fn serve(gateway: &Arc<Gateway>, conn: &mut Conn) -> bool {
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

/// Closes the connections of logged-in clients silent past their heartbeat.
fn sweep(gateway: &Arc<Gateway>, conns: &mut [Option<Conn>], free: &mut Vec<usize>, now: Instant) {
    for token in 0..conns.len() {
        let Some(conn) = conns[token].as_mut() else { continue };
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

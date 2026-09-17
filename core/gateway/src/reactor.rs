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
//! Each half of interface 1, public and private, has loops of its own, so that fanning market data out to every viewer
//! never holds up an order's answer. A connection arrives undecided, and its request line says what it is: a path of
//! the other half goes to that half's loops, a WebSocket upgrade becomes a session (`ws.rs`), and anything else one
//! REST request (`rest.rs`), answered — at once, or when the node answers — and closed. This file only reads, writes
//! and waits.
//!
//! It is also where what a client said and was told is taken for the audit log (`audit.rs`,
//! interfaces/audit/README.md): each REST request and its response, and a WebSocket's upgrade, frames and end. Taking a
//! record only queues it; nothing here waits on the disk.

use crate::api::{Gateway, Half, Pending};
use crate::audit::{self, Fields};
use crate::forwarded::Forwarding;
use crate::hub::{Derived, Outbox, Queued};
use crate::identity::Credential;
use crate::limits::monotonic;
use crate::treasury::Call;
use crate::ws::{self, Session};
use crate::{http, rest};
use mio::net::TcpStream as Stream;
use mio::{Events, Interest, Poll, Registry, Token, Waker};
use serde_json::{json, Map, Value};
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::handshake::server::{Callback, ErrorResponse, Request, Response, ServerHandshake};
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

/// Connection numbers for the audit log, unique across both halves' loops within a process.
static CONNECTIONS: AtomicU64 = AtomicU64::new(1);

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

/// One half's loops, and a way to give one of them a connection.
pub struct Reactors {
    loops: Vec<Arc<Shared>>,
    next: AtomicUsize,
    /// The other half's loops, when this process serves both halves: a connection for that half is handed to them.
    other: OnceLock<Arc<Reactors>>,
}

impl Reactors {
    /// Starts `threads` loops serving connections for `gateway`'s half. A WebSocket client more than `max_behind`
    /// bytes behind on what it has been sent is disconnected.
    pub fn start(gateway: &Arc<Gateway>, threads: usize, max_behind: usize) -> std::io::Result<Arc<Self>> {
        let (mut polls, mut loops) = (Vec::new(), Vec::new());
        for _ in 0..threads.max(1) {
            let poll = Poll::new()?;
            let waker = Waker::new(poll.registry(), WAKER)?;
            loops.push(Arc::new(Shared {
                waker,
                ready: Mutex::new(Vec::new()),
                incoming: Mutex::new(Vec::new()),
                woken: AtomicBool::new(false),
            }));
            polls.push(poll);
        }
        let reactors = Arc::new(Self { loops, next: AtomicUsize::new(0), other: OnceLock::new() });
        for (index, (poll, shared)) in polls.into_iter().zip(&reactors.loops).enumerate() {
            let (gateway, own, reactors) = (Arc::clone(gateway), Arc::clone(shared), Arc::clone(&reactors));
            let name = format!("gw-{}-{index}", gateway.half.name());
            thread::Builder::new().name(name).spawn(move || run(poll, &own, &reactors, &gateway, max_behind))?;
        }
        Ok(reactors)
    }

    /// Joins the loops of the two halves, so each hands the other the connections that belong to it.
    pub fn join(public: &Arc<Self>, private: &Arc<Self>) {
        let _ = public.other.set(Arc::clone(private));
        let _ = private.other.set(Arc::clone(public));
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
    /// A request passed to the treasury, until its response is there.
    call: Option<Call>,
    response: Vec<u8>,
    written: usize,
    /// The credential the request's token is, for the audit log: its account and, for an API key, the key.
    who: Option<Credential>,
    /// Whether the response has been recorded in the audit log.
    recorded: bool,
}

enum Socket {
    /// Accepted; its first bytes have not said what it is yet.
    Undecided(Stream),
    Handshake(MidHandshake<ServerHandshake<Stream, Upgrade>>),
    Open(WebSocket<Stream>),
    Http(Http),
}

struct Conn {
    socket: Option<Socket>,
    /// Opened once a WebSocket handshake completes; until then the outbox waits here for it.
    session: Option<Session>,
    outbox: Option<Outbox>,
    queue: Receiver<Queued>,
    notify: Notify,
    /// Whether the socket may hold input not yet read. The loop is told when new input arrives, not that input is
    /// still there, so a session that stopped reading at its limit remembers it has more to read.
    unread: bool,
    /// When the connection was accepted, for `ARRIVAL`.
    accepted: Instant,
    /// Who the audit log says a record is about.
    origin: Origin,
    /// The forwarding headers of a WebSocket's upgrade request, once read, when it came from a trusted proxy.
    upgrade: Option<Arc<OnceLock<Forwarding>>>,
    /// The path of a WebSocket's upgrade request.
    path: String,
    /// Why the connection ended, for the audit log, when something other than the client or the network ended it.
    ending: &'static str,
}

/// One loop, for ever.
fn run(mut poll: Poll, shared: &Arc<Shared>, reactors: &Reactors, gateway: &Arc<Gateway>, max_behind: usize) {
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
                Some(conn) => serve(gateway, conn, max_behind, poll.registry(), reactors),
                None => continue,
            };
            if !alive {
                close_slot(gateway, &mut conns, &mut free, token, None);
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
    let address = stream.peer_addr().ok();
    let peer = address.map_or_else(|| "unknown".to_string(), |a| a.to_string());
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
        origin: Origin {
            id: CONNECTIONS.fetch_add(1, Ordering::Relaxed),
            peer,
            ip: address.map(|a| a.ip()),
            client: None,
        },
        upgrade: None,
        path: String::new(),
        ending: "closed",
    });
    Some(token)
}

/// Does everything a connection has waiting, and says whether it is still worth keeping.
fn serve(gateway: &Arc<Gateway>, conn: &mut Conn, max_behind: usize, registry: &Registry, reactors: &Reactors) -> bool {
    if matches!(conn.socket, Some(Socket::Undecided(_))) {
        match decide(gateway, conn, max_behind, registry, reactors) {
            Decided::Waiting => return true,
            Decided::Gone => return false,
            Decided::Here => {}
        }
    }
    if let Some(Socket::Http(request)) = conn.socket.as_mut() {
        return serve_http(gateway, request, &conn.notify, &mut conn.origin);
    }
    serve_ws(gateway, conn)
}

enum Decided {
    /// Not all of its request line has arrived.
    Waiting,
    /// Closed, refused, or handed to the other half's loops.
    Gone,
    /// Served by this loop from now on.
    Here,
}

/// Settles what a new connection is from its request line, read without taking it from the socket so that whatever
/// serves the connection reads the request whole. This half's WebSocket upgrade becomes a session. A path of the other
/// half goes to that half's loops if this process serves it; if not, it is a REST request here, which is refused as a
/// route this half does not have. Anything else is one REST request.
fn decide(gateway: &Gateway, conn: &mut Conn, max_behind: usize, registry: &Registry, reactors: &Reactors) -> Decided {
    let Some(Socket::Undecided(mut stream)) = conn.socket.take() else { return Decided::Gone };
    let mut head = [0u8; http::MAX_LINE];
    let n = match stream.peek(&mut head) {
        Ok(0) => return Decided::Gone,
        Ok(n) => n,
        Err(e) if e.kind() == ErrorKind::WouldBlock => {
            conn.socket = Some(Socket::Undecided(stream));
            return Decided::Waiting;
        }
        Err(_) => return Decided::Gone,
    };
    let line = match head[..n].iter().position(|&b| b == b'\n') {
        Some(end) => &head[..end],
        None if n < head.len() => {
            conn.socket = Some(Socket::Undecided(stream));
            return Decided::Waiting;
        }
        // Longer than a request line may be: a REST request, which is refused as one.
        None => &head[..0],
    };
    let line = String::from_utf8_lossy(line);
    let mut words = line.split_whitespace();
    let method = words.next().unwrap_or("");
    let path = words.next().unwrap_or("").split('?').next().unwrap_or("");
    if let (Some(half), Some(other)) = (Half::of(path), reactors.other.get()) {
        if half != gateway.half {
            let _ = registry.deregister(&mut stream);
            other.adopt(TcpStream::from(stream));
            return Decided::Gone;
        }
    }
    conn.socket = Some(if method == "GET" && path == gateway.half.websocket() {
        conn.path = path.to_string();
        // Its headers are read by the handshake; the forwarding ones are kept only from a trusted proxy.
        conn.upgrade = gateway.proxies.trusts_peer(conn.origin.ip).then(|| Arc::new(OnceLock::new()));
        let upgrade = Upgrade(conn.upgrade.clone());
        match tungstenite::accept_hdr_with_config(stream, upgrade, Some(socket_config(max_behind))) {
            Ok(ws) => Socket::Open(ws),
            Err(HandshakeError::Interrupted(mid)) => Socket::Handshake(mid),
            Err(HandshakeError::Failure(_)) => {
                record_refused_upgrade(gateway, conn);
                return Decided::Gone;
            }
        }
    } else {
        Socket::Http(Http {
            stream,
            request: Vec::new(),
            pending: None,
            call: None,
            response: Vec::new(),
            written: 0,
            who: None,
            recorded: false,
        })
    });
    Decided::Here
}

/// A REST connection: read until the request is whole, answer it — now, or when the node has — write the response,
/// and let the connection go.
fn serve_http(gateway: &Gateway, http: &mut Http, notify: &Notify, origin: &mut Origin) -> bool {
    if http.pending.is_none() && http.call.is_none() && http.response.is_empty() {
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
            Ok(Some(request)) => {
                http.who = request
                    .headers
                    .get("authorization")
                    .and_then(|h| h.strip_prefix("Bearer "))
                    .and_then(|token| gateway.authenticate(token.trim()).ok());
                let forwarding = Forwarding {
                    x_forwarded_for: request.headers.get("x-forwarded-for").cloned(),
                    forwarded: request.headers.get("forwarded").cloned(),
                };
                origin.client = gateway.proxies.client(origin.ip, &forwarding);
                record_request(gateway, origin, http, &request);
                match rest::route(gateway, &request, notify) {
                    rest::Routed::Now(answer) => http.response = rest::response(answer),
                    rest::Routed::Later(pending) => http.pending = Some(pending),
                    rest::Routed::Service(call) => http.call = Some(call),
                }
            }
            Err(reason) => {
                let mut fields = about(gateway, origin, None, None, None);
                fields.put("dir", "in");
                fields.put("line", request_line(&http.request));
                fields.put("unreadable", reason.clone());
                gateway.audit.record("http-request", fields);
                http.response = rest::unreadable(&reason);
            }
        }
    }
    if let Some(pending) = &http.pending {
        let Some(answer) = gateway.finish(pending) else { return true };
        http.response = rest::response(answer);
        http.pending = None;
    }
    if let Some(call) = &http.call {
        let Some(response) = call.take() else { return true };
        http.response = response;
        http.call = None;
    }
    if !http.recorded {
        http.recorded = true;
        record_response(gateway, origin, http);
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
            Err(HandshakeError::Failure(_)) => {
                record_refused_upgrade(gateway, conn);
                return false;
            }
        }
    }
    let Some(Socket::Open(socket)) = conn.socket.as_mut() else { return false };
    if conn.session.is_none() {
        let Some(outbox) = conn.outbox.take() else { return false };
        conn.session = Session::open(gateway, outbox, Arc::clone(&conn.notify));
        if let Some(session) = &conn.session {
            forwarded_client(gateway, &mut conn.origin, conn.upgrade.take());
            let mut fields = about(gateway, &conn.origin, None, None, Some(session.id()));
            fields.put("path", conn.path.clone());
            gateway.audit.record("ws-open", fields);
        }
    }
    let Some(session) = conn.session.as_mut() else { return false };
    let (origin, ending) = (&conn.origin, &mut conn.ending);
    for _ in 0..PASSES {
        let mut answered = false;
        // What the client sent, until there is nothing more or the session may take no more.
        while conn.unread && !session.at_limit() {
            match socket.read() {
                Ok(Message::Text(text)) => {
                    session.heard();
                    record_from_client(gateway, origin, session, text.as_str());
                    for reply in ws::handle(gateway, session, text.as_str()) {
                        record_to_client(gateway, origin, session, &reply, true);
                        if !queue(socket, reply, ending) {
                            return false;
                        }
                    }
                }
                Ok(Message::Binary(data)) => {
                    let mut fields = about(gateway, origin, session.account(), session.api_key(), Some(session.id()));
                    fields.put("dir", "in");
                    fields.put("binary", data.len());
                    gateway.audit.record("ws-frame", fields);
                    let refused = ws::binary_refused();
                    record_to_client(gateway, origin, session, &refused, true);
                    if !queue(socket, refused, ending) {
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
            record_to_client(gateway, origin, session, &reply, true);
            if !queue(socket, reply, ending) {
                return false;
            }
        }
        // Messages the hub queued.
        while let Ok(message) = conn.queue.try_recv() {
            record_queued(gateway, origin, session, &message);
            if !queue(socket, message.text, ending) {
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

/// Closes the connections of logged-in clients silent past their heartbeat or whose API key has been revoked or has
/// expired, and of clients that have not managed to say what they want within `ARRIVAL`.
fn sweep(gateway: &Arc<Gateway>, conns: &mut [Option<Conn>], free: &mut Vec<usize>, now: Instant) {
    for token in 0..conns.len() {
        let Some(conn) = conns[token].as_mut() else { continue };
        let arriving = match &conn.socket {
            Some(Socket::Undecided(_) | Socket::Handshake(_)) => true,
            Some(Socket::Http(http)) => http.pending.is_none() && http.call.is_none() && http.response.is_empty(),
            _ => false,
        };
        if arriving {
            if now.duration_since(conn.accepted) > ARRIVAL {
                close_slot(gateway, conns, free, token, None);
            }
            continue;
        }
        let Some(session) = conn.session.as_mut() else { continue };
        if ws::key_withdrawn(gateway, session) {
            let told = ws::key_withdrawn_error();
            record_to_client(gateway, &conn.origin, session, &told, true);
            if let Some(Socket::Open(socket)) = conn.socket.as_mut() {
                let _ = socket.write(Message::text(told));
                let _ = socket.close(None);
                let _ = socket.flush();
            }
            close_slot(gateway, conns, free, token, Some("key no longer valid"));
            continue;
        }
        if !session.heartbeat_missed(now) {
            continue;
        }
        ws::heartbeat_expired(gateway, session);
        if let Some(Socket::Open(socket)) = conn.socket.as_mut() {
            let _ = socket.close(None);
            let _ = socket.flush();
        }
        close_slot(gateway, conns, free, token, Some("heartbeat missed"));
    }
}

/// Forgets a connection: dropping its socket closes it and takes it out of the loop, and its session ends. `reason`
/// is why, for the audit log, when the connection does not already know.
fn close_slot(
    gateway: &Arc<Gateway>,
    conns: &mut [Option<Conn>],
    free: &mut Vec<usize>,
    token: usize,
    reason: Option<&'static str>,
) {
    if let Some(conn) = conns[token].take() {
        if let Some(session) = &conn.session {
            let mut fields = about(gateway, &conn.origin, session.account(), session.api_key(), Some(session.id()));
            fields.put("reason", reason.unwrap_or(conn.ending));
            gateway.audit.record("ws-close", fields);
            ws::end(gateway, session);
        }
        free.push(token);
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// The audit log (interfaces/audit/README.md, gateway)

/// Who the records of one connection are about.
struct Origin {
    /// The connection's number.
    id: u64,
    /// The address it came from, as `address:port`: behind a proxy, the proxy.
    peer: String,
    ip: Option<IpAddr>,
    /// The client a trusted proxy said it forwarded the connection's request for (`forwarded.rs`), once read.
    client: Option<String>,
}

/// Takes a WebSocket upgrade request's forwarding headers as the handshake reads them, when there is somewhere to
/// keep them: only a connection from a trusted proxy has.
struct Upgrade(Option<Arc<OnceLock<Forwarding>>>);

impl Callback for Upgrade {
    fn on_request(self, request: &Request, response: Response) -> Result<Response, ErrorResponse> {
        if let Some(seen) = self.0 {
            let every = |name: &str| {
                let values: Vec<&str> =
                    request.headers().get_all(name).iter().filter_map(|v| v.to_str().ok()).collect();
                (!values.is_empty()).then(|| values.join(", "))
            };
            let _ = seen.set(Forwarding { x_forwarded_for: every("x-forwarded-for"), forwarded: every("forwarded") });
        }
        Ok(response)
    }
}

/// Settles the client of a WebSocket from its upgrade request's forwarding headers, if they were kept.
fn forwarded_client(gateway: &Gateway, origin: &mut Origin, upgrade: Option<Arc<OnceLock<Forwarding>>>) {
    if let Some(forwarding) = upgrade.as_deref().and_then(OnceLock::get) {
        origin.client = gateway.proxies.client(origin.ip, forwarding);
    }
}

/// The fields that say which connection a record is about.
fn about(gateway: &Gateway, origin: &Origin, account: Option<u64>, key: Option<&str>, session: Option<u64>) -> Fields {
    let mut fields = Fields::new();
    fields.put("conn", origin.id);
    fields.put("peer", origin.peer.clone());
    // Only when a trusted proxy named one: otherwise the peer is the client.
    if let Some(client) = &origin.client {
        fields.put("client", client.clone());
    }
    fields.put("half", gateway.half.name());
    if let Some(account) = account {
        fields.put("account", account.to_string());
    }
    // The key's ID, never its secret, so an auditor can tell which of an account's keys did something.
    if let Some(key) = key {
        fields.put("key", key);
    }
    if let Some(session) = session {
        fields.put("session", session.to_string());
    }
    fields
}

/// The request line of what has arrived of a request, as text, however much of it there is.
fn request_line(request: &[u8]) -> String {
    let line = request.split(|&b| b == b'\n').next().unwrap_or(&[]);
    let line = &line[..line.len().min(http::MAX_LINE)];
    String::from_utf8_lossy(line).trim_end_matches('\r').to_string()
}

fn record_request(gateway: &Gateway, origin: &Origin, http: &Http, request: &http::Request) {
    let who = http.who.as_ref();
    let mut fields = about(gateway, origin, who.map(|c| c.account), who.and_then(|c| c.key.as_deref()), None);
    fields.put("dir", "in");
    fields.put("line", request_line(&http.request));
    let headers: Map<String, Value> =
        request.headers.iter().map(|(name, value)| (name.clone(), json!(audit::header_value(name, value)))).collect();
    fields.put("headers", Value::Object(headers));
    fields.wire("body", &String::from_utf8_lossy(&request.body));
    gateway.audit.record("http-request", fields);
}

fn record_response(gateway: &Gateway, origin: &Origin, http: &Http) {
    let response = &http.response;
    let status = response.get(9..12).and_then(|s| std::str::from_utf8(s).ok()).and_then(|s| s.parse::<u16>().ok());
    let body = response.windows(4).position(|w| w == b"\r\n\r\n").map_or(&response[..0], |i| &response[i + 4..]);
    let who = http.who.as_ref();
    let mut fields = about(gateway, origin, who.map(|c| c.account), who.and_then(|c| c.key.as_deref()), None);
    fields.put("dir", "out");
    fields.put("status", status);
    if gateway.half == Half::Public && status.is_some_and(|s| s < 400) {
        // What the public half answers is market data: the same for every client, reconstructable from the journal,
        // and polled all day, so its bytes would fill the log. The length and hash of what was sent still say what the
        // answer was; a refusal or an error keeps its body.
        fields.put("length", body.len());
        fields.put("sha256", audit::sha256_hex(body));
    } else {
        // Masked too: the answer that creates an API key carries its secret.
        fields.wire("body", &String::from_utf8_lossy(body));
    }
    gateway.audit.record("http-response", fields);
}

fn record_refused_upgrade(gateway: &Gateway, conn: &mut Conn) {
    forwarded_client(gateway, &mut conn.origin, conn.upgrade.take());
    let mut fields = about(gateway, &conn.origin, None, None, None);
    fields.put("path", conn.path.clone());
    gateway.audit.record("ws-refused", fields);
}

/// A text frame from the client. On the public half a heartbeat is not recorded: viewers send them all day, and
/// nothing about one can be in dispute.
fn record_from_client(gateway: &Gateway, origin: &Origin, session: &Session, text: &str) {
    if gateway.half == Half::Public
        && serde_json::from_str::<Value>(text).is_ok_and(|m| m.get("op").and_then(Value::as_str) == Some("heartbeat"))
    {
        return;
    }
    let mut fields = about(gateway, origin, session.account(), session.api_key(), Some(session.id()));
    fields.put("dir", "in");
    fields.wire("msg", text);
    gateway.audit.record("ws-frame", fields);
}

/// A frame for the client: on the private half every one; on the public half only the errors that answer what the
/// client sent, never the market data the hub queues (`answer` says which this is).
fn record_to_client(gateway: &Gateway, origin: &Origin, session: &Session, text: &str, answer: bool) {
    if gateway.half == Half::Public
        && !(answer && serde_json::from_str::<Value>(text).is_ok_and(|m| m.get("type") == Some(&json!("error"))))
    {
        return;
    }
    let mut fields = about(gateway, origin, session.account(), session.api_key(), Some(session.id()));
    fields.put("dir", "out");
    // Masked like anything else now that masking costs the loop nothing: no answer carries a credential today, and
    // one added later should not have to remember this.
    fields.wire("msg", text);
    gateway.audit.record("ws-frame", fields);
}

/// A message the hub queued for the client. One the gateway rendered from a sequenced input is recorded as what it
/// was, which input it came from and the hash of what was sent: the journal holds what it was rendered from, and
/// `reconstruct` renders it again to check (interfaces/audit/README.md). Anything else is recorded in full.
fn record_queued(gateway: &Gateway, origin: &Origin, session: &Session, message: &Queued) {
    if gateway.half == Half::Public {
        return;
    }
    let mut fields = about(gateway, origin, session.account(), session.api_key(), Some(session.id()));
    fields.put("dir", "out");
    match message.derived {
        Some(Derived { kind, sequence }) => fields.derived(&message.text, kind, sequence),
        None => fields.wire("msg", &message.text),
    }
    gateway.audit.record("ws-frame", fields);
}

/// Queues a message for the client, and says whether the connection is still worth keeping. A would-block is not a
/// failure: the message stays in the socket's own buffer and goes out on a later flush, when the loop is told the
/// socket can take more. A connection dropped for being too far behind says so in `ending`.
fn queue(socket: &mut WebSocket<Stream>, message: String, ending: &mut &'static str) -> bool {
    match socket.write(Message::text(message)) {
        Ok(()) => true,
        Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => true,
        // A slow consumer: further behind than a connection may be (`socket_config`). Dropped rather than buffered
        // for without limit; a client that reconnects starts again from a fresh image.
        Err(tungstenite::Error::WriteBufferFull(_)) => {
            let peer = socket.get_ref().peer_addr().map_or_else(|_| "a client".to_string(), |a| a.to_string());
            eprintln!("gateway: disconnected {peer}: too far behind on what it has been sent");
            *ending = "too far behind";
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

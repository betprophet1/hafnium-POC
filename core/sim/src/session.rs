//! A client session on the gateway's WebSocket (interfaces/gateway/asyncapi.yaml), as the synthetic maker uses it: one
//! login with cancel-on-disconnect and a heartbeat, requests matched to replies by `id`, everything else kept in order.

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket};
use venue_link::wire::Error;

/// How long connecting, or any one read or write outside the poll loop, may take before the gateway counts as gone.
const PATIENCE: Duration = Duration::from_secs(10);

/// The bot's own pacing clock. Nothing it measures reaches the journal.
#[allow(clippy::disallowed_methods)]
pub fn monotonic() -> Instant {
    Instant::now()
}

/// Connects to the gateway with every wait bounded. Without that, a gateway that vanishes mid-connection (its pod
/// deleted, its node gone) leaves the socket established and the client blocked on it for ever.
pub fn connect(addr: &str) -> Result<TcpStream, Error> {
    let resolved = addr.to_socket_addrs()?.next().ok_or_else(|| format!("{addr} resolves to nothing"))?;
    let stream = TcpStream::connect_timeout(&resolved, PATIENCE)?;
    stream.set_read_timeout(Some(PATIENCE))?;
    stream.set_write_timeout(Some(PATIENCE))?;
    Ok(stream)
}

pub struct Session {
    socket: WebSocket<TcpStream>,
    inbox: VecDeque<Value>,
    next_id: u64,
    last_sent: Instant,
    heartbeat: Duration,
}

impl Session {
    /// Connects and logs in. The gateway cancels this session's resting orders when it ends, and when it has been
    /// silent for `heartbeat_seconds` (interfaces/gateway/README.md, sessions).
    pub fn open(gateway: &str, token: &str, heartbeat_seconds: u64) -> Result<(Self, Value), Error> {
        let stream = connect(gateway)?;
        stream.set_nodelay(true)?;
        let (socket, _) = tungstenite::client(format!("ws://{gateway}/v4/ws"), stream)
            .map_err(|e| format!("websocket handshake: {e}"))?;
        socket.get_ref().set_read_timeout(Some(Duration::from_millis(50)))?;
        let mut session = Self {
            socket,
            inbox: VecDeque::new(),
            next_id: 0,
            last_sent: monotonic(),
            heartbeat: Duration::from_secs(heartbeat_seconds.max(1)),
        };
        let login =
            json!({"op": "login", "token": token, "cancelOnDisconnect": true, "heartbeatSeconds": heartbeat_seconds});
        let welcome = session.request(login)?;
        if welcome["type"] != "welcome" {
            return Err(format!("login refused: {}", welcome["error"]).into());
        }
        Ok((session, welcome))
    }

    pub fn send(&mut self, message: &Value) -> Result<(), Error> {
        self.socket.send(Message::text(message.to_string()))?;
        self.last_sent = monotonic();
        Ok(())
    }

    /// Sends a request and waits for the message answering it; every other message is kept for `poll`.
    pub fn request(&mut self, mut message: Value) -> Result<Value, Error> {
        self.next_id += 1;
        let id = format!("r{}", self.next_id);
        message["id"] = json!(id);
        self.send(&message)?;
        let deadline = monotonic() + Duration::from_secs(10);
        while monotonic() < deadline {
            if let Some(m) = self.read()? {
                if m["id"] == id.as_str() {
                    return Ok(m);
                }
                self.inbox.push_back(m);
            }
        }
        Err("the gateway did not answer within 10 s".into())
    }

    /// Everything that arrived within `wait`, in order, sending a heartbeat whenever one is due.
    pub fn poll(&mut self, wait: Duration) -> Result<Vec<Value>, Error> {
        let deadline = monotonic() + wait;
        loop {
            if monotonic().duration_since(self.last_sent) >= self.heartbeat / 3 {
                self.send(&json!({"op": "heartbeat"}))?;
            }
            if let Some(m) = self.read()? {
                self.inbox.push_back(m);
            }
            if monotonic() >= deadline {
                return Ok(self.inbox.drain(..).collect());
            }
        }
    }

    fn read(&mut self) -> Result<Option<Value>, Error> {
        match self.socket.read() {
            Ok(Message::Text(text)) => Ok(Some(serde_json::from_str(text.as_str())?)),
            Ok(Message::Close(_)) => Err("the gateway closed the session".into()),
            Ok(_) => Ok(None),
            // A timeout is the poll interval; an interrupted read (a signal, a debugger) is retried.
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted) =>
            {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }
}

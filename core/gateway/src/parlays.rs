//! The private half's connection to the parlay service (`venue-parlays`), which holds requests for quote and the
//! quotes attached to them (interfaces/gateway/README.md, parlays). The gateway keeps none of that: it forwards a
//! client's call, answers with what the service says, and delivers what the service sends for an account's sessions.
//!
//! The protocol is JSON, one object per line, over one connection. A call carries a `ref` this side chooses, and its
//! answer carries the same `ref`, in whatever order answers are ready; anything with `to` instead is a message for
//! those accounts' sessions. Like the ingress (`venue_link::submit`), a caller never waits here: it gets a `Pending`
//! and a notify once the answer can be taken. While the service is unreachable every call fails at once, because a
//! request for quote that nothing is serving should be refused rather than queued.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;
use venue_link::submit::Notify;
use venue_link::wire::Error;

/// Calls waiting to be written; past this a caller is refused rather than blocked.
const QUEUE_DEPTH: usize = 1024;

/// What the service answered: its result, or its refusal as `(status, code, message)`.
pub type Answer = Result<Value, (u16, String, String)>;

/// Where one call's answer goes, and who to tell.
struct Waiting {
    answer: SyncSender<Answer>,
    notify: Option<Notify>,
}

impl Waiting {
    fn send(self, answer: Answer) {
        let _ = self.answer.send(answer);
        if let Some(notify) = self.notify {
            notify();
        }
    }
}

struct Call {
    body: Value,
    waiting: Waiting,
}

/// Delivers a message to the sessions of the accounts named.
pub type Deliver = Arc<dyn Fn(&[u64], &Value) + Send + Sync>;

pub struct Parlays {
    queue: SyncSender<Call>,
}

/// A call the service has not answered yet.
pub struct Pending {
    wait: Receiver<Answer>,
}

fn gone() -> (u16, String, String) {
    (503, "UNAVAILABLE".into(), "the parlay service is not reachable; nothing was requested".into())
}

impl Pending {
    /// The answer, if it has arrived.
    pub fn take(&self) -> Option<Answer> {
        match self.wait.try_recv() {
            Ok(answer) => Some(answer),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(gone())),
        }
    }

    /// The answer, however long it takes.
    #[cfg(test)]
    pub fn wait(self) -> Answer {
        self.wait.recv().unwrap_or_else(|_| Err(gone()))
    }
}

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Parlays {
    /// Connects in the background and keeps reconnecting, handing whatever the service sends for sessions to
    /// `deliver`.
    pub fn spawn(addr: &str, deliver: Deliver) -> Self {
        let (queue, calls) = sync_channel(QUEUE_DEPTH);
        let addr = addr.to_string();
        thread::spawn(move || connect(&addr, &calls, &deliver));
        Self { queue }
    }

    /// Hands a call to the service without waiting for its answer.
    pub fn call(&self, body: Value, notify: Option<Notify>) -> Result<Pending, Error> {
        let (answer, wait) = sync_channel(1);
        let call = Call { body, waiting: Waiting { answer, notify } };
        self.queue.try_send(call).map_err(|_| "the parlay service's queue is full")?;
        Ok(Pending { wait })
    }
}

/// Owns the connection, one generation at a time. Between generations, calls are refused as they arrive.
fn connect(addr: &str, calls: &Receiver<Call>, deliver: &Deliver) {
    loop {
        match TcpStream::connect(addr) {
            Ok(stream) => {
                eprintln!("gateway: connected to the parlay service at {addr}");
                if let Err(e) = generation(stream, calls, deliver) {
                    eprintln!("gateway: parlay service {addr}: {e}; reconnecting");
                }
            }
            Err(e) => eprintln!("gateway: parlay service {addr}: {e}; retrying"),
        }
        // A second between attempts, refusing whatever is asked meanwhile.
        for _ in 0..10 {
            match calls.recv_timeout(Duration::from_millis(100)) {
                Ok(call) => call.waiting.send(Err(gone())),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

fn generation(stream: TcpStream, calls: &Receiver<Call>, deliver: &Deliver) -> Result<(), Error> {
    stream.set_nodelay(true)?;
    let waiting: Arc<Mutex<BTreeMap<u64, Waiting>>> = Arc::new(Mutex::new(BTreeMap::new()));
    let reading = {
        let (stream, waiting, deliver) = (stream.try_clone()?, Arc::clone(&waiting), Arc::clone(deliver));
        thread::spawn(move || read(stream, &waiting, &deliver))
    };
    let mut writer = stream.try_clone()?;
    let mut next = 0u64;
    let outcome = loop {
        let mut call = match calls.recv_timeout(Duration::from_millis(100)) {
            Ok(call) => call,
            Err(RecvTimeoutError::Timeout) if reading.is_finished() => break Err(Error::from("the service closed")),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break Ok(()),
        };
        next += 1;
        call.body["ref"] = json!(next);
        locked(&waiting).insert(next, call.waiting);
        let line = format!("{}\n", call.body);
        if let Err(e) = writer.write_all(line.as_bytes()) {
            break Err(e.into());
        }
    };
    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = reading.join();
    for (_, w) in std::mem::take(&mut *locked(&waiting)) {
        w.send(Err(gone()));
    }
    outcome
}

fn read(stream: TcpStream, waiting: &Mutex<BTreeMap<u64, Waiting>>, deliver: &Deliver) {
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { return };
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        if let Some(accounts) = message["to"].as_array() {
            let accounts: Vec<u64> = accounts.iter().filter_map(Value::as_u64).collect();
            deliver(&accounts, &message["message"]);
            continue;
        }
        let Some(w) = message["ref"].as_u64().and_then(|r| locked(waiting).remove(&r)) else { continue };
        let error = &message["error"];
        w.send(if error.is_null() {
            Ok(message["result"].clone())
        } else {
            Err((
                error["status"].as_u64().and_then(|s| u16::try_from(s).ok()).unwrap_or(503),
                error["code"].as_str().unwrap_or("UNAVAILABLE").to_string(),
                error["message"].as_str().unwrap_or("").to_string(),
            ))
        });
    }
}

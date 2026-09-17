//! The private half's connection to the parlay service (`venue-parlays`), which holds requests for quote and the
//! quotes attached to them (interfaces/gateway/README.md, parlays). The gateway keeps none of that: it forwards a
//! client's call, answers with what the service says, and delivers what the service sends for an account's sessions.
//!
//! The protocol is JSON, one object per line, over one connection. A call carries a `ref` this side chooses, and its
//! answer carries the same `ref`, in whatever order answers are ready; anything with `to` instead is a message for
//! those accounts' sessions. Like the ingress (`venue_link::submit`), a caller never waits here: it gets a `Pending`
//! and a notify once the answer can be taken. While the service is unreachable every call fails at once, because a
//! request for quote that nothing is serving should be refused rather than queued.
//!
//! Responders' subscriptions are the one thing kept here, and only for as long as their sessions last: the service
//! forgets a gateway's subscriptions when its connection ends, whether the service restarted or the connection
//! dropped, so this side remembers what each live session subscribed with and subscribes it again first thing on
//! every new connection. Requests still open that it would have been sent come back with the answer and are
//! delivered to the session. A resubscription the service refuses (the account's approval was revoked meanwhile) is
//! forgotten, and the session told with an `rfqSubscription` message.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;
use venue_link::submit::Notify;
use venue_link::wire::Error;

/// Calls waiting to be written; past this a caller is refused rather than blocked.
const QUEUE_DEPTH: usize = 1024;

/// What the service answered: its result, or its refusal as `(status, code, message)`.
pub type Answer = Result<Value, (u16, String, String)>;

/// Where a message from the service goes.
pub enum Target<'a> {
    /// Every private session of these accounts.
    Accounts(&'a [u64]),
    /// These sessions, by the key the hub gave them.
    Sessions(&'a [u64]),
}

/// One session's subscription as this side knows it.
#[derive(Clone)]
struct Remembered {
    /// Tells one subscribe from the next.
    stamp: u64,
    /// The call to send again.
    call: Value,
    /// Whether the service has taken it. One still on its way is not sent again on a new connection: it is about to
    /// be sent there anyway, or refused as the old connection's calls are.
    taken: bool,
}

/// What each session subscribed to requests with, by session key.
#[derive(Default)]
struct Subscribed {
    next_stamp: u64,
    calls: BTreeMap<u64, Remembered>,
}

/// What an answer to a subscribe changes here.
enum Subscribing {
    /// A session's own subscribe, remembered as it was sent: a refusal puts back what the session had before.
    Asked { memory: Arc<Mutex<Subscribed>>, session: u64, stamp: u64, previous: Option<Remembered> },
    /// A subscription sent again on a new connection: what it catches up with is delivered, and a refusal other than
    /// the connection going again ends it.
    Replayed { memory: Arc<Mutex<Subscribed>>, session: u64, stamp: u64, deliver: Deliver },
}

/// Where one call's answer goes, and who to tell.
struct Waiting {
    answer: Option<SyncSender<Answer>>,
    notify: Option<Notify>,
    subscribing: Option<Subscribing>,
}

impl Waiting {
    fn send(self, answer: Answer) {
        match self.subscribing {
            Some(Subscribing::Asked { memory, session, stamp, previous }) => {
                let mut memory = locked(&memory);
                if memory.calls.get(&session).is_some_and(|r| r.stamp == stamp) {
                    match (answer.is_ok(), previous) {
                        (true, _) => {
                            if let Some(r) = memory.calls.get_mut(&session) {
                                r.taken = true;
                            }
                        }
                        (false, Some(previous)) => {
                            memory.calls.insert(session, previous);
                        }
                        (false, None) => {
                            memory.calls.remove(&session);
                        }
                    }
                }
            }
            Some(Subscribing::Replayed { memory, session, stamp, deliver }) => match &answer {
                Ok(result) => {
                    for rfq in result["requests"].as_array().into_iter().flatten() {
                        deliver(Target::Sessions(&[session]), rfq);
                    }
                }
                // The connection went again before the answer: the next one sends it again.
                Err((503, _, _)) => {}
                Err((_, _, reason)) => {
                    let mut memory = locked(&memory);
                    if memory.calls.get(&session).is_some_and(|r| r.stamp == stamp) {
                        memory.calls.remove(&session);
                        drop(memory);
                        let ended = json!({"type": "rfqSubscription", "subscribed": false, "reason": reason});
                        deliver(Target::Sessions(&[session]), &ended);
                    }
                }
            },
            _ => {}
        }
        if let Some(sender) = self.answer {
            let _ = sender.send(answer);
        }
        if let Some(notify) = self.notify {
            notify();
        }
    }
}

struct Call {
    body: Value,
    waiting: Waiting,
}

/// Delivers a message to the sessions named.
pub type Deliver = Arc<dyn Fn(Target<'_>, &Value) + Send + Sync>;

pub struct Parlays {
    queue: SyncSender<Call>,
    subscribed: Arc<Mutex<Subscribed>>,
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
        let subscribed = Arc::new(Mutex::new(Subscribed::default()));
        let memory = Arc::clone(&subscribed);
        thread::spawn(move || connect(&addr, &calls, &deliver, &memory));
        Self { queue, subscribed }
    }

    /// Hands a call to the service without waiting for its answer.
    pub fn call(&self, body: Value, notify: Option<Notify>) -> Result<Pending, Error> {
        self.send(body, notify, None)
    }

    fn send(&self, body: Value, notify: Option<Notify>, subscribing: Option<Subscribing>) -> Result<Pending, Error> {
        let (answer, wait) = sync_channel(1);
        let call = Call { body, waiting: Waiting { answer: Some(answer), notify, subscribing } };
        self.queue.try_send(call).map_err(|e| {
            let (TrySendError::Full(call) | TrySendError::Disconnected(call)) = e;
            // Whatever it would have changed here is put back; nobody is waiting for it.
            Waiting { answer: None, notify: None, ..call.waiting }.send(Err(gone()));
            "the parlay service's queue is full"
        })?;
        Ok(Pending { wait })
    }

    /// Subscribes a session to requests (`body` is the whole `subscribe` call), remembering it so that it can be sent
    /// again on a new connection.
    pub fn subscribe(&self, session: u64, body: Value, notify: Option<Notify>) -> Result<Pending, Error> {
        let (stamp, previous) = {
            let mut memory = locked(&self.subscribed);
            memory.next_stamp += 1;
            let stamp = memory.next_stamp;
            (stamp, memory.calls.insert(session, Remembered { stamp, call: body.clone(), taken: false }))
        };
        let memory = Arc::clone(&self.subscribed);
        self.send(body, notify, Some(Subscribing::Asked { memory, session, stamp, previous }))
    }

    /// Unsubscribes a session (`body` is the whole `unsubscribe` call).
    pub fn unsubscribe(&self, session: u64, body: Value, notify: Option<Notify>) -> Result<Pending, Error> {
        locked(&self.subscribed).calls.remove(&session);
        self.call(body, notify)
    }

    /// A session has ended: if it was subscribed, the service is told, and nobody waits for the answer.
    pub fn forget(&self, session: u64) {
        let Some(Remembered { call: mut body, .. }) = locked(&self.subscribed).calls.remove(&session) else { return };
        body["op"] = json!("unsubscribe");
        if let Some(fields) = body.as_object_mut() {
            fields.remove("filters");
        }
        let _ = self.call(body, None);
    }

    /// Whether a session is subscribed, as far as this side knows.
    #[cfg(test)]
    pub fn is_subscribed(&self, session: u64) -> bool {
        locked(&self.subscribed).calls.contains_key(&session)
    }
}

/// Owns the connection, one generation at a time. Between generations, calls are refused as they arrive.
fn connect(addr: &str, calls: &Receiver<Call>, deliver: &Deliver, memory: &Arc<Mutex<Subscribed>>) {
    loop {
        match TcpStream::connect(addr) {
            Ok(stream) => {
                eprintln!("gateway: connected to the parlay service at {addr}");
                if let Err(e) = generation(stream, calls, deliver, memory) {
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

fn generation(
    stream: TcpStream,
    calls: &Receiver<Call>,
    deliver: &Deliver,
    memory: &Arc<Mutex<Subscribed>>,
) -> Result<(), Error> {
    stream.set_nodelay(true)?;
    let waiting: Arc<Mutex<BTreeMap<u64, Waiting>>> = Arc::new(Mutex::new(BTreeMap::new()));
    let reading = {
        let (stream, waiting, deliver, memory) =
            (stream.try_clone()?, Arc::clone(&waiting), Arc::clone(deliver), Arc::clone(memory));
        thread::spawn(move || read(stream, &waiting, &deliver, &memory))
    };
    let mut writer = stream.try_clone()?;
    let mut next = 0u64;
    // Every live subscription first, before any call that was queued behind them.
    let replays: Vec<(u64, u64, Value)> = locked(memory)
        .calls
        .iter()
        .filter(|(_, r)| r.taken)
        .map(|(session, r)| (*session, r.stamp, r.call.clone()))
        .collect();
    if !replays.is_empty() {
        eprintln!("gateway: subscribing {} sessions to the parlay service again", replays.len());
    }
    let mut replayed = Ok(());
    for (session, stamp, mut body) in replays {
        next += 1;
        body["ref"] = json!(next);
        let subscribing =
            Subscribing::Replayed { memory: Arc::clone(memory), session, stamp, deliver: Arc::clone(deliver) };
        locked(&waiting).insert(next, Waiting { answer: None, notify: None, subscribing: Some(subscribing) });
        if let Err(e) = writer.write_all(format!("{body}\n").as_bytes()) {
            replayed = Err(e);
            break;
        }
    }
    let outcome = loop {
        if let Err(e) = std::mem::replace(&mut replayed, Ok(())) {
            break Err(e.into());
        }
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

fn read(stream: TcpStream, waiting: &Mutex<BTreeMap<u64, Waiting>>, deliver: &Deliver, memory: &Mutex<Subscribed>) {
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { return };
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        if let Some(accounts) = message["to"].as_array() {
            let accounts: Vec<u64> = accounts.iter().filter_map(Value::as_u64).collect();
            deliver(Target::Accounts(&accounts), &message["message"]);
            continue;
        }
        if let Some(sessions) = message["toSessions"].as_array() {
            let sessions: Vec<u64> = sessions.iter().filter_map(Value::as_u64).collect();
            // A subscription the service ended (a revoked approval) is not sent again on the next connection.
            if message["message"]["type"] == "rfqSubscription" && message["message"]["subscribed"] == false {
                let mut memory = locked(memory);
                for session in &sessions {
                    memory.calls.remove(session);
                }
            }
            deliver(Target::Sessions(&sessions), &message["message"]);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc::{channel, Receiver};

    /// A stand-in for the parlay service: each connection it accepts is handed to the test as a line reader and a
    /// writer.
    struct Fake {
        listener: TcpListener,
    }

    impl Fake {
        fn new() -> Self {
            Self { listener: TcpListener::bind("127.0.0.1:0").expect("a port") }
        }

        fn address(&self) -> String {
            self.listener.local_addr().expect("its address").to_string()
        }

        fn accept(&self) -> (std::io::Lines<BufReader<TcpStream>>, TcpStream) {
            let (stream, _) = self.listener.accept().expect("the gateway connects");
            stream.set_read_timeout(Some(Duration::from_secs(10))).expect("a timeout");
            (BufReader::new(stream.try_clone().expect("a clone")).lines(), stream)
        }
    }

    fn next(lines: &mut std::io::Lines<BufReader<TcpStream>>) -> Value {
        let line = lines.next().expect("a call").expect("a line");
        serde_json::from_str(&line).expect("JSON")
    }

    fn answer(stream: &mut TcpStream, call: &Value, result: Value) {
        let line = if result["error"].is_object() {
            json!({"ref": call["ref"], "error": result["error"]})
        } else {
            json!({"ref": call["ref"], "result": result})
        };
        stream.write_all(format!("{line}\n").as_bytes()).expect("answered");
    }

    fn deliveries() -> (Deliver, Receiver<(Vec<u64>, Value)>) {
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        let deliver: Deliver = Arc::new(move |target, message| {
            if let Target::Sessions(sessions) = target {
                let _ = locked(&tx).send((sessions.to_vec(), message.clone()));
            }
        });
        (deliver, rx)
    }

    fn subscribe(session: u64) -> Value {
        json!({"op": "subscribe", "accountId": 10, "session": session, "filters": null})
    }

    /// The gateway's half of a restart: what its live sessions subscribed with is sent again on the next connection,
    /// before anything else, and what that catches up with reaches the sessions; a session that ended or unsubscribed
    /// is not sent again, and one the service now refuses is ended and told.
    #[test]
    fn subscriptions_are_sent_again_when_the_connection_to_the_service_is_new() {
        let fake = Fake::new();
        let (deliver, delivered) = deliveries();
        let parlays = Parlays::spawn(&fake.address(), deliver);
        let (mut lines, mut stream) = fake.accept();
        let mut pending = Vec::new();
        for session in [1, 2, 3, 4] {
            pending.push(parlays.subscribe(session, subscribe(session), None).expect("sent"));
            let call = next(&mut lines);
            assert_eq!(call["session"], session);
            answer(&mut stream, &call, json!({"subscribed": true, "requests": []}));
        }
        for p in pending {
            assert!(p.wait().is_ok());
        }
        parlays.forget(2);
        assert_eq!(next(&mut lines)["op"], "unsubscribe");
        let refused = parlays.subscribe(
            3,
            json!({"op": "subscribe", "accountId": 10, "session": 3, "filters": {"minLegs": 9}}),
            None,
        );
        let call = next(&mut lines);
        answer(&mut stream, &call, json!({"error": {"status": 400, "code": "BAD_REQUEST", "message": "minLegs"}}));
        assert!(refused.expect("sent").wait().is_err());
        assert!(parlays.is_subscribed(3), "a refused subscribe leaves the session's subscription as it was");

        // The service restarts.
        drop((lines, stream));
        let (mut lines, mut stream) = fake.accept();
        let replayed: Vec<Value> = (0..3).map(|_| next(&mut lines)).collect();
        let sessions: Vec<u64> = replayed.iter().filter_map(|c| c["session"].as_u64()).collect();
        assert_eq!(sessions, vec![1, 3, 4], "every live subscription and nothing else, in {replayed:?}");
        assert_eq!(replayed[1]["filters"], Value::Null, "the filters that were accepted, not the refused ones");
        let rfq = json!({"type": "rfq", "requestId": "r1"});
        answer(&mut stream, &replayed[0], json!({"subscribed": true, "requests": [rfq]}));
        answer(&mut stream, &replayed[1], json!({"subscribed": true, "requests": []}));
        let revoked = json!({"error": {"status": 403, "code": "FORBIDDEN", "message": "not approved"}});
        answer(&mut stream, &replayed[2], revoked);
        let (to, message) = delivered.recv_timeout(Duration::from_secs(5)).expect("the catch-up");
        assert_eq!((to, message), (vec![1], rfq));
        let (to, message) = delivered.recv_timeout(Duration::from_secs(5)).expect("the ending");
        assert_eq!((to, &message["type"], &message["subscribed"]), (vec![4], &json!("rfqSubscription"), &json!(false)));
        assert!(!parlays.is_subscribed(4));

        // The service ends a subscription itself when an approval is revoked.
        let ended = json!({"toSessions": [3], "message": {"type": "rfqSubscription", "subscribed": false}});
        stream.write_all(format!("{ended}\n").as_bytes()).expect("sent");
        let (to, _) = delivered.recv_timeout(Duration::from_secs(5)).expect("the ending");
        assert_eq!(to, vec![3]);
        assert!(!parlays.is_subscribed(3) && parlays.is_subscribed(1));
    }
}

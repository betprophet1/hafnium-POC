//! Talking to a node: inputs through the ingress, everything that happened through the feed.

use serde_json::{json, Value};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::Duration;
use venue_harness::codec;
use venue_node::frames::{read_frame, write_frame, MAX_FRAME};

pub type Error = Box<dyn std::error::Error + Send + Sync>;

type Connection = (BufReader<TcpStream>, BufWriter<TcpStream>);

/// A connection to a node's ingress, reopened on the next send after any error.
pub struct Ingress {
    addr: String,
    conn: Option<Connection>,
    buf: Vec<u8>,
    frame: Vec<u8>,
}

impl Ingress {
    pub fn new(addr: &str) -> Self {
        Self { addr: addr.to_string(), conn: None, buf: vec![0; MAX_FRAME], frame: Vec::new() }
    }

    /// Sends one input with a zeroed record header (the sequencer stamps it, spec E-17.2) and returns its results.
    pub fn send(&mut self, mut input: Value) -> Result<Vec<Value>, Error> {
        let object = input.as_object_mut().ok_or("an input must be a JSON object")?;
        object.insert("record".into(), json!({"sequence": 0, "sequencedAt": 0, "sequencerGroup": 0}));
        let n = codec::encode(&input, &mut self.buf)?;
        let outcome = self.exchange(n);
        if outcome.is_err() {
            self.conn = None;
        }
        outcome
    }

    fn exchange(&mut self, n: usize) -> Result<Vec<Value>, Error> {
        if self.conn.is_none() {
            let stream = TcpStream::connect(&self.addr)?;
            stream.set_nodelay(true)?;
            stream.set_read_timeout(Some(Duration::from_secs(10)))?;
            self.conn = Some((BufReader::new(stream.try_clone()?), BufWriter::new(stream)));
        }
        let (reader, writer) = self.conn.as_mut().ok_or("not connected")?;
        write_frame(writer, &self.buf[..n])?;
        writer.flush()?;
        let mut results = Vec::new();
        loop {
            match read_frame(reader, &mut self.frame)? {
                Some(0) => return Ok(results),
                Some(m) => results.push(codec::decode(&self.frame[..m])?),
                None => return Err("the node closed the connection".into()),
            }
        }
    }
}

/// One input from the feed, with its results.
pub struct Unit {
    pub input: Value,
    pub results: Vec<Value>,
}

/// Follows a node's feed for ever. `on_connect` runs before every connection's backlog, so a reader can start over.
pub fn follow(addr: &str, on_connect: impl FnMut(), on_unit: impl FnMut(Unit)) -> ! {
    follow_reporting(addr, on_connect, on_unit, |e| eprintln!("feed {addr}: {e}"))
}

/// As `follow`, telling `on_error` why each connection ended or could not be made (with what happens next), for a
/// process that logs in its own format.
pub fn follow_reporting(
    addr: &str,
    mut on_connect: impl FnMut(),
    mut on_unit: impl FnMut(Unit),
    mut on_error: impl FnMut(&str),
) -> ! {
    loop {
        match TcpStream::connect(addr) {
            Ok(stream) => {
                on_connect();
                if let Err(e) = read_units(stream, &mut on_unit) {
                    on_error(&format!("{e}; reconnecting"));
                }
            }
            Err(e) => on_error(&format!("{e}; retrying")),
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn read_units(stream: TcpStream, on_unit: &mut impl FnMut(Unit)) -> Result<(), Error> {
    let mut reader = BufReader::new(stream);
    let mut frame = Vec::new();
    loop {
        let Some(n) = read_frame(&mut reader, &mut frame)? else { return Err("the feed ended".into()) };
        let input = codec::decode(&frame[..n])?;
        let mut results = Vec::new();
        loop {
            match read_frame(&mut reader, &mut frame)? {
                Some(0) => break,
                Some(m) => results.push(codec::decode(&frame[..m])?),
                None => return Err("the feed ended inside a unit".into()),
            }
        }
        on_unit(Unit { input, results });
    }
}

/// Command and client order IDs unique to this process: a random nonce, then a counter (spec E-7.1, E-9.2).
pub struct Ids {
    prefix: String,
    count: u64,
}

impl Ids {
    /// `tag` names the actor in the IDs it creates.
    pub fn new(tag: char) -> Result<Self, Error> {
        let mut nonce = [0u8; 4];
        File::open("/dev/urandom")?.read_exact(&mut nonce)?;
        Ok(Self { prefix: format!("{tag}{:08x}", u32::from_le_bytes(nonce)), count: 0 })
    }

    /// At most 16 bytes, so it serves as a command ID too.
    pub fn fresh(&mut self) -> String {
        self.count += 1;
        format!("{}{:x}", self.prefix, self.count)
    }
}

/// A `NewOrder` with the defaults of a plain limit order, overridden by `fields`.
pub fn order(account: u64, client_order_id: String, fields: Value) -> Value {
    let mut v = json!({
        "msg": "NewOrder", "sessionId": account * 10, "accountId": account, "clientOrderId": client_order_id,
        "enteredContract": "YES", "timeInForce": "GTC", "expiresAt": null, "atEventStart": "CANCEL", "flags": [],
        "protectionTicks": 0, "spendCapCents": null, "enteredStakeCents": null, "clientOrderGroup": null
    });
    if let (Some(o), Some(f)) = (v.as_object_mut(), fields.as_object()) {
        for (k, x) in f {
            o.insert(k.clone(), x.clone());
        }
    }
    v
}

pub fn amend(account: u64, instrument: u64, order_id: u64, tick: u64, quantity: u64) -> Value {
    json!({
        "msg": "AmendOrder", "sessionId": account * 10, "accountId": account, "instrumentId": instrument,
        "orderId": order_id, "newTick": tick, "newQuantity": quantity, "atEventStart": "UNCHANGED"
    })
}

pub fn cancel(account: u64, instrument: u64, order_id: u64) -> Value {
    json!({
        "msg": "CancelOrder", "sessionId": account * 10, "accountId": account, "instrumentId": instrument,
        "orderId": order_id, "clientOrderId": ""
    })
}

pub fn command(msg: &str, command_id: String, mut fields: Value) -> Value {
    fields["msg"] = json!(msg);
    fields["commandId"] = json!(command_id);
    fields
}

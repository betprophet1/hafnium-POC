//! Talking to a node: inputs through the ingress, everything that happened through the feed.

use serde_json::{json, Value};
use std::cell::OnceCell;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::thread;
use std::time::Duration;
use venue_harness::codec;
use venue_journal::message_header_codec::MessageHeaderDecoder;
use venue_journal::ReadBuf;
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

/// One frame as the node sent it, with the JSON shape of it made only if something asks.
///
/// The feed is SBE, and `venue_harness::codec` turns a frame into a `serde_json::Value` -- a `Map` of `String`
/// keys and boxed values, read back by comparing those keys. That codec exists so the hand-computed scenarios in
/// spec/engine/scenarios can be diffed against what the engine emits, and the feed path inherited it. It costs
/// about 857ns a result where reading the same fields out of the generated decoder costs about 2ns
/// (prototypes/feedapply), on the one thread per half that applies every unit (ADR-0002).
///
/// So a frame now carries its bytes, and `template` lets a reader dispatch on what it is and decode the fields it
/// wants. Readers that still want the JSON shape get it through `Deref`, decoded once and kept -- which keeps
/// every existing reader working and means nothing decodes a frame no one reads. The `Deref` is a migration aid,
/// not the destination: it hides the cost of a full decode behind an index, and should go once the readers that
/// need it have moved.
pub struct Frame {
    bytes: Vec<u8>,
    value: OnceCell<Value>,
}

impl Frame {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes, value: OnceCell::new() }
    }

    /// A frame from the JSON shape of one, for a test that describes what the feed carried rather than holding
    /// the bytes. Encoded through the same codec the node writes with, so what comes back out is what a reader
    /// of the real feed would see, including whatever the schema does not carry.
    pub fn of(value: &Value) -> Self {
        let mut bytes = vec![0_u8; MAX_FRAME];
        match codec::encode(value, &mut bytes) {
            Ok(n) => {
                bytes.truncate(n);
                Self::new(bytes)
            }
            Err(e) => panic!("a frame the codec cannot encode: {e}: {value}"),
        }
    }

    /// The bytes the node sent, for a reader decoding the fields it wants itself.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Whether anything has asked for the JSON shape of this frame yet. For tests that mean to check a reader
    /// takes a frame typed rather than decoding it, which is otherwise only visible as a timing difference.
    pub fn decoded(&self) -> bool {
        self.value.get().is_some()
    }

    /// Which message this is, without decoding it: the schema's template id, for dispatch.
    pub fn template(&self) -> u16 {
        MessageHeaderDecoder::default().wrap(ReadBuf::new(&self.bytes), 0).template_id()
    }
}

impl std::ops::Deref for Frame {
    type Target = Value;

    /// Decoded on the first ask and kept. A frame nothing asks about is never decoded at all.
    fn deref(&self) -> &Value {
        self.value.get_or_init(|| codec::decode(&self.bytes).unwrap_or(Value::Null))
    }
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&**self, f)
    }
}

/// One input from the feed, with its results.
pub struct Unit {
    pub input: Frame,
    pub results: Vec<Frame>,
}

/// The units a journal and the result log of replaying it make: what the feed carried, rebuilt from the files after
/// the fact, so that anything the gateway derived from the feed can be derived again (interfaces/audit/README.md,
/// frames derived from the journal). Inputs are in the order they were sequenced, each with the results of that
/// input, which name it by `inputSequence`.
///
/// A `Snapshot` frame is skipped, because the feed never carried one either: the node's `serve_subscriber` skips
/// it by template id, so a rebuild that produced one would not be a rebuild of what the feed carried. It matters
/// most in the case §3.7 describes — `embed` appends wherever the journal currently ends, so a snapshot naming an
/// earlier sequence can sit physically *after* later inputs. Applied as though it were an input, its record
/// header drags a reader's own position backwards, and a reader that then resumed from there would be re-sent
/// inputs it had already applied.
pub fn replayed_units(journal: &Path, results: &Path) -> Result<Vec<Unit>, Error> {
    let mut by_input: BTreeMap<u64, Vec<Frame>> = BTreeMap::new();
    for result in frames_of(results)? {
        by_input.entry(result["result"]["inputSequence"].as_u64().unwrap_or(0)).or_default().push(result);
    }
    let mut units = Vec::new();
    for input in frames_of(journal)? {
        if input["msg"] == "Snapshot" {
            continue;
        }
        let sequence = input["record"]["sequence"].as_u64().unwrap_or(0);
        let results = by_input.remove(&sequence).unwrap_or_default();
        units.push(Unit { input, results });
    }
    Ok(units)
}

/// Every frame of a journal or result log, decoded. A file cut off mid-frame ends where it was cut, as the node's own
/// reader treats it.
fn frames_of(path: &Path) -> Result<Vec<Frame>, Error> {
    let mut reader = BufReader::new(File::open(path)?);
    let (mut frame, mut out) = (Vec::new(), Vec::new());
    loop {
        match read_frame(&mut reader, &mut frame) {
            Ok(Some(n)) => out.push(Frame::new(frame[..n].to_vec())),
            Ok(None) => return Ok(out),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(out),
            Err(e) => return Err(e.into()),
        }
    }
}

/// Follows a node's feed for ever.
///
/// `on_connect` runs before every connection's backlog and answers with the last sequence this reader already
/// holds — zero to be sent everything, which is what a reader that starts over each time returns. Answering
/// honestly is what makes a reconnect cost what was missed rather than the whole journal.
///
/// `on_reset` runs when the node is about to send the backlog from the beginning although this reader asked to
/// resume: its node was replaced, and whatever it kept is a view of a history that never happened. A reader that
/// always asks for everything never sees it.
pub fn follow(addr: &str, on_connect: impl FnMut() -> u64, on_reset: impl FnMut(), on_unit: impl FnMut(Unit)) -> ! {
    follow_reporting(addr, on_connect, on_reset, on_unit, |e| eprintln!("feed {addr}: {e}"))
}

/// As `follow`, telling `on_error` why each connection ended or could not be made (with what happens next), for a
/// process that logs in its own format.
pub fn follow_reporting(
    addr: &str,
    mut on_connect: impl FnMut() -> u64,
    mut on_reset: impl FnMut(),
    mut on_unit: impl FnMut(Unit),
    mut on_error: impl FnMut(&str),
) -> ! {
    loop {
        match TcpStream::connect(addr) {
            Ok(mut stream) => {
                let resume_from = on_connect();
                match open(&mut stream, resume_from) {
                    Ok(resuming) => {
                        if resume_from > 0 && !resuming {
                            on_reset();
                        }
                        if let Err(e) = read_units(stream, &mut on_unit) {
                            on_error(&format!("{e}; reconnecting"));
                        }
                    }
                    Err(e) => on_error(&format!("{e}; reconnecting")),
                }
            }
            Err(e) => on_error(&format!("{e}; retrying")),
        }
        thread::sleep(Duration::from_secs(1));
    }
}

/// Opens a subscription: says where this reader has got to, and reads back whether the node is honouring it.
fn open(stream: &mut TcpStream, resume_from: u64) -> Result<bool, Error> {
    stream.write_all(&resume_from.to_le_bytes())?;
    stream.flush()?;
    let mut answer = [0u8; 1];
    stream.read_exact(&mut answer)?;
    Ok(answer[0] != 0)
}

fn read_units(stream: TcpStream, on_unit: &mut impl FnMut(Unit)) -> Result<(), Error> {
    let mut reader = BufReader::new(stream);
    let mut frame = Vec::new();
    loop {
        let Some(n) = read_frame(&mut reader, &mut frame)? else { return Err("the feed ended".into()) };
        let input = Frame::new(frame[..n].to_vec());
        let mut results = Vec::new();
        loop {
            match read_frame(&mut reader, &mut frame)? {
                Some(0) => break,
                Some(m) => results.push(Frame::new(frame[..m].to_vec())),
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

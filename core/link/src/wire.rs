//! Talking to a node: inputs through the ingress, everything that happened through the feed.

use serde_json::{json, Value};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::net::TcpStream;
use std::path::Path;
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
/// **Neither file is held in memory.** This used to decode the whole journal and the whole result log into
/// `Vec<Value>` before pairing them, which is fine on a fixture and not fine on a venue: taking a snapshot pair
/// from the live journal — 780,000 units — peaked at **3.6 GB**, against a scheduled job's 512 MiB on an 8 GB
/// node that also runs the venue. The cost grew with the length of history rather than with the size of the
/// state, so it got worse every day and no amount of cutting the *engine's* work helped, because the
/// materialising happened before any of it.
///
/// Both files are in sequence order — the node appends an input and then the results of applying it — so they
/// can be walked in lockstep, and nothing bigger than one unit is ever live. That order is the one assumption
/// here, and it is the same one `results_cut_offset` and `trusted_results` already make. A result naming an
/// input the journal no longer carries (a journal `cut` past it) is skipped rather than kept for a sequence
/// that will never arrive.
pub fn replayed_units(journal: &Path, results: &Path) -> Result<Units, Error> {
    Ok(Units { journal: Frames::open(journal)?, results: Frames::open(results)?.peekable() })
}

/// The units of a journal and its result log, one at a time.
pub struct Units {
    journal: Frames,
    results: std::iter::Peekable<Frames>,
}

impl Iterator for Units {
    type Item = Result<Unit, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let input = match self.journal.next()? {
                Ok(input) => input,
                Err(e) => return Some(Err(e)),
            };
            if input["msg"] == "Snapshot" {
                continue;
            }
            let sequence = input["record"]["sequence"].as_u64().unwrap_or(0);

            let mut results = Vec::new();
            loop {
                // Peek before taking, so the borrow is released before the results are advanced.
                let wanted = match self.results.peek() {
                    Some(Ok(result)) => result["result"]["inputSequence"].as_u64().unwrap_or(0) <= sequence,
                    Some(Err(_)) => true,
                    None => false,
                };
                if !wanted {
                    break;
                }
                match self.results.next() {
                    // Equal: this input's own results. Lower: a result for an input this journal no longer
                    // carries, which is dropped rather than attached to the wrong input.
                    Some(Ok(result)) => {
                        if result["result"]["inputSequence"].as_u64().unwrap_or(0) == sequence {
                            results.push(result);
                        }
                    }
                    Some(Err(e)) => return Some(Err(e)),
                    None => break,
                }
            }
            return Some(Ok(Unit { input, results }));
        }
    }
}

/// Every frame of a journal or result log, decoded, one at a time. A file cut off mid-frame ends where it was
/// cut, as the node's own reader treats it.
struct Frames {
    reader: BufReader<File>,
    frame: Vec<u8>,
}

impl Frames {
    fn open(path: &Path) -> Result<Self, Error> {
        Ok(Self { reader: BufReader::new(File::open(path)?), frame: Vec::new() })
    }
}

impl Iterator for Frames {
    type Item = Result<Value, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        match read_frame(&mut self.reader, &mut self.frame) {
            Ok(Some(n)) => Some(codec::decode(&self.frame[..n]).map_err(Into::into)),
            Ok(None) => None,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => None,
            Err(e) => Some(Err(e.into())),
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

//! A pipelined connection to a node's ingress, for a gateway serving many client connections at once.
//!
//! [`Ingress`](crate::wire::Ingress) sends one input and waits for its results, which is all a tool needs. A
//! gateway cannot hold that across a round trip: every client would queue behind it. Here each caller encodes its
//! own input, hands the bytes to a bounded queue and waits on its own reply channel, so the only shared work is a
//! queue push. One forwarder thread owns the socket and writes; one reader thread matches each batch of results to
//! the request at the front of the pending queue, which is safe because a node replies on a connection in commit
//! order. Nothing is reordered, so a session's inputs reach the sequencer in the order it sent them.
//!
//! A full queue blocks the caller: that is backpressure, and it is better than growing without bound when the node
//! falls behind. When a connection fails, every request already sent is answered with an error, because its outcome
//! is genuinely unknown (E-17.4); the caller resends with the same client order ID, which the engine deduplicates.

use crate::wire::Error;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{BufReader, BufWriter, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;
use venue_harness::codec;
use venue_node::frames::{read_frame, write_frame, MAX_FRAME};

/// Requests waiting to be written. Deep enough to ride out a pause at the node, short enough that a caller feels
/// backpressure long before memory becomes a problem.
const QUEUE_DEPTH: usize = 1024;

/// Where `ResultHeader.inputSequence` sits in a result. The schema puts that header first in every result's body
/// (`interfaces/sbe/venue-journal.xml`), after SBE's fixed message header, which is
/// `venue_journal::message_header_codec::ENCODED_LENGTH` bytes.
const INPUT_SEQUENCE: usize = 8;

/// What the node said: one raw SBE frame per result, decoded by whoever asked.
type Frames = Vec<Vec<u8>>;
type Answer = SyncSender<Result<Frames, String>>;

struct Request {
    bytes: Vec<u8>,
    answer: Answer,
}

thread_local! {
    /// Each caller encodes into its own buffer, so encoding never touches shared state.
    static ENCODED: RefCell<Vec<u8>> = RefCell::new(vec![0; MAX_FRAME]);
}

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Fails every request whose results can no longer arrive.
fn fail_pending(pending: &Mutex<VecDeque<Answer>>) {
    for answer in locked(pending).drain(..) {
        let _ = answer.send(Err("the connection to the sequencer group ended".to_string()));
    }
}

pub struct Submit {
    queue: SyncSender<Request>,
}

impl Submit {
    /// Connects in the background and keeps reconnecting; sends before the first connection simply wait.
    pub fn spawn(addr: &str) -> Self {
        let (queue, requests) = sync_channel(QUEUE_DEPTH);
        let addr = addr.to_string();
        thread::spawn(move || forward(&addr, &requests));
        Self { queue }
    }

    /// Sends one input with a zeroed record header (the sequencer stamps it, spec E-17.2) and returns its results.
    pub fn send(&self, mut input: Value) -> Result<Vec<Value>, Error> {
        let object = input.as_object_mut().ok_or("an input must be a JSON object")?;
        object.insert("record".into(), json!({"sequence": 0, "sequencedAt": 0, "sequencerGroup": 0}));
        let bytes = ENCODED.with(|buffer| {
            let mut buffer = buffer.borrow_mut();
            let n = codec::encode(&input, &mut buffer)?;
            Ok::<Vec<u8>, Error>(buffer[..n].to_vec())
        })?;
        let (answer, wait) = sync_channel(1);
        self.queue.send(Request { bytes, answer }).map_err(|_| "the ingress forwarder stopped")?;
        let frames = wait.recv().map_err(|_| "the ingress forwarder dropped the request")??;
        frames.iter().map(|frame| codec::decode(frame).map_err(Error::from)).collect()
    }
}

/// Owns the connection: one generation at a time, reconnecting for ever.
fn forward(addr: &str, requests: &Receiver<Request>) {
    loop {
        match TcpStream::connect(addr) {
            Ok(stream) => {
                if let Err(e) = generation(stream, requests) {
                    eprintln!("ingress {addr}: {e}; reconnecting");
                }
            }
            Err(e) => eprintln!("ingress {addr}: {e}; retrying"),
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// Writes requests and lets a reader thread answer them, until the connection or the queue ends.
fn generation(stream: TcpStream, requests: &Receiver<Request>) -> Result<(), Error> {
    stream.set_nodelay(true)?;
    let pending: Arc<Mutex<VecDeque<Answer>>> = Arc::new(Mutex::new(VecDeque::new()));
    let mut writer = BufWriter::new(stream.try_clone()?);
    let reader_stream = stream.try_clone()?;
    let reading = {
        let pending = Arc::clone(&pending);
        thread::spawn(move || read_results(reader_stream, &pending))
    };
    let outcome = loop {
        // Timed, so that a connection which dies while the gateway happens to be idle is still noticed: otherwise
        // requests already written would wait for results that can never arrive.
        let request = match requests.recv_timeout(Duration::from_millis(100)) {
            Ok(request) => request,
            Err(RecvTimeoutError::Timeout) if reading.is_finished() => {
                break Err(Error::from("the node closed the connection"));
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break Err(Error::from("the gateway stopped")),
        };
        locked(&pending).push_back(request.answer);
        if let Err(e) = write_frame(&mut writer, &request.bytes).and_then(|()| writer.flush()) {
            break Err(Error::from(e));
        }
    };
    let _ = stream.shutdown(Shutdown::Both);
    let _ = reading.join();
    fail_pending(&pending);
    outcome
}

/// The input a result answers. None if the frame is too short to carry a result header at all.
fn input_sequence(frame: &[u8]) -> Option<u64> {
    let bytes: [u8; 8] = frame.get(INPUT_SEQUENCE..INPUT_SEQUENCE + 8)?.try_into().ok()?;
    Some(u64::from_le_bytes(bytes))
}

/// Matches each batch of results, terminated by an empty frame, to the oldest request still waiting, checking as
/// it goes that the stream still lines up with that queue.
fn read_results(stream: TcpStream, pending: &Mutex<VecDeque<Answer>>) {
    let mut reader = BufReader::new(stream);
    let mut frame = Vec::new();
    let mut last = 0u64;
    loop {
        let mut batch = Frames::new();
        loop {
            match read_frame(&mut reader, &mut frame) {
                Ok(Some(0)) => break,
                Ok(Some(n)) => batch.push(frame[..n].to_vec()),
                Ok(None) | Err(_) => return fail_pending(pending),
            }
        }
        // Requests are answered by position, so the stream has to stay in step with the queue. The node commits
        // inputs in order and answers each with one batch, so the input a batch names must climb; if it does not,
        // this connection is not what this thread thinks it is, and passing the batch on would hand a caller
        // another caller's results. An empty batch names no input and is perfectly normal: a mass cancel that
        // cancels nothing emits only its terminator, since E-5.2 gives `MassCancel` no outcome result.
        if let Some(first) = batch.first() {
            let Some(sequence) = input_sequence(first) else { return fail_pending(pending) };
            if sequence <= last {
                return fail_pending(pending);
            }
            last = sequence;
        }
        let waiting = locked(pending).pop_front();
        match waiting {
            Some(answer) => {
                let _ = answer.send(Ok(batch));
            }
            // Results with nothing waiting for them mean the stream is not what this thread thinks it is.
            None => return fail_pending(pending),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc::channel;

    /// A frame shaped like a result without going through the codec: the message header, then the input sequence.
    fn synthetic_frame(sequence: u64) -> Vec<u8> {
        let mut frame = vec![0u8; INPUT_SEQUENCE];
        frame.extend_from_slice(&sequence.to_le_bytes());
        frame.extend_from_slice(&[0u8; 14]); // the rest of ResultHeader and the body
        frame
    }

    /// A real `OrderAccepted`, encoded by the codec a node encodes its results with.
    fn encoded_result(sequence: u64, order_id: u64) -> Vec<u8> {
        let result = json!({
            "msg": "OrderAccepted",
            "result": {"inputSequence": sequence, "resultIndex": 0, "sequencedAt": 0, "sequencerGroup": 1},
            "sessionId": 10, "orderId": order_id, "accountId": 1, "clientOrderId": "a1",
            "instrumentId": 1000, "side": "BUY", "tick": 9, "quantity": 1, "reservedCents": 10
        });
        let mut buffer = vec![0u8; MAX_FRAME];
        let n = codec::encode(&result, &mut buffer).expect("a result the codec can encode");
        buffer[..n].to_vec()
    }

    /// A node that answers each request with the next batch and then closes; no batches means it closes at once.
    /// Enough of the protocol to drive a `Submit`, and nothing else: no engine, no journal, no feed.
    fn stub_node(batches: Vec<Vec<Vec<u8>>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
        let addr = listener.local_addr().expect("an address").to_string();
        thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else { return };
            let Ok(clone) = stream.try_clone() else { return };
            let mut reader = BufReader::new(clone);
            let mut writer = BufWriter::new(stream);
            let mut frame = Vec::new();
            for batch in batches {
                if !matches!(read_frame(&mut reader, &mut frame), Ok(Some(_))) {
                    return;
                }
                for f in &batch {
                    let _ = write_frame(&mut writer, f);
                }
                let _ = write_frame(&mut writer, &[]); // the empty frame ends the batch
                let _ = writer.flush();
            }
        });
        addr
    }

    /// Sends on another thread, so a stub that never answers fails the test instead of hanging the run.
    fn send(submit: &Arc<Submit>, input: Value) -> Result<Vec<Value>, String> {
        let submit = Arc::clone(submit);
        let (tx, rx) = channel();
        thread::spawn(move || {
            let _ = tx.send(submit.send(input).map_err(|e| e.to_string()));
        });
        rx.recv_timeout(Duration::from_secs(5)).unwrap_or_else(|_| Err("nothing answered within 5s".to_string()))
    }

    /// The smallest input there is: the stub never looks at it.
    fn tick() -> Value {
        json!({"msg": "TimeTick"})
    }

    #[test]
    fn a_result_names_the_input_it_answers() {
        assert_eq!(input_sequence(&synthetic_frame(1)), Some(1));
        assert_eq!(input_sequence(&synthetic_frame(u64::MAX)), Some(u64::MAX));
    }

    /// Where the input sequence sits is a claim about the schema, so read it out of what the codec really writes.
    #[test]
    fn the_input_sequence_is_where_the_schema_says_it_is() {
        assert_eq!(input_sequence(&encoded_result(7, 1)), Some(7));
        assert_eq!(input_sequence(&encoded_result(4_000_000_000, 1)), Some(4_000_000_000));
    }

    #[test]
    fn a_frame_too_short_to_be_a_result_names_nothing() {
        // Rather than read whatever follows in memory: the caller fails the connection instead.
        for len in 0..INPUT_SEQUENCE + 8 {
            assert_eq!(input_sequence(&vec![0u8; len]), None, "length {len}");
        }
    }

    #[test]
    fn a_batch_reaches_the_caller_that_asked_for_it() {
        let addr = stub_node(vec![vec![encoded_result(1, 4242)]]);
        let submit = Arc::new(Submit::spawn(&addr));
        let results = send(&submit, tick()).expect("the stub's results");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["msg"], "OrderAccepted");
        assert_eq!(results[0]["orderId"].as_u64(), Some(4242));
    }

    /// Requests are answered by position, so the stream has to stay in step with the queue of waiting callers. A
    /// node commits inputs in order and answers each with one batch; an input sequence that stops climbing means
    /// the two have come apart, and passing that batch on would hand this caller someone else's results.
    #[test]
    fn a_stream_that_stops_climbing_is_dropped_rather_than_delivered() {
        let addr = stub_node(vec![vec![encoded_result(5, 1)], vec![encoded_result(5, 2)]]);
        let submit = Arc::new(Submit::spawn(&addr));
        assert!(send(&submit, tick()).is_ok(), "the first batch answers input 5 and is fine");
        let second = send(&submit, tick());
        assert!(second.is_err(), "the second batch repeats input 5: {second:?}");
    }

    #[test]
    fn a_node_that_answers_nothing_fails_the_request() {
        let addr = stub_node(Vec::new());
        let submit = Arc::new(Submit::spawn(&addr));
        assert!(send(&submit, tick()).is_err());
    }
}

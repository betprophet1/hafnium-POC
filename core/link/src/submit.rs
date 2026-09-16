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
    use super::{input_sequence, INPUT_SEQUENCE};

    /// A result frame: SBE's message header, then the body, which starts with `ResultHeader.inputSequence`.
    fn result_frame(sequence: u64) -> Vec<u8> {
        let mut frame = vec![0u8; INPUT_SEQUENCE];
        frame.extend_from_slice(&sequence.to_le_bytes());
        frame.extend_from_slice(&[0u8; 14]); // the rest of ResultHeader and the body
        frame
    }

    #[test]
    fn a_result_names_the_input_it_answers() {
        assert_eq!(input_sequence(&result_frame(1)), Some(1));
        assert_eq!(input_sequence(&result_frame(u64::MAX)), Some(u64::MAX));
    }

    #[test]
    fn a_frame_too_short_to_be_a_result_names_nothing() {
        // Rather than read whatever follows in memory: the caller fails the connection instead.
        for len in 0..INPUT_SEQUENCE + 8 {
            assert_eq!(input_sequence(&vec![0u8; len]), None, "length {len}");
        }
    }
}

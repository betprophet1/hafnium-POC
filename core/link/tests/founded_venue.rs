//! A venue founded on a snapshot, seen from outside: a reader started from the pair's view half holds the
//! venue's actual state and carries on from it, where one that asks for everything comes up empty.
//!
//! This is the end the seeding work is for. A node restored from the engine half is correct inside, but its
//! journal holds no inputs at all, so a reader that asks for sequence 0 is sent an empty backlog and shows an
//! empty venue — up, and useless. The two halves together are what make a founded venue indistinguishable from
//! one that had been running all along.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use venue_link::pair;
use venue_link::view::{Live, View};
use venue_node::node::{seed, serve, Config};

fn free_port() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0").expect("a port").local_addr().expect("its address")
}

fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let nth = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("venue-founded-{name}-{}-{nth}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// A venue founded on the recorded fixture's state at sequence 62: both halves taken, the node seeded from the
/// engine half and serving. Returns its feed address and the pair's directory.
fn founded_venue(dir: &Path) -> (SocketAddr, PathBuf) {
    let recorded = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interfaces/audit/recorded");
    let journal = dir.join("journal.sbe");
    std::fs::copy(recorded.join("journal.sbe"), &journal).expect("the journal copies");
    std::fs::copy(recorded.join("capacity"), dir.join("capacity")).expect("the capacity copies");

    let pair_dir = dir.join("pair");
    let at = pair::take(&journal, &pair_dir, Some(62), None).expect("both halves are taken");
    assert_eq!(at, 62);

    let data = dir.join("venue-data");
    seed(&pair_dir.join(pair::ENGINE), &data, "small").expect("the node is seeded from the engine half");

    let (listen, feed) = (free_port(), free_port());
    let config = Config {
        listen,
        feed,
        data_dir: data,
        group: 1,
        node_id: 1,
        tick: Duration::from_millis(50),
        fsync: false,
        capacity: "small".to_string(),
        metrics: None,
    };
    std::thread::spawn(move || {
        let _ = serve(&config);
    });
    // Counted, not clocked: this workspace's readers have no clock of their own.
    for _ in 0..600 {
        if TcpStream::connect(feed).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    (feed, pair_dir)
}

/// Waits for a reader to have caught up past the sequence the venue was founded at.
fn caught_up(live: &Live) -> bool {
    for _ in 0..200 {
        if live.view.lock().expect("the view").sequence > 62 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn a_reader_started_from_the_view_half_holds_the_venue_that_was_founded() {
    let dir = scratch("from-view");
    let (feed, pair_dir) = founded_venue(&dir);
    let taken = View::load(&pair_dir.join(pair::VIEW)).expect("the view half loads");
    assert!(!taken.instruments.is_empty(), "the fixture founded an empty venue; the test proves nothing");

    let live = Live::follow_from(&feed.to_string(), &pair_dir.join(pair::VIEW)).expect("follows from the view half");
    assert!(caught_up(&live), "the reader never caught up with the founded venue");

    let view = live.view.lock().expect("the view");
    assert_eq!(
        view.instruments.keys().collect::<Vec<_>>(),
        taken.instruments.keys().collect::<Vec<_>>(),
        "the reader does not hold the markets the venue was founded with"
    );
    assert_eq!(view.ladders, taken.ladders, "the reader has no tick ladders; it could not price anything");
    assert!(view.sequence > 62, "the reader did not carry on past the sequence it was founded at");
}

/// The same venue, read the way every reader reads one today. This is the gap the view half closes, and it is
/// worth having it fail loudly here if it ever stops being true — an empty view is what "seeded the node and
/// nothing else" actually looks like from outside.
#[test]
fn a_reader_that_asks_for_everything_sees_an_empty_founded_venue() {
    let dir = scratch("from-nothing");
    let (feed, pair_dir) = founded_venue(&dir);
    let taken = View::load(&pair_dir.join(pair::VIEW)).expect("the view half loads");
    assert!(!taken.instruments.is_empty());

    let live = Live::follow(&feed.to_string());
    assert!(caught_up(&live), "the reader never reached the founded venue's tail");

    let view = live.view.lock().expect("the view");
    assert!(
        view.instruments.is_empty() && view.ladders.is_empty(),
        "a reader asking a founded venue for everything now gets state; this test is out of date, and \
         `a_reader_started_from_the_view_half_holds_the_venue_that_was_founded` should be the only path"
    );
}

//! A single-member node for laptops and test environments (blueprint §16, work package WP-8).
//!
//! The smallest thing that runs the engine as a service: a simulated sequencer obeying spec §17, a journal file
//! (interface 11), the result log beside it (interface 20), and a TCP ingress for tools, bots and a gateway.
//! Production puts the engine behind Aeron Cluster instead (WP-10). Nothing here is on that path, and nothing
//! the service does produces a latency number. `bench` and `load` are separate measurement tools: `bench` times
//! the engine alone on a recorded journal and `load` drives a running node's ingress, to compare builds and code,
//! never to state the venue's latency.

pub mod bench;
pub mod frames;
pub mod load;
pub mod metrics;
pub mod node;
pub mod sequencer;

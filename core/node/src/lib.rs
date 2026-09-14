//! A single-member node for laptops and test environments (blueprint §16, work package WP-8).
//!
//! The smallest thing that runs the engine as a service: a simulated sequencer obeying spec §17, a journal file
//! (interface 11), the result log beside it (interface 20), and a TCP ingress for tools, bots and a gateway.
//! Production puts the engine behind Aeron Cluster instead (WP-10). Nothing here is on that path, and nothing
//! here may produce a latency number.

pub mod frames;
pub mod node;
pub mod sequencer;

//! Talking to a node from outside the engine: inputs through its ingress, everything that happened through its
//! feed, a projection of the feed, and the demo catalogue that names what the engine only numbers. The gateway and
//! the mini venue's tools use it; nothing on the engine's side of interface 7 does.

pub mod catalogue;
pub mod submit;
pub mod view;
pub mod wire;

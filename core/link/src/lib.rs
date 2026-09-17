//! Talking to a node from outside the engine: inputs through its ingress, everything that happened through its
//! feed, a projection of the feed, and the demo catalogue that names what the engine only numbers. The gateway and
//! the mini venue's tools use it; nothing on the engine's side of interface 7 does.

pub mod catalogue;
pub mod submit;
pub mod view;
pub mod wire;

/// Spawns a long-lived thread under a name, so `top`, a profile and /proc/<pid>/task say which thread is which: a
/// gateway's CPU time is worth little as one number when six threads share the work.
pub fn named<T: Send + 'static>(name: &str, body: impl FnOnce() -> T + Send + 'static) -> std::thread::JoinHandle<T> {
    // Panics as `thread::spawn` itself does when a thread cannot be started.
    std::thread::Builder::new().name(name.into()).spawn(body).expect("spawning a named thread")
}

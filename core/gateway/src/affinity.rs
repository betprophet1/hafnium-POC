//! Pinning a reactor thread to one core.
//!
//! The blueprint already makes this argument for the matching engine's own threads ("pin the consensus module
//! thread alongside the engine cores", blueprint §15) but never applied it to the gateway's reactor pool.
//! Measurement showed why that matters: raising `--reactor-threads` bought nothing at all once threads
//! outnumbered physical cores on an unpinned host, because nothing kept a thread on the core whose cache it had
//! warmed -- the scheduler was free to bounce it to any other core, including a hyperthread sibling already
//! doing unrelated work.
//!
//! `core_affinity` wraps the platform call in a safe API (this crate cannot write `unsafe` itself: the
//! workspace's `forbid(unsafe_code)` lint applies here too, not only in `core/engine`). Its own list of cores
//! already reflects whatever the OS actually allows this process to run on -- a container's cpuset or a
//! `taskset` invocation constrains it the same way it would constrain a raw `sched_getaffinity` call -- so
//! pinning to one of these is never pinning to a core the process was already excluded from.

use core_affinity::CoreId;

/// The cores this process may actually use, in a fixed order: every reactor thread pinned through
/// `pin_current_thread` with the same list agrees on what index N means, so a set of loops spread themselves
/// across distinct cores rather than piling onto the first one every list happens to start with.
pub fn cores() -> Vec<CoreId> {
    core_affinity::get_core_ids().unwrap_or_default()
}

/// Restricts the calling thread to `cores[index % cores.len()]`, round-robining once there are more reactor
/// threads than cores. A no-op if `cores` is empty (the platform's affinity could not be read, or this build is
/// not on Linux) -- an unpinned reactor thread is a working one, just without the guarantee, not a reason to
/// fail startup over.
pub fn pin_current_thread(cores: &[CoreId], index: usize) {
    if let Some(&core) = cores.get(index % cores.len().max(1)) {
        core_affinity::set_for_current(core);
    }
}

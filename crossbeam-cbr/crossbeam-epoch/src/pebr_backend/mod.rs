//! Epoch-based memory reclamation.
//!
//! An interesting problem concurrent collections deal with comes from the remove operation.
//! Suppose that a thread removes an element from a lock-free map, while another thread is reading
//! that same element at the same time. The first thread must wait until the second thread stops
//! reading the element. Only then it is safe to destruct it.
//!
//! Programming languages that come with garbage collectors solve this problem trivially. The
//! garbage collector will destruct the removed element when no thread can hold a reference to it
//! anymore.
//!
//! This crate implements a basic memory reclamation mechanism, which is based on epochs. When an
//! element gets removed from a concurrent collection, it is inserted into a pile of garbage and
//! marked with the current epoch. Every time a thread accesses a collection, it checks the current
//! epoch, attempts to increment it, and destructs some garbage that became so old that no thread
//! can be referencing it anymore.
//!
//! That is the general mechanism behind epoch-based memory reclamation, but the details are a bit
//! more complicated. Anyhow, memory reclamation is designed to be fully automatic and something
//! users of concurrent collections don't have to worry much about.
//!
//! # Pointers
//!
//! Concurrent collections are built using atomic pointers. This module provides [`Atomic`], which
//! is just a shared atomic pointer to a heap-allocated object. Loading an [`Atomic`] yields a
//! [`Shared`], which is an epoch-protected pointer through which the loaded object can be safely
//! read.
//!
//! # Pinning
//!
//! Before an [`Atomic`] can be loaded, a participant must be [`pin`]ned. By pinning a participant
//! we declare that any object that gets removed from now on must not be destructed just
//! yet. Garbage collection of newly removed objects is suspended until the participant gets
//! unpinned.
//!
//! # Garbage
//!
//! Objects that get removed from concurrent collections must be stashed away until all currently
//! pinned participants get unpinned. Such objects can be stored into a thread-local or global
//! storage, where they are kept until the right time for their destruction comes.
//!
//! There is a global shared instance of garbage queue. You can [`defer`] the execution of an
//! arbitrary function until the global epoch is advanced enough. Most notably, concurrent data
//! structures may defer the deallocation of an object.
//!
//! TODO(@jeehoonkang): discuss hazard pointers.
//!
//! # APIs
//!
//! For majority of use cases, just use the default garbage collector by invoking [`pin`]. If you
//! want to create your own garbage collector, use the [`Collector`] API.
//!
//! [`Atomic`]: struct.Atomic.html
//! [`Collector`]: struct.Collector.html
//! [`Shared`]: struct.Shared.html
//! [`pin`]: fn.pin.html
//! [`defer`]: fn.defer.html

#[cfg_attr(
    feature = "nightly",
    cfg(all(target_has_atomic = "cas", target_has_atomic = "ptr"))
)]
cfg_if! {
    if #[cfg(any(feature = "alloc", feature = "std"))] {
        pub(crate) mod atomic;
        pub(crate) mod collector;
        pub(crate) mod deferred;
        pub(crate) mod garbage;
        pub(crate) mod guard;
        pub(crate) mod hazard;
        pub(crate) mod bloom_filter;
        pub(crate) mod internal;
        pub(crate) mod sync;
        pub(crate) mod tag;
        pub(crate) mod recovery;

        pub use self::atomic::{Atomic, CompareAndSetError, CompareAndSetOrdering, Owned, Pointer, Shared};
        pub use self::collector::{Collector, LocalHandle};
        pub use self::guard::{unprotected, EpochGuard, ReadGuard, WriteGuard, Writable, ReadStatus, Defender};
        pub use self::hazard::{Shield, ShieldError};
    }
}

cfg_if! {
    if #[cfg(feature = "std")] {
        pub mod default;
        pub use self::default::{default_collector, is_pinned, pin};
    }
}

pub use self::internal::GLOBAL_GARBAGE_COUNT;

//! Michael-Scott lock-free queue.
//!
//! Usable with any number of producers and consumers.
//!
//! Michael and Scott.  Simple, Fast, and Practical Non-Blocking and Blocking Concurrent Queue
//! Algorithms.  PODC 1996.  http://dl.acm.org/citation.cfm?id=248106
//!
//! Simon Doherty, Lindsay Groves, Victor Luchangco, and Mark Moir. 2004b. Formal Verification of a
//! Practical Lock-Free Queue Algorithm. https://doi.org/10.1007/978-3-540-30232-2_7

use core::mem::{self, ManuallyDrop};
use core::ptr;
use core::sync::atomic::Ordering::{Acquire, Relaxed, Release};

use crossbeam_utils::CachePadded;

use crossbeam_ebr::{unprotected, Atomic, Guard, Owned, Shared};

// The representation here is a singly-linked list, with a sentinel node at the front. In general
// the `tail` pointer may lag behind the actual tail. Non-sentinel nodes are either all `Data` or
// all `Blocked` (requests for data from blocked threads).
#[derive(Debug)]
pub struct Queue<T> {
    head: CachePadded<Atomic<Node<T>>>,
    tail: CachePadded<Atomic<Node<T>>>,
}

#[derive(Debug)]
struct Node<T> {
    /// The slot in which a value of type `T` can be stored.
    ///
    /// The type of `data` is `ManuallyDrop<T>` because a `Node<T>` doesn't always contain a `T`.
    /// For example, the sentinel node in a queue never contains a value: its slot is always empty.
    /// Other nodes start their life with a push operation and contain a value until it gets popped
    /// out. After that such empty nodes get added to the collector for destruction.
    data: ManuallyDrop<T>,

    next: Atomic<Node<T>>,
}

// Any particular `T` should never be accessed concurrently, so no need for `Sync`.
unsafe impl<T: Send> Sync for Queue<T> {}
unsafe impl<T: Send> Send for Queue<T> {}

impl<T> Queue<T> {
    /// Create a new, empty queue.
    pub fn new() -> Queue<T> {
        let q = Queue {
            head: CachePadded::new(Atomic::null()),
            tail: CachePadded::new(Atomic::null()),
        };
        #[allow(deprecated)]
        let sentinel = Owned::new(Node {
            data: unsafe { mem::uninitialized() },
            next: Atomic::null(),
        });
        unsafe {
            let guard = &unprotected();
            let sentinel = sentinel.into_shared(guard);
            q.head.store(sentinel, Relaxed);
            q.tail.store(sentinel, Relaxed);
            q
        }
    }

    /// Attempts to atomically place `n` into the `next` pointer of `onto`, and returns `true` on
    /// success. The queue's `tail` pointer may be updated.
    #[inline(always)]
    fn push_internal(&self, onto: Shared<Node<T>>, new: Shared<Node<T>>, guard: &Guard) -> bool {
        // is `onto` the actual tail?
        let o = unsafe { onto.deref() };
        let next = o.next.load(Acquire, guard);
        if unsafe { next.as_ref().is_some() } {
            // if not, try to "help" by moving the tail pointer forward
            let _ = self.tail.compare_and_set(onto, next, Release, guard);
            false
        } else {
            // looks like the actual tail; attempt to link in `n`
            let result = o
                .next
                .compare_and_set(Shared::null(), new, Release, guard)
                .is_ok();
            if result {
                // try to move the tail pointer forward
                let _ = self.tail.compare_and_set(onto, new, Release, guard);
            }
            result
        }
    }

    /// Adds `t` to the back of the queue, possibly waking up threads blocked on `pop`.
    pub fn push(&self, t: T, guard: &Guard) {
        let new = Owned::new(Node {
            data: ManuallyDrop::new(t),
            next: Atomic::null(),
        });
        let new = Owned::into_shared(new, guard);

        loop {
            // We push onto the tail, so we'll start optimistically by looking there first.
            let tail = self.tail.load(Acquire, guard);

            // Attempt to push onto the `tail` snapshot; fails if `tail.next` has changed.
            if self.push_internal(tail, new, guard) {
                break;
            }
        }
    }

    /// Attempts to pop a data node. `Ok(None)` if queue is empty; `Err(())` if lost race to pop.
    #[inline(always)]
    fn pop_internal(&self, guard: &Guard) -> Result<Option<T>, ()> {
        let head = self.head.load(Acquire, guard);
        let h = unsafe { head.deref() };
        let next = h.next.load(Acquire, guard);
        match unsafe { next.as_ref() } {
            Some(n) => unsafe {
                self.head
                    .compare_and_set(head, next, Release, guard)
                    .map(|_| {
                        let tail = self.tail.load(Relaxed, guard);
                        // Advance the tail so that we don't retire a pointer to a reachable node.
                        if head == tail {
                            let _ = self.tail.compare_and_set(tail, next, Release, guard);
                        }
                        guard.defer_destroy(head);
                        Some(ManuallyDrop::into_inner(ptr::read(&n.data)))
                    })
                    .map_err(|_| ())
            },
            None => Ok(None),
        }
    }

    /// Attempts to dequeue from the front.
    ///
    /// Returns `None` if the queue is observed to be empty.
    pub fn try_pop(&self, guard: &Guard) -> Option<T> {
        loop {
            if let Ok(head) = self.pop_internal(guard) {
                return head;
            }
        }
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        unsafe {
            let guard = &unprotected();

            while let Some(_) = self.try_pop(guard) {}

            // Destroy the remaining sentinel node.
            let sentinel = self.head.load(Relaxed, guard);
            drop(sentinel.into_owned());
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crossbeam_ebr::pin;
    use crossbeam_utils::thread;

    struct Queue<T> {
        queue: super::Queue<T>,
    }

    impl<T> Queue<T> {
        pub fn new() -> Queue<T> {
            Queue {
                queue: super::Queue::new(),
            }
        }

        pub fn push(&self, t: T) {
            let guard = &pin();
            self.queue.push(t, guard);
        }

        pub fn is_empty(&self) -> bool {
            let guard = &pin();
            let head = self.queue.head.load(Acquire, guard);
            let h = unsafe { head.deref() };
            h.next.load(Acquire, guard).is_null()
        }

        pub fn try_pop(&self) -> Option<T> {
            let guard = &pin();
            self.queue.try_pop(guard)
        }

    }

    const CONC_COUNT: i64 = 1000000;
    const THREADS: i64 = 24;

    #[test]
    fn smoke_queue() {
        // push_try_pop_many_mpmc
        enum LR {
            Left(i64),
            Right(i64),
        }

        let q: Queue<LR> = Queue::new();
        assert!(q.is_empty());

        thread::scope(|scope| {
            for _t in 0..THREADS / 3 {
                scope.spawn(|_| {
                    for i in CONC_COUNT - 1..CONC_COUNT {
                        q.push(LR::Left(i))
                    }
                });
                scope.spawn(|_| {
                    for i in CONC_COUNT - 1..CONC_COUNT {
                        q.push(LR::Right(i))
                    }
                });
                scope.spawn(|_| {
                    let mut vl = vec![];
                    let mut vr = vec![];
                    for _i in 0..CONC_COUNT {
                        match q.try_pop() {
                            Some(LR::Left(x)) => vl.push(x),
                            Some(LR::Right(x)) => vr.push(x),
                            _ => {}
                        }
                    }

                    let mut vl2 = vl.clone();
                    let mut vr2 = vr.clone();
                    vl2.sort();
                    vr2.sort();

                    assert_eq!(vl, vl2);
                    assert_eq!(vr, vr2);
                });
            }
        })
        .unwrap();
    }
}

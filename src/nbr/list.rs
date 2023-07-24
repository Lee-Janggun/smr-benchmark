use super::concurrent_map::ConcurrentMap;
use nbr_rs::{read_phase, Guard};

use hp_pp::{tag, tagged, untagged};
use std::cmp::Ordering::{Equal, Greater, Less};
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

// `#[repr(C)]` is used to ensure the first field
// is also the first data in the memory alignment.
#[repr(C)]
#[derive(Debug)]
struct Node<K, V> {
    next: AtomicPtr<Node<K, V>>,
    key: K,
    value: V,
}

struct List<K, V> {
    head: AtomicPtr<Node<K, V>>,
}

impl<K, V> Drop for List<K, V> {
    fn drop(&mut self) {
        unsafe {
            let mut curr = self.head.load(Ordering::Relaxed);

            while let Some(curr_ref) = untagged(curr).as_ref() {
                let next = curr_ref.next.load(Ordering::Relaxed);
                drop(Box::from_raw(untagged(curr)));
                curr = next;
            }
        }
    }
}

impl<K, V> Node<K, V> {
    fn new(key: K, value: V) -> Self {
        Self {
            next: AtomicPtr::new(ptr::null_mut()),
            key,
            value,
        }
    }
}

struct Cursor<K, V> {
    prev: *mut Node<K, V>,
    curr: *mut Node<K, V>,
    found: bool,
}

impl<K, V> List<K, V>
where
    K: Ord,
{
    pub fn new() -> Self {
        List {
            head: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// Clean up a chain of logically removed nodes in each traversal.
    fn find_harris(&self, key: &K, guard: &Guard) -> Cursor<K, V> {
        let mut cursor;
        let mut prev_next;

        loop {
            read_phase!(guard; [cursor.prev, cursor.curr] => {
                (cursor, prev_next) = {
                    // Declaring inner cursor is important to let the compiler to conduct register
                    // optimization.
                    let mut cursor = Cursor {
                        prev: &self.head as *const _ as *mut Node<K, V>,
                        curr: self.head.load(Ordering::Acquire),
                        found: false,
                    };
                    let mut prev_next = cursor.curr;

                    // Finding phase
                    // - cursor.curr: first unmarked node w/ key >= search key (4)
                    // - cursor.prev: the ref of .next in previous unmarked node (1 -> 2)
                    // 1 -> 2 -x-> 3 -x-> 4 -> 5 -> ∅  (search key: 4)

                    cursor.found = loop {
                        let curr_node = some_or!(unsafe { cursor.curr.as_ref() }, break false);
                        let next = curr_node.next.load(Ordering::Acquire);

                        // - finding stage is done if cursor.curr advancement stops
                        // - advance cursor.curr if (.next is marked) || (cursor.curr < key)
                        // - stop cursor.curr if (not marked) && (cursor.curr >= key)
                        // - advance cursor.prev if not marked

                        if tag(next) != 0 {
                            // We add a 0 tag here so that `cursor.curr`s tag is always 0.
                            cursor.curr = tagged(next, 0);
                            continue;
                        }

                        match curr_node.key.cmp(key) {
                            Less => {
                                cursor.prev = cursor.curr;
                                cursor.curr = next;
                                prev_next = next;
                            }
                            Equal => break true,
                            Greater => break false,
                        }
                    };
                    (cursor, prev_next)
                };
            });

            // If prev and curr WERE adjacent, no need to clean up
            if prev_next == cursor.curr {
                return cursor;
            }

            // cleanup marked nodes between prev and curr
            let prev_ref = unsafe { &*cursor.prev };
            if prev_ref
                .next
                .compare_exchange(prev_next, cursor.curr, Ordering::Release, Ordering::Relaxed)
                .is_err()
            {
                continue;
            }

            // retire from cursor.prev.load() to cursor.curr (exclusive)
            let mut node = prev_next;
            while tagged(node, 0) != cursor.curr {
                let next = unsafe { &*untagged(node) }.next.load(Ordering::Acquire);
                unsafe { guard.retire(untagged(node)) };
                node = next;
            }

            return cursor;
        }
    }

    fn find_harris_michael(&self, key: &K, guard: &Guard) -> Cursor<K, V> {
        let mut cursor;
        let mut removed_next;

        loop {
            read_phase!(guard; [cursor.prev, cursor.curr] => {
                (cursor, removed_next) = {
                    // Declaring inner cursor is important to let the compiler to conduct register
                    // optimization.
                    let mut cursor = Cursor {
                        prev: &self.head as *const _ as *mut Node<K, V>,
                        curr: self.head.load(Ordering::Acquire),
                        found: false,
                    };
                    let mut removed_next = ptr::null_mut();

                    cursor.found = loop {
                        let curr_node = some_or!(unsafe { cursor.curr.as_ref() }, break false);
                        let next = curr_node.next.load(Ordering::Acquire);

                        // NOTE: original version aborts here if self.prev is tagged

                        if tag(next) != 0 {
                            // Found a logically removed node.
                            // As it cannot be physically removed in read phase,
                            // save it at a local variable and remove it
                            // in write phase.
                            removed_next = untagged(next);
                            break false;
                        }

                        match curr_node.key.cmp(key) {
                            Less => {
                                cursor.prev = cursor.curr;
                                cursor.curr = next;
                            }
                            Equal => break true,
                            Greater => break false,
                        }
                    };
                    (cursor, removed_next)
                };
            });

            if !removed_next.is_null() {
                let prev_ref = unsafe { &*cursor.prev };
                if prev_ref
                    .next
                    .compare_exchange(
                        cursor.curr,
                        removed_next,
                        Ordering::Release,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    unsafe { guard.retire(cursor.curr) };
                }
                continue;
            }

            return cursor;
        }
    }

    /// Gotta go fast. Doesn't fail.
    fn find_harris_herlihy_shavit(&self, key: &K, guard: &Guard) -> Cursor<K, V> {
        let mut cursor;

        read_phase!(guard; [cursor.prev, cursor.curr] => {
            cursor = {
                // Declaring inner cursor is important to let the compiler to conduct register
                // optimization.
                let mut cursor = Cursor {
                    prev: &self.head as *const _ as *mut Node<K, V>,
                    curr: self.head.load(Ordering::Acquire),
                    found: false,
                };

                cursor.found = loop {
                    let curr_node = some_or!(unsafe { untagged(cursor.curr).as_ref() }, break false);

                    match curr_node.key.cmp(key) {
                        Less => {
                            cursor.prev = cursor.curr;
                            cursor.curr = curr_node.next.load(Ordering::Acquire);
                        }
                        Equal => break tag(curr_node.next.load(Ordering::Relaxed)) == 0,
                        Greater => break false,
                    }
                };
                cursor.curr = untagged(cursor.curr);
                cursor.prev = untagged(cursor.prev);
                cursor
            };
        });

        return cursor;
    }

    pub fn get<'g, F>(&'g self, key: &K, find: F, guard: &'g Guard) -> Option<&'g V>
    where
        F: Fn(&List<K, V>, &K, &Guard) -> Cursor<K, V>,
    {
        let cursor = find(self, key, guard);
        if cursor.found {
            unsafe { cursor.curr.as_ref() }.map(|n| &n.value)
        } else {
            None
        }
    }

    pub fn insert<F>(&self, key: K, value: V, find: F, guard: &Guard) -> bool
    where
        F: Fn(&List<K, V>, &K, &Guard) -> Cursor<K, V>,
    {
        let mut new_node = Box::new(Node::new(key, value));
        loop {
            let cursor = find(self, &new_node.key, guard);
            if cursor.found {
                return false;
            }

            new_node.next.store(cursor.curr, Ordering::Relaxed);
            let new_node_ptr = Box::into_raw(new_node);

            match unsafe { &*cursor.prev }.next.compare_exchange(
                cursor.curr,
                new_node_ptr,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(_) => new_node = unsafe { Box::from_raw(new_node_ptr) },
            }
        }
    }

    pub fn remove<'g, F>(&'g self, key: &K, find: F, guard: &'g Guard) -> Option<&'g V>
    where
        F: Fn(&List<K, V>, &K, &Guard) -> Cursor<K, V>,
    {
        loop {
            let cursor = find(self, key, guard);
            if !cursor.found {
                return None;
            }

            let curr_node = unsafe { &*cursor.curr };
            let next = curr_node.next.fetch_or(1, Ordering::AcqRel);
            if tag(next) == 1 {
                continue;
            }

            let prev_ref = unsafe { &*cursor.prev };
            if prev_ref
                .next
                .compare_exchange(cursor.curr, next, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                unsafe { guard.retire(cursor.curr) };
            }
            return Some(&curr_node.value);
        }
    }

    fn pop<'g>(&self, guard: &'g Guard) -> Option<(&'g K, &'g V)> {
        loop {
            let mut cursor = Cursor {
                prev: ptr::null_mut(),
                curr: ptr::null_mut(),
                found: false,
            };
            read_phase!(guard; [cursor.prev, cursor.curr] => {
                cursor.prev = &self.head as *const _ as *mut Node<K, V>;
                cursor.curr = self.head.load(Ordering::Acquire);
            });

            let curr_node = match unsafe { cursor.curr.as_ref() } {
                Some(node) => node,
                None => return None,
            };

            let next = curr_node.next.fetch_or(1, Ordering::AcqRel);

            if (tag(next) & 1) != 0 {
                continue;
            }

            if unsafe { &*cursor.prev }
                .next
                .compare_exchange(cursor.curr, next, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                unsafe { guard.retire(cursor.curr) };
            }
            return Some((&curr_node.key, &curr_node.value));
        }
    }

    /// Omitted
    pub fn harris_get<'g>(&'g self, key: &K, guard: &'g Guard) -> Option<&'g V> {
        self.get(key, Self::find_harris, guard)
    }

    /// Omitted
    pub fn harris_insert<'g>(&'g self, key: K, value: V, guard: &'g Guard) -> bool {
        self.insert(key, value, Self::find_harris, guard)
    }

    /// Omitted
    pub fn harris_remove<'g>(&'g self, key: &K, guard: &'g Guard) -> Option<&'g V> {
        self.remove(key, Self::find_harris, guard)
    }

    /// Omitted
    pub fn harris_michael_get<'g>(&'g self, key: &K, guard: &'g Guard) -> Option<&'g V> {
        self.get(key, Self::find_harris_michael, guard)
    }

    /// Omitted
    pub fn harris_michael_insert(&self, key: K, value: V, guard: &Guard) -> bool {
        self.insert(key, value, Self::find_harris_michael, guard)
    }

    /// Omitted
    pub fn harris_michael_remove<'g>(&'g self, key: &K, guard: &'g Guard) -> Option<&'g V> {
        self.remove(key, Self::find_harris_michael, guard)
    }

    /// Omitted
    pub fn harris_herlihy_shavit_get<'g>(&'g self, key: &K, guard: &'g Guard) -> Option<&'g V> {
        self.get(key, Self::find_harris_herlihy_shavit, guard)
    }
}

pub struct HList<K, V> {
    inner: List<K, V>,
}

impl<K, V> ConcurrentMap<K, V> for HList<K, V>
where
    K: Ord,
{
    fn new() -> Self {
        HList { inner: List::new() }
    }

    #[inline]
    fn get<'g>(&'g self, key: &K, guard: &'g Guard) -> Option<&'g V> {
        self.inner.harris_get(key, guard)
    }
    #[inline]
    fn insert(&self, key: K, value: V, guard: &Guard) -> bool {
        self.inner.harris_insert(key, value, guard)
    }
    #[inline]
    fn remove<'g>(&'g self, key: &K, guard: &'g Guard) -> Option<&'g V> {
        self.inner.harris_remove(key, guard)
    }
}

pub struct HMList<K, V> {
    inner: List<K, V>,
}

impl<K, V> HMList<K, V>
where
    K: Ord,
{
    /// For optimistic search on HashMap
    #[inline]
    pub fn get_harris_herlihy_shavit<'g>(&'g self, key: &K, guard: &'g Guard) -> Option<&'g V> {
        self.inner.harris_herlihy_shavit_get(key, guard)
    }
}

impl<K, V> ConcurrentMap<K, V> for HMList<K, V>
where
    K: Ord,
{
    fn new() -> Self {
        HMList { inner: List::new() }
    }

    #[inline]
    fn get<'g>(&'g self, key: &K, guard: &'g Guard) -> Option<&'g V> {
        self.inner.harris_michael_get(key, guard)
    }
    #[inline]
    fn insert(&self, key: K, value: V, guard: &Guard) -> bool {
        self.inner.harris_michael_insert(key, value, guard)
    }
    #[inline]
    fn remove<'g>(&'g self, key: &K, guard: &'g Guard) -> Option<&'g V> {
        self.inner.harris_michael_remove(key, guard)
    }
}

pub struct HHSList<K, V> {
    inner: List<K, V>,
}

impl<K, V> HHSList<K, V>
where
    K: Ord,
{
    pub fn pop<'g>(&self, guard: &'g Guard) -> Option<(&'g K, &'g V)> {
        self.inner.pop(guard)
    }
}

impl<K, V> ConcurrentMap<K, V> for HHSList<K, V>
where
    K: Ord,
{
    fn new() -> Self {
        HHSList { inner: List::new() }
    }

    #[inline]
    fn get<'g>(&'g self, key: &K, guard: &'g Guard) -> Option<&'g V> {
        self.inner.harris_herlihy_shavit_get(key, guard)
    }
    #[inline]
    fn insert(&self, key: K, value: V, guard: &Guard) -> bool {
        self.inner.harris_insert(key, value, guard)
    }
    #[inline]
    fn remove<'g>(&'g self, key: &K, guard: &'g Guard) -> Option<&'g V> {
        self.inner.harris_remove(key, guard)
    }
}

#[cfg(test)]
mod tests {
    use super::{HHSList, HList, HMList};
    use crate::nbr::concurrent_map;

    #[test]
    fn smoke_h_list() {
        concurrent_map::tests::smoke::<HList<i32, String>>(2);
    }

    #[test]
    fn smoke_hm_list() {
        concurrent_map::tests::smoke::<HMList<i32, String>>(2);
    }

    #[test]
    fn smoke_hhs_list() {
        concurrent_map::tests::smoke::<HHSList<i32, String>>(2);
    }

    #[test]
    fn litmus_hhs_pop() {
        use concurrent_map::ConcurrentMap;
        let map = HHSList::new();

        let guard = unsafe { nbr_rs::unprotected() };
        map.insert(1, "1", guard);
        map.insert(2, "2", guard);
        map.insert(3, "3", guard);

        fn assert_eq(a: (&i32, &&str), b: (i32, &str)) {
            assert_eq!(*a.0, b.0);
            assert_eq!(*a.1, b.1);
        }

        assert_eq(map.pop(guard).unwrap(), (1, "1"));
        assert_eq(map.pop(guard).unwrap(), (2, "2"));
        assert_eq(map.pop(guard).unwrap(), (3, "3"));
        assert_eq!(map.pop(guard), None);
    }
}

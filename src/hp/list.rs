use super::concurrent_map::ConcurrentMap;

use std::cmp::Ordering::{Equal, Greater, Less};
use std::mem;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

use hp_pp::{decompose_ptr, light_membarrier, retire, tag, untagged, HazardPointer};

#[derive(Debug)]
pub struct Node<K, V> {
    /// Mark: tag(), Tag: not needed
    next: AtomicPtr<Node<K, V>>,
    key: K,
    value: V,
}

pub struct List<K, V> {
    head: AtomicPtr<Node<K, V>>,
}

impl<K, V> Default for List<K, V>
where
    K: Ord,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> Drop for List<K, V> {
    fn drop(&mut self) {
        unsafe {
            let mut curr = *self.head.get_mut();

            while !curr.is_null() {
                let next = untagged(*(*curr).next.get_mut());
                drop(Box::from_raw(curr));
                curr = next;
            }
        }
    }
}

pub struct Handle<'domain> {
    prev_h: HazardPointer<'domain>,
    curr_h: HazardPointer<'domain>,
}

impl Default for Handle<'static> {
    fn default() -> Self {
        Self {
            prev_h: HazardPointer::default(),
            curr_h: HazardPointer::default(),
        }
    }
}

impl<'domain> Handle<'domain> {
    // bypass E0499-E0503, etc that are supposed to be fixed by polonius
    #[inline]
    fn launder<'hp1, 'hp2>(&'hp1 mut self) -> &'hp2 mut Self {
        unsafe { core::mem::transmute(self) }
    }
}

pub struct Cursor<'domain, 'hp, K, V> {
    prev: *mut Node<K, V>, // not &AtomicPtr because we can't construct the cursor out of thin air
    curr: *mut Node<K, V>,
    handle: &'hp mut Handle<'domain>,
}

impl<'domain, 'hp, K, V> Cursor<'domain, 'hp, K, V> {
    pub fn new(head: &AtomicPtr<Node<K, V>>, handle: &'hp mut Handle<'domain>) -> Self {
        Self {
            prev: head as *const _ as *mut _,
            curr: head.load(Ordering::Acquire),
            handle,
        }
    }
}

impl<'domain, 'hp, K, V> Cursor<'domain, 'hp, K, V>
where
    K: Ord,
{
    #[inline]
    fn find_harris_michael(&mut self, key: &K) -> Result<bool, ()> {
        loop {
            debug_assert_eq!(tag(self.curr), 0);
            if self.curr.is_null() {
                return Ok(false);
            }

            let prev = unsafe { &(*self.prev).next };

            self.handle.curr_h.protect_raw(self.curr);
            light_membarrier();
            if prev.load(Ordering::Acquire) != self.curr {
                return Err(());
            }

            let curr_node = unsafe { &*self.curr };

            let next = curr_node.next.load(Ordering::Acquire);
            let (next_base, next_tag) = decompose_ptr(next);

            if next_tag == 0 {
                match curr_node.key.cmp(key) {
                    Less => {
                        mem::swap(&mut self.prev, &mut self.curr);
                        mem::swap(&mut self.handle.prev_h, &mut self.handle.curr_h);
                    }
                    Equal => return Ok(true),
                    Greater => return Ok(false),
                }
            } else if prev
                .compare_exchange(self.curr, next_base, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                unsafe { retire(self.curr) };
            } else {
                return Err(());
            }
            self.curr = next_base;
        }
    }
}

impl<K, V> List<K, V>
where
    K: Ord,
{
    /// Creates a new list.
    pub fn new() -> Self {
        List {
            head: AtomicPtr::new(ptr::null_mut()),
        }
    }

    #[inline]
    fn get<'domain, 'hp, F>(
        &self,
        key: &K,
        find: F,
        handle: &'hp mut Handle<'domain>,
    ) -> Option<&'hp V>
    where
        F: Fn(&mut Cursor<'domain, 'hp, K, V>, &K) -> Result<bool, ()>,
    {
        loop {
            let mut cursor = Cursor::new(&self.head, handle.launder());
            match find(&mut cursor, key) {
                Ok(true) => return unsafe { Some(&((*cursor.curr).value)) },
                Ok(false) => return None,
                Err(_) => continue,
            }
        }
    }

    fn insert_inner<'domain, 'hp, F>(
        &self,
        node: *mut Node<K, V>,
        find: &F,
        handle: &'hp mut Handle<'domain>,
    ) -> Result<bool, ()>
    where
        F: Fn(&mut Cursor<'domain, 'hp, K, V>, &K) -> Result<bool, ()>,
    {
        loop {
            let mut cursor = Cursor::new(&self.head, handle.launder());
            let found = find(&mut cursor, unsafe { &(*node).key })?;
            if found {
                drop(unsafe { Box::from_raw(node) });
                return Ok(false);
            }

            unsafe { &*node }.next.store(cursor.curr, Ordering::Relaxed);
            if unsafe { &*cursor.prev }
                .next
                .compare_exchange(cursor.curr, node, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(true);
            }
        }
    }

    #[inline]
    fn insert<'domain, 'hp, F>(
        &self,
        key: K,
        value: V,
        find: F,
        handle: &'hp mut Handle<'domain>,
    ) -> bool
    where
        F: Fn(&mut Cursor<'domain, 'hp, K, V>, &K) -> Result<bool, ()>,
    {
        let node = Box::into_raw(Box::new(Node {
            key,
            value,
            next: AtomicPtr::new(ptr::null_mut()),
        }));

        loop {
            match self.insert_inner(node, &find, handle.launder()) {
                Ok(r) => return r,
                Err(()) => continue,
            }
        }
    }

    fn remove_inner<'domain, 'hp, F>(
        &self,
        key: &K,
        find: &F,
        handle: &'hp mut Handle<'domain>,
    ) -> Result<Option<&'hp V>, ()>
    where
        F: Fn(&mut Cursor<'domain, 'hp, K, V>, &K) -> Result<bool, ()>,
    {
        loop {
            let mut cursor = Cursor::new(&self.head, handle.launder());
            let found = find(&mut cursor, key)?;
            if !found {
                return Ok(None);
            }

            let curr_node = unsafe { &*cursor.curr };
            let next = curr_node.next.fetch_or(1, Ordering::Relaxed);
            let next_tag = tag(next);
            if next_tag == 1 {
                continue;
            }

            let prev = unsafe { &(*cursor.prev).next };

            if prev
                .compare_exchange(cursor.curr, next, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                unsafe { retire(cursor.curr) };
            }

            return Ok(Some(&curr_node.value));
        }
    }

    #[inline]
    fn remove<'domain, 'hp, F>(
        &self,
        key: &K,
        find: F,
        handle: &'hp mut Handle<'domain>,
    ) -> Option<&'hp V>
    where
        F: Fn(&mut Cursor<'domain, 'hp, K, V>, &K) -> Result<bool, ()>,
    {
        loop {
            match self.remove_inner(key, &find, handle.launder()) {
                Ok(r) => return r,
                Err(_) => continue,
            }
        }
    }

    pub fn harris_michael_get<'domain, 'hp>(
        &self,
        key: &K,
        handle: &'hp mut Handle<'domain>,
    ) -> Option<&'hp V> {
        self.get(key, Cursor::find_harris_michael, handle)
    }

    pub fn harris_michael_insert<'domain, 'hp>(
        &self,
        key: K,
        value: V,
        handle: &'hp mut Handle<'domain>,
    ) -> bool {
        self.insert(key, value, Cursor::find_harris_michael, handle)
    }

    pub fn harris_michael_remove<'domain, 'hp>(
        &self,
        key: &K,
        handle: &'hp mut Handle<'domain>,
    ) -> Option<&'hp V> {
        self.remove(key, Cursor::find_harris_michael, handle)
    }
}

pub struct HMList<K, V> {
    inner: List<K, V>,
}

impl<K, V> ConcurrentMap<K, V> for HMList<K, V>
where
    K: Ord,
{
    type Handle<'domain> = Handle<'domain>;

    fn handle() -> Self::Handle<'static> {
        Handle::default()
    }

    fn new() -> Self {
        HMList { inner: List::new() }
    }

    #[inline]
    fn get<'domain, 'hp>(&self, handle: &'hp mut Self::Handle<'domain>, key: &K) -> Option<&'hp V> {
        self.inner.harris_michael_get(key, handle)
    }
    #[inline]
    fn insert<'domain, 'hp>(
        &self,
        handle: &'hp mut Self::Handle<'domain>,
        key: K,
        value: V,
    ) -> bool {
        self.inner.harris_michael_insert(key, value, handle)
    }
    #[inline]
    fn remove<'domain, 'hp>(
        &self,
        handle: &'hp mut Self::Handle<'domain>,
        key: &K,
    ) -> Option<&'hp V> {
        self.inner.harris_michael_remove(key, handle)
    }
}

#[cfg(test)]
mod tests {
    use super::HMList;
    use crate::hp::concurrent_map;

    #[test]
    fn smoke_hm_list() {
        concurrent_map::tests::smoke::<HMList<i32, String>>();
    }
}

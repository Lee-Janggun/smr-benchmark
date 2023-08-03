use std::{
    cell::RefCell,
    collections::VecDeque,
    marker::PhantomData,
    mem::{align_of, zeroed},
    ptr::null_mut,
    sync::atomic::AtomicU64,
};

use arrayvec::ArrayVec;
use atomic::{Atomic, Ordering};
use crossbeam_utils::CachePadded;
use portable_atomic::{compiler_fence, AtomicU128};

pub const ENTRIES_PER_BAG: usize = 64;
pub const INIT_BAGS_PER_LOCAL: usize = 16;
pub const NOT_RETIRED: u64 = u64::MAX;

pub struct Ver<T> {
    birth: AtomicU64,
    retire: AtomicU64,
    data: T,
}

pub struct Global<T> {
    epoch: CachePadded<AtomicU64>,
    avail: BagStack<Ver<T>>,
}

unsafe impl<T> Sync for Global<T> {}
unsafe impl<T> Send for Global<T> {}

impl<T> Global<T> {
    pub fn new(capacity: usize) -> Self {
        let avail = BagStack::new();
        let count = capacity / ENTRIES_PER_BAG + if capacity % ENTRIES_PER_BAG > 0 { 1 } else { 0 };
        for _ in 0..count {
            avail.push(Box::into_raw(Box::new(Bag::new_with_alloc())));
        }
        Self {
            epoch: CachePadded::new(AtomicU64::new(0)),
            avail,
        }
    }

    pub fn epoch(&self) -> u64 {
        // On weakly-ordered systems (e.g., ARM, PowerPc, etc.), reads must be ordered using
        // special CPU load or memory fence instructions.
        if cfg!(not(any(target_arch = "x86", target_arch = "x86_64"))) {
            core::sync::atomic::fence(Ordering::SeqCst);
        }
        self.epoch.load(Ordering::Acquire)
    }

    pub fn advance(&self, expected: u64) -> Result<u64, u64> {
        match self.epoch.compare_exchange(
            expected,
            expected + 1,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => Ok(expected + 1),
            Err(_) => Err(expected),
        }
    }

    pub fn acquire(&self) -> *mut Bag<Ver<T>> {
        loop {
            if let Some(bag) = self.avail.pop() {
                return bag;
            } else {
                self.avail
                    .push(Box::into_raw(Box::new(Bag::new_with_alloc())));
            }
        }
    }

    pub fn retire(&self, bag: *mut Bag<Ver<T>>) {
        self.avail.push(bag);
    }
}

pub struct BagStack<T> {
    /// NOTE: A timestamp is necessary to prevent ABA problems.
    head: AtomicU128,
    _marker: PhantomData<T>,
}

impl<T> BagStack<T> {
    fn new() -> Self {
        Self {
            head: AtomicU128::new(0),
            _marker: PhantomData,
        }
    }

    pub fn pop(&self) -> Option<*mut Bag<T>> {
        loop {
            let (ts, head) = decompose_u128::<Bag<T>>(self.head.load(Ordering::Acquire));

            if let Some(head_ref) = unsafe { head.as_ref() } {
                let next = head_ref.next.load(Ordering::Acquire);
                if self
                    .head
                    .compare_exchange(
                        compose_u128(ts, head),
                        next,
                        Ordering::Release,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    head_ref.next.store(0, Ordering::Release);
                    return Some(head);
                }
            } else {
                return None;
            }
        }
    }

    pub fn push(&self, bag: *mut Bag<T>) {
        debug_assert!(!bag.is_null());
        loop {
            let (ts, head) = decompose_u128::<Bag<T>>(self.head.load(Ordering::Acquire));
            unsafe { &*bag }
                .next
                .store(compose_u128(ts, head), Ordering::Release);
            if self
                .head
                .compare_exchange(
                    compose_u128(ts, head),
                    compose_u128(ts + 1, bag),
                    Ordering::Release,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                return;
            }
        }
    }
}

impl<T> Drop for BagStack<T> {
    fn drop(&mut self) {
        let mut head = decompose_u128::<Bag<T>>(self.head.load(Ordering::Relaxed)).1;
        while !head.is_null() {
            head = decompose_u128::<Bag<T>>(
                unsafe { Box::from_raw(head) }.next.load(Ordering::Relaxed),
            )
            .1;
        }
    }
}

pub struct Bag<T> {
    /// NOTE: A timestamp is necessary to prevent ABA problems.
    next: AtomicU128,
    entries: ArrayVec<*mut T, ENTRIES_PER_BAG>,
}

impl<T> Bag<T> {
    fn new() -> Self {
        Self {
            next: AtomicU128::new(0),
            entries: ArrayVec::new(),
        }
    }

    fn new_with_alloc() -> Self {
        let mut alloc = [null_mut(); ENTRIES_PER_BAG];
        for ptr in &mut alloc {
            *ptr = unsafe { Box::into_raw(Box::new(zeroed())) };
        }
        Self {
            next: AtomicU128::new(0),
            entries: ArrayVec::from(alloc),
        }
    }

    fn push(&mut self, obj: *mut T) -> bool {
        self.entries.try_push(obj).is_ok()
    }

    fn pop(&mut self) -> Option<*mut T> {
        self.entries.pop()
    }
}

pub struct Local<T> {
    global: *const Global<T>,
    avail: RefCell<VecDeque<*mut Bag<Ver<T>>>>,
    retired: RefCell<VecDeque<*mut Bag<Ver<T>>>>,
}

impl<T> Local<T> {
    fn global(&self) -> &Global<T> {
        unsafe { &*self.global }
    }

    pub fn new(global: &Global<T>) -> Self {
        let mut avail = VecDeque::with_capacity(INIT_BAGS_PER_LOCAL);
        avail.resize_with(INIT_BAGS_PER_LOCAL, || global.acquire());
        let mut retired = VecDeque::new();
        retired.push_back(Box::into_raw(Box::new(Bag::new())));
        Self {
            global,
            avail: RefCell::new(avail),
            retired: RefCell::new(retired),
        }
    }

    fn pop_avail(&self) -> *mut Ver<T> {
        loop {
            // Try acquiring an available slot from a thread-local bag.
            loop {
                let bag = match self.avail.borrow().front() {
                    Some(bag) => *bag,
                    None => break,
                };
                let bag_ref = unsafe { &mut *bag };
                if let Some(item) = bag_ref.pop() {
                    return item;
                } else {
                    self.avail.borrow_mut().pop_front();
                    self.retired.borrow_mut().push_back(bag);
                }
            }

            // Acquire some fresh bags from the global and try again.
            self.avail
                .borrow_mut()
                .resize_with(INIT_BAGS_PER_LOCAL, || self.global().acquire());
        }
    }

    fn return_avail(&self, ver: *mut Ver<T>) {
        let bag = *self.avail.borrow().front().unwrap();
        let bag_ref = unsafe { &mut *bag };
        bag_ref.push(ver);
    }

    fn push_retired(&self, ver: *mut Ver<T>) {
        // Try find an available slot from a thread-local bag.
        loop {
            let bag = match self.retired.borrow().front() {
                Some(bag) => *bag,
                None => break,
            };
            let bag_ref = unsafe { &mut *bag };
            if bag_ref.push(ver) {
                return;
            } else {
                self.retired.borrow_mut().pop_front();
                self.global().retire(bag);
            }
        }

        // Create a fresh bag to store a node.
        let mut bag = Box::new(Bag::new());
        bag.push(ver);
        self.retired.borrow_mut().push_back(Box::into_raw(bag));
    }

    pub fn guard(&self) -> Guard<T> {
        Guard {
            local: self,
            epoch: self.global().epoch(),
        }
    }
}

pub struct Guard<T> {
    local: *const Local<T>,
    epoch: u64,
}

impl<T> Guard<T> {
    fn global(&self) -> &Global<T> {
        unsafe { &*self.local().global }
    }

    fn local(&self) -> &Local<T> {
        unsafe { &*self.local }
    }

    pub fn allocate(&self) -> Result<Shared<'_, T>, ()> {
        let ptr = self.local().pop_avail();
        debug_assert!(!ptr.is_null());
        let slot_ref = unsafe { &*ptr };
        if self.epoch <= slot_ref.retire.load(Ordering::Acquire) {
            self.local().return_avail(ptr);
            let _ = self.global().advance(self.epoch);
            return Err(());
        }

        slot_ref.birth.store(self.epoch, Ordering::Release);
        slot_ref.retire.store(NOT_RETIRED, Ordering::Release);
        Ok(Shared {
            ptr,
            birth: self.epoch,
            _marker: PhantomData,
        })
    }

    pub unsafe fn retire<'g>(&self, ptr: Shared<'g, T>) -> Result<(), ()> {
        let ver = ptr
            .as_versioned()
            .expect("Attempted to retire a null pointer.");

        if ver.birth.load(Ordering::Acquire) > ptr.birth
            || ver.retire.load(Ordering::Acquire) != NOT_RETIRED
        {
            return Ok(());
        }

        let curr_epoch = self.global().epoch();
        ver.retire.store(curr_epoch, Ordering::Release);
        self.local().push_retired((ver as *const Ver<T>).cast_mut());
        if self.epoch < curr_epoch {
            return Err(());
        }
        Ok(())
    }

    pub fn validate_epoch(&self) -> Result<(), ()> {
        if self.epoch == self.global().epoch() {
            Ok(())
        } else {
            Err(())
        }
    }
}

pub struct Shared<'g, T> {
    ptr: *mut Ver<T>,
    birth: u64,
    _marker: PhantomData<&'g ()>,
}

impl<'g, T> Shared<'g, T> {
    pub unsafe fn deref(&self) -> &'g T {
        &unsafe { &*ptr_with_tag(self.ptr, 0) }.data
    }

    pub fn as_ref(&self) -> Option<&'g T> {
        self.as_versioned().map(|ver| &ver.data)
    }

    fn as_versioned(&self) -> Option<&'g Ver<T>> {
        unsafe { ptr_with_tag(self.ptr, 0).as_ref() }
    }

    pub fn is_null(&self) -> bool {
        self.ptr.is_null()
    }

    pub fn tag(&self) -> Result<usize, ()> {
        let result = decompose_ptr(self.ptr).1;
        compiler_fence(Ordering::SeqCst);
        if let Some(ver) = unsafe { ptr_with_tag(self.ptr, 0).as_ref() } {
            if self.birth != ver.birth.load(Ordering::Acquire) {
                return Err(());
            }
        }
        Ok(result)
    }

    pub fn with_tag(&self, tag: usize) -> Self {
        Self {
            ptr: ptr_with_tag(self.ptr, tag),
            birth: self.birth,
            _marker: PhantomData,
        }
    }

    pub fn as_raw(&self) -> *mut Ver<T> {
        self.ptr
    }
}

impl<'g, T> Clone for Shared<'g, T> {
    fn clone(&self) -> Self {
        Self { ..*self }
    }
}

impl<'g, T> Copy for Shared<'g, T> {}

impl<'g, T> PartialEq for Shared<'g, T> {
    fn eq(&self, other: &Self) -> bool {
        self.ptr == other.ptr && self.birth == other.birth
    }
}

pub struct VerAtomic<T> {
    link: AtomicU128,
    _marker: PhantomData<T>,
}

unsafe impl<T> Sync for VerAtomic<T> {}
unsafe impl<T> Send for VerAtomic<T> {}

impl<T> VerAtomic<T> {
    pub fn null() -> Self {
        Self {
            link: AtomicU128::new(0),
            _marker: PhantomData,
        }
    }

    pub fn load<'g>(&self, order: Ordering, guard: &'g Guard<T>) -> Result<Shared<'g, T>, ()> {
        let result = unsafe { self.load_unchecked(order, guard) };
        compiler_fence(Ordering::SeqCst);
        guard.validate_epoch()?;
        Ok(result)
    }

    pub unsafe fn load_unchecked<'g>(&self, order: Ordering, _: &'g Guard<T>) -> Shared<'g, T> {
        let (_, ptr) = decompose_u128::<Ver<T>>(self.link.load(order));
        let birth = if let Some(ver) = unsafe { ptr_with_tag(ptr, 0).as_ref() } {
            ver.birth.load(Ordering::Acquire)
        } else {
            0
        };
        Shared {
            ptr,
            birth,
            _marker: PhantomData,
        }
    }

    pub fn compare_exchange<'g>(
        &self,
        owner: Shared<'g, T>,
        current: Shared<'g, T>,
        new: Shared<'g, T>,
        success: Ordering,
        failure: Ordering,
        _: &'g Guard<T>,
    ) -> Result<*mut Ver<T>, *mut Ver<T>> {
        // let real = self.link.load(Ordering::SeqCst);
        // println!("real {} {:p}", decompose_u128::<Ver<T>>(real).0, decompose_u128::<Ver<T>>(real).1);
        // println!("curr {} {:p}", owner.birth.max(current.birth), current.as_raw());
        // println!("next {} {:p}", owner.birth.max(new.birth), new.as_raw());
        let curr = compose_u128(owner.birth.max(current.birth), current.as_raw());
        let next = compose_u128(owner.birth.max(new.birth), new.as_raw());
        self.link
            .compare_exchange(curr, next, success, failure)
            .map(|comp| decompose_u128(comp).1)
            .map_err(|comp| decompose_u128(comp).1)
    }

    pub fn nullify<'g>(&self, owner: Shared<'g, T>, tag: usize, _: &'g Guard<T>) -> Shared<'g, T> {
        let prev = self.link.load(Ordering::Acquire);
        let result = Shared {
            ptr: ptr_with_tag(null_mut(), tag),
            birth: owner.birth,
            _marker: PhantomData,
        };
        self.link
            .compare_exchange(
                prev,
                compose_u128(result.birth, result.ptr),
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .unwrap();
        result
    }
}

pub struct Entry<T> {
    link: *mut Ver<T>,
}

unsafe impl<T> Sync for Entry<T> {}
unsafe impl<T> Send for Entry<T> {}

impl<T> Entry<T> {
    pub fn new(init: Shared<'_, T>) -> Self {
        Self {
            link: init.as_raw(),
        }
    }

    pub fn load<'g>(&self, guard: &'g Guard<T>) -> Result<Shared<'g, T>, ()> {
        let ptr = self.link;
        if let Some(ver) = unsafe { ptr_with_tag(ptr, 0).as_ref() } {
            let birth = ver.birth.load(Ordering::Acquire);
            compiler_fence(Ordering::SeqCst);
            guard.validate_epoch()?;
            Ok(Shared {
                ptr,
                birth,
                _marker: PhantomData,
            })
        } else {
            Ok(Shared {
                ptr,
                birth: 0,
                _marker: PhantomData,
            })
        }
    }
}

pub struct ImmAtomic<T: Copy> {
    data: Atomic<T>,
}

unsafe impl<T: Copy> Sync for ImmAtomic<T> {}
unsafe impl<T: Copy> Send for ImmAtomic<T> {}

impl<T: Copy> ImmAtomic<T> {
    pub fn new(v: T) -> Self {
        Self {
            data: Atomic::new(v),
        }
    }

    pub fn get<G>(&self, guard: &Guard<G>) -> Result<T, ()> {
        let value = self.data.load(Ordering::Acquire);
        compiler_fence(Ordering::SeqCst);
        guard.validate_epoch()?;
        Ok(value)
    }

    pub fn set(&self, v: T) {
        self.data.store(v, Ordering::Release);
    }
}

fn compose_u128<T>(meta: u64, ptr: *mut T) -> u128 {
    ((meta as u128) << 64) | (ptr as usize as u128)
}

fn decompose_u128<T>(value: u128) -> (u64, *mut T) {
    let meta = (value >> 64) as u64;
    let ptr = (value & (u64::MAX as u128)) as usize as *mut T;
    (meta, ptr)
}

/// Returns a bitmask containing the unused least significant bits of an aligned pointer to `T`.
#[inline]
pub fn low_bits<T>() -> usize {
    (1 << align_of::<T>().trailing_zeros()) - 1
}

/// Given a tagged pointer `data`, returns the same pointer, but tagged with `tag`.
///
/// `tag` is truncated to fit into the unused bits of the pointer to `T`.
#[inline]
pub fn ptr_with_tag<T>(ptr: *mut T, tag: usize) -> *mut T {
    ((ptr as usize & !low_bits::<T>()) | (tag & low_bits::<T>())) as _
}

/// Decomposes a tagged pointer `data` into the pointer and the tag.
#[inline]
pub fn decompose_ptr<T>(ptr: *mut T) -> (*mut T, usize) {
    let raw = ((ptr as usize) & !low_bits::<T>()) as *mut T;
    let tag = (ptr as usize) & low_bits::<T>();
    (raw, tag)
}

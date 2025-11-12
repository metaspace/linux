// SPDX-License-Identifier: GPL-2.0

//! XArray abstraction.
//!
//! C header: [`include/linux/xarray.h`](srctree/include/linux/xarray.h)

use crate::{
    alloc::{self, flags::GFP_KERNEL, KVec},
    bindings, build_assert,
    error::{to_result, Error, Result},
    ffi::c_void,
    prelude::{EBUSY, ENOMEM, ENOSPC},
    str::CStr,
    sync::LockClassKey,
    types::{ForeignOwnable, NotThreadSafe, Opaque},
};
use core::{
    iter,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    pin::Pin,
    ptr::{null_mut, NonNull},
};
use pin_init::{pin_data, pin_init, pinned_drop, PinInit};

/// Creates a [`XArray`] initialiser with the given name and a newly-created lock class.
///
/// It uses the name if one is given, otherwise it generates one based on the file name and line
/// number.
#[macro_export]
macro_rules! new_xarray {
    ($kind:expr $(, $name:literal)? $(,)?) => {
        $crate::xarray::XArray::new(
            $kind, $crate::optional_name!($($name)?), $crate::static_lock_class!())
    };
}
pub use new_xarray;

/// An array which efficiently maps sparse integer indices to owned objects.
///
/// This is similar to a [`crate::alloc::kvec::Vec<Option<T>>`], but more efficient when there are
/// holes in the index space, and can be efficiently grown.
///
/// # Invariants
///
/// `self.xa` is always an initialized and valid [`bindings::xarray`] whose entries are either
/// `XA_ZERO_ENTRY` or came from `T::into_foreign`.
///
/// # Examples
///
/// ```rust
/// use core::pin::Pin;
/// use kernel::alloc::KBox;
/// use kernel::xarray::{new_xarray, AllocKind, XArray};
///
/// let xa: Pin<KBox<XArray<KBox<u32>>>> =
///     KBox::pin_init(new_xarray!(AllocKind::Alloc1), GFP_KERNEL)?;
///
/// let dead = KBox::new(0xdead, GFP_KERNEL)?;
/// let beef = KBox::new(0xbeef, GFP_KERNEL)?;
///
/// let mut guard = xa.lock();
///
/// assert_eq!(guard.get(0), None);
///
/// assert_eq!(guard.store(0, dead, GFP_KERNEL)?.as_deref(), None);
/// assert_eq!(guard.get(0).copied(), Some(0xdead));
///
/// *guard.get_mut(0).unwrap() = 0xffff;
/// assert_eq!(guard.get(0).copied(), Some(0xffff));
///
/// assert_eq!(
///     guard.store(0, beef, GFP_KERNEL)?.as_deref().copied(),
///     Some(0xffff)
/// );
/// assert_eq!(guard.get(0).copied(), Some(0xbeef));
///
/// guard.remove(0);
/// assert_eq!(guard.get(0), None);
///
/// # Ok::<(), Error>(())
/// ```
#[pin_data(PinnedDrop)]
pub struct XArray<T: ForeignOwnable> {
    #[pin]
    xa: Opaque<bindings::xarray>,
    _p: PhantomData<(T)>,
}

#[pinned_drop]
impl<T: ForeignOwnable> PinnedDrop for XArray<T> {
    fn drop(self: Pin<&mut Self>) {
        self.iter().for_each(|ptr| {
            let ptr = ptr.as_ptr();
            // SAFETY: `ptr` came from `T::into_foreign`.
            //
            // INVARIANT: we own the only reference to the array which is being dropped so the
            // broken invariant is not observable on function exit.
            drop(unsafe { T::from_foreign(ptr) })
        });

        // SAFETY: `self.xa` is always valid by the type invariant.
        unsafe { bindings::xa_destroy(self.xa.get()) };
    }
}

/// A buffer for preallocating XArray nodes.
///
/// This structure allows preallocating memory for XArray insertions to avoid
/// allocation failures during operations where allocation is not desirable.
pub struct XArrayPreloadBuffer {
    nodes: KVec<*mut bindings::xa_node>,
    size: usize,
    head: usize,
    tail: usize,
}

impl XArrayPreloadBuffer {
    /// Creates a new preload buffer with capacity for the given number of leaf values.
    ///
    /// Inserting a leaf value into an [`XArray`] may require allocating a
    /// number of internal nodes. This buffer will calculate the upper limit of
    /// required internal nodes for inserting `entry_count` leaf values and use
    /// that to size the buffer.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::XArrayPreloadBuffer};
    /// let buffer = XArrayPreloadBuffer::new(10)?;
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn new(entry_count: usize) -> Result<Self> {
        let node_count = entry_count
            * ((usize::BITS as usize / bindings::XA_CHUNK_SHIFT)
                + if (usize::BITS as usize % bindings::XA_CHUNK_SHIFT) == 0 {
                    0
                } else {
                    1
                });

        let mut this = Self {
            nodes: KVec::new(),
            size: node_count + 1,
            head: 0,
            tail: 0,
        };

        for _ in 0..this.size {
            this.nodes.push(null_mut(), GFP_KERNEL)?;
        }

        Ok(this)
    }

    /// Allocates
    pub fn preload(&mut self, flags: alloc::Flags) -> Result {
        while !self.full() {
            self.alloc(flags)?
        }
        Ok(())
    }

    /// Fills the buffer with preallocated nodes from the given vector.
    ///
    /// Nodes are moved from the vector into the buffer until the buffer is full
    /// or the vector is empty.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{XArrayPreloadBuffer, XArrayPreloadNode}};
    /// let mut buffer = XArrayPreloadBuffer::new(5)?;
    /// let mut nodes = KVec::new();
    /// nodes.push(XArrayPreloadNode::new(GFP_KERNEL)?, GFP_KERNEL)?;
    /// buffer.preload_with(&mut nodes)?;
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn preload_with(&mut self, nodes: &mut KVec<XArrayPreloadNode>) -> Result {
        while !self.full() {
            if let Some(node) = nodes.pop() {
                self.push(node)?
            } else {
                break;
            }
        }

        Ok(())
    }

    /// Returns `true` if the buffer is full and cannot accept more nodes.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{XArrayPreloadBuffer, XArrayPreloadNode}};
    /// let mut buffer = XArrayPreloadBuffer::new(1)?;
    /// if !buffer.full() {
    ///     let count = buffer.free_count();
    ///     let mut nodes = KVec::new();
    ///     for _ in 0..count {
    ///         nodes.push(XArrayPreloadNode::new(GFP_KERNEL)?, GFP_KERNEL)?;
    ///     }
    ///     buffer.preload_with(&mut nodes)?;
    /// }
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn full(&self) -> bool {
        (self.head + 1) % self.size != self.tail
    }

    fn empty(&self) -> bool {
        self.head == self.tail
    }

    /// Returns the number of available slots in the buffer.
    pub fn free_count(&self) -> usize {
        (if self.head >= self.tail {
            self.size - (self.head - self.tail)
        } else {
            (self.size - self.tail) + self.head
        } - 1)
    }

    fn alloc(&mut self, flags: alloc::Flags) -> Result {
        if self.full() {
            return Err(ENOSPC);
        }

        self.push(XArrayPreloadNode::new(flags)?)?;

        Ok(())
    }

    fn push(&mut self, node: XArrayPreloadNode) -> Result {
        if self.full() {
            return Err(ENOSPC);
        }

        self.nodes[self.head] = node.into_raw();
        self.head = (self.head + 1) % self.size;

        Ok(())
    }

    /// Removes and returns one preallocated node from the buffer.
    ///
    /// Returns `None` if the buffer is empty.
    fn take_one(&mut self) -> Option<XArrayPreloadNode> {
        if self.empty() {
            return None;
        }

        let node = self.nodes[self.tail];
        self.tail = (self.tail + 1) % self.size;

        Some(XArrayPreloadNode(node))
    }
}

impl Drop for XArrayPreloadBuffer {
    fn drop(&mut self) {
        while !self.empty() {
            drop(self.take_one().expect("Not empty"));
        }
    }
}

/// A preallocated XArray node.
///
/// This represents a single preallocated internal node for an XArray.
/// Nodes can be stored in an [`XArrayPreloadBuffer`] for later use.
pub struct XArrayPreloadNode(*mut bindings::xa_node);

impl XArrayPreloadNode {
    /// Allocates a new XArray node.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::XArrayPreloadNode};
    /// let node = XArrayPreloadNode::new(GFP_KERNEL)?;
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn new(flags: alloc::Flags) -> Result<Self> {
        // SAFETY: `radix_tree_node_cachep` is a valid kmem cache for XArray nodes.
        let ptr = unsafe {
            bindings::kmem_cache_alloc_noprof(bindings::radix_tree_node_cachep, flags.as_raw())
        };

        if ptr.is_null() {
            return Err(ENOMEM);
        }

        // SAFETY: `ptr` is non-null and was allocated from `radix_tree_node_cachep`.
        Ok(unsafe { XArrayPreloadNode::from_raw(ptr.cast()) })
    }

    fn into_raw(self) -> *mut bindings::xa_node {
        self.0
    }

    /// Creates an `XArrayPreloadNode` from a raw pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid pointer to an XArray node allocated from `radix_tree_node_cachep`.
    unsafe fn from_raw(ptr: *mut bindings::xa_node) -> Self {
        Self(ptr)
    }
}

impl Drop for XArrayPreloadNode {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid pointer allocated from `radix_tree_node_cachep`.
        unsafe { bindings::kmem_cache_free(bindings::radix_tree_node_cachep, self.0.cast()) }
    }
}

/// Flags passed to [`XArray::new`] to configure the array's allocation tracking behavior.
pub enum AllocKind {
    /// Consider the first element to be at index 0.
    Alloc,
    /// Consider the first element to be at index 1.
    Alloc1,
}

impl<T: ForeignOwnable> XArray<T> {
    fn iter(&self) -> impl Iterator<Item = NonNull<c_void>> + '_ {
        let mut index = 0;

        // SAFETY: `self.xa` is always valid by the type invariant.
        iter::once(unsafe {
            bindings::xa_find(self.xa.get(), &mut index, usize::MAX, bindings::XA_PRESENT)
        })
        .chain(iter::from_fn(move || {
            // SAFETY: `self.xa` is always valid by the type invariant.
            Some(unsafe {
                bindings::xa_find_after(self.xa.get(), &mut index, usize::MAX, bindings::XA_PRESENT)
            })
        }))
        .map_while(|ptr| NonNull::new(ptr.cast()))
    }

    /// Creates a new initializer for this type.
    pub fn new(
        kind: AllocKind,
        name: &'static CStr,
        key: Pin<&'static LockClassKey>,
    ) -> impl PinInit<Self> {
        let flags = match kind {
            AllocKind::Alloc => bindings::XA_FLAGS_ALLOC,
            AllocKind::Alloc1 => bindings::XA_FLAGS_ALLOC1,
        };
        pin_init!(Self {
            // SAFETY: `xa` is valid while the closure is called.
            //
            // INVARIANT: `xa` is initialized here to an empty, valid [`bindings::xarray`].
            xa <- Opaque::ffi_init(|xa: *mut bindings::xarray| unsafe {
                bindings::__spin_lock_init(&raw mut (*xa).xa_lock, name.as_ptr().cast(), key.as_ptr());
                (*xa).xa_flags = flags;
                (*xa).xa_head = null_mut();
            }),
            _p: PhantomData,
        })
    }

    /// Attempts to lock the [`XArray`] for exclusive access.
    pub fn try_lock(&self) -> Option<Guard<'_, T>> {
        // SAFETY: `self.xa` is always valid by the type invariant.
        if (unsafe { bindings::xa_trylock(self.xa.get()) } != 0) {
            Some(Guard {
                xa: self,
                _not_send: NotThreadSafe,
            })
        } else {
            None
        }
    }

    /// Locks the [`XArray`] for exclusive access.
    pub fn lock(&self) -> Guard<'_, T> {
        // SAFETY: `self.xa` is always valid by the type invariant.
        unsafe { bindings::xa_lock(self.xa.get()) };
        Guard {
            xa: self,
            _not_send: NotThreadSafe,
        }
    }
}

impl<'a, T: ForeignOwnable> Guard<'a, T> {
    /// Checks if the XArray contains an element at the specified index.
    ///
    /// # Examples
    ///
    /// ```
    /// # use core::pin::Pin;
    /// # use kernel::{alloc::{flags::GFP_KERNEL, kbox::KBox}, xarray::{AllocKind, new_xarray, XArray}};
    /// let xa: Pin<KBox<XArray<KBox<u32>>>>  = KBox::pin_init(new_xarray!(AllocKind::Alloc), GFP_KERNEL)?;
    ///
    /// let mut guard = xa.lock();
    /// assert_eq!(guard.contains_index(42), false);
    ///
    /// guard.store(42, KBox::new(0u32, GFP_KERNEL)?, GFP_KERNEL)?;
    ///
    /// assert_eq!(guard.contains_index(42), true);
    ///
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn contains_index(&self, index: usize) -> bool {
        self.get(index).is_some()
    }

    fn load(&self, index: usize) -> Option<NonNull<c_void>> {
        // TODO: Split out into separate commit. Old code took xa_lock even though we already hold
        // it.
        let mut state = XArrayState::new(self, index);
        // SAFETY: `state.state` is always valid by the type invariant.
        let ptr = unsafe { bindings::xas_load(&raw mut state.state) };
        NonNull::new(ptr)
    }

    /// Provides a reference to the element at the given index.
    pub fn get(&self, index: usize) -> Option<T::Borrowed<'_>> {
        let ptr = self.load(index)?;
        // SAFETY: `ptr` came from `T::into_foreign`.
        Some(unsafe { T::borrow(ptr.as_ptr()) })
    }

    /// Provides a mutable reference to the element at the given index.
    pub fn get_mut<'b>(&'b mut self, index: usize) -> Option<T::BorrowedMut<'_>> {
        let ptr = self.load(index)?;

        // SAFETY: `ptr` came from `T::into_foreign`.
        Some(unsafe { T::borrow_mut(ptr.as_ptr()) })
    }

    /// Gets an entry for the specified index, which can be vacant or occupied.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{AllocKind, new_xarray, Entry, XArray}};
    /// let mut xa: Pin<KBox<XArray<KBox<u32>>>> =
    ///     KBox::pin_init(new_xarray!(AllocKind::Alloc), GFP_KERNEL)?;
    /// let mut guard = xa.lock();
    ///
    /// assert_eq!(guard.contains_index(42), false);
    ///
    /// match guard.get_entry(42) {
    ///     Entry::Vacant(entry) => {
    ///         entry.insert(KBox::new(0x1337u32, GFP_KERNEL)?, None)?;
    ///     }
    ///     Entry::Occupied(_) => unreachable!("We did not insert an entry yet"),
    /// }
    ///
    /// assert_eq!(guard.get(42), Some(&0x1337));
    ///
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn get_entry<'b>(&'b mut self, index: usize) -> Entry<'a, 'b, T> {
        match self.load(index) {
            None => Entry::Vacant(VacantEntry::new(self, index)),
            Some(ptr) => Entry::Occupied(OccupiedEntry::new(self, index, ptr)),
        }
    }

    fn load_next(&self, mut index: usize) -> Option<(usize, NonNull<c_void>)> {
        // SAFETY: `self.xa.xa` is always valid by the type invariant and the caller holds the lock.
        let ptr = unsafe {
            bindings::xa_find(
                self.xa.xa.get(),
                &mut index,
                usize::MAX,
                bindings::XA_PRESENT,
            )
        };
        NonNull::new(ptr).map(|ptr| (index, ptr))
    }

    /// Finds the next element starting from the given index.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{AllocKind, new_xarray, XArray}};
    /// let mut xa: Pin<KBox<XArray<KBox<u32>>>> =
    ///     KBox::pin_init(new_xarray!(AllocKind::Alloc), GFP_KERNEL)?;
    /// let mut guard = xa.lock();
    ///
    /// guard.store(10, KBox::new(10u32, GFP_KERNEL)?, GFP_KERNEL)?;
    /// guard.store(20, KBox::new(20u32, GFP_KERNEL)?, GFP_KERNEL)?;
    ///
    /// if let Some((found_index, value)) = guard.find_next(11) {
    ///     assert_eq!(found_index, 20);
    ///     assert_eq!(*value, 20);
    /// }
    ///
    /// if let Some((found_index, value)) = guard.find_next(5) {
    ///     assert_eq!(found_index, 10);
    ///     assert_eq!(*value, 10);
    /// }
    ///
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn find_next(&self, index: usize) -> Option<(usize, T::Borrowed<'_>)> {
        self.load_next(index)
            // SAFETY: `ptr` came from `T::into_foreign`.
            .map(|(index, ptr)| (index, unsafe { T::borrow(ptr.as_ptr()) }))
    }

    /// Finds the next element starting from the given index, returning a mutable reference.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{AllocKind, new_xarray, XArray}};
    /// let mut xa: Pin<KBox<XArray<KBox<u32>>>> =
    ///     KBox::pin_init(new_xarray!(AllocKind::Alloc), GFP_KERNEL)?;
    /// let mut guard = xa.lock();
    ///
    /// guard.store(10, KBox::new(10u32, GFP_KERNEL)?, GFP_KERNEL)?;
    /// guard.store(20, KBox::new(20u32, GFP_KERNEL)?, GFP_KERNEL)?;
    ///
    /// if let Some((found_index, mut_value)) = guard.find_next_mut(5) {
    ///     assert_eq!(found_index, 10);
    ///     *mut_value = 0x99;
    /// }
    ///
    /// assert_eq!(guard.get(10).copied(), Some(0x99));
    ///
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn find_next_mut(&mut self, index: usize) -> Option<(usize, T::BorrowedMut<'_>)> {
        self.load_next(index)
            // SAFETY: `ptr` came from `T::into_foreign`.
            .map(move |(index, ptr)| (index, unsafe { T::borrow_mut(ptr.as_ptr()) }))
    }

    /// Finds the next occupied entry starting from the given index.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{AllocKind, new_xarray, XArray}};
    /// let mut xa: Pin<KBox<XArray<KBox<u32>>>> =
    ///     KBox::pin_init(new_xarray!(AllocKind::Alloc), GFP_KERNEL)?;
    /// let mut guard = xa.lock();
    ///
    /// guard.store(10, KBox::new(10u32, GFP_KERNEL)?, GFP_KERNEL)?;
    /// guard.store(20, KBox::new(20u32, GFP_KERNEL)?, GFP_KERNEL)?;
    ///
    /// if let Some(entry) = guard.find_next_entry(5) {
    ///     assert_eq!(entry.index(), 10);
    ///     let value = entry.remove();
    ///     assert_eq!(*value, 10);
    /// }
    ///
    /// assert_eq!(guard.get(10), None);
    ///
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn find_next_entry<'b>(&'b mut self, index: usize) -> Option<OccupiedEntry<'a, 'b, T>> {
        let mut state = XArrayState::new(self, index);

        // SAFETY: `state.state` is properly initialized by XArrayState::new and the caller holds
        // the lock.
        let ptr = NonNull::new(unsafe { bindings::xas_find(&mut state.state, usize::MAX) })?;

        Some(OccupiedEntry { state, ptr })
    }

    /// Finds the next occupied entry starting at the given index, wrapping around.
    ///
    /// Searches for an entry starting at `index` up to the maximum index. If no entry
    /// is found, wraps around and searches from index 0 up to `index`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{AllocKind, new_xarray, XArray}};
    /// let mut xa: Pin<KBox<XArray<KBox<u32>>>> =
    ///     KBox::pin_init(new_xarray!(AllocKind::Alloc), GFP_KERNEL)?;
    /// let mut guard = xa.lock();
    ///
    /// guard.store(100, KBox::new(42u32, GFP_KERNEL)?, GFP_KERNEL)?;
    /// let entry = guard.find_next_entry_circular(101);
    /// assert_eq!(entry.map(|e| e.index()), Some(100));
    ///
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn find_next_entry_circular<'b>(
        &'b mut self,
        index: usize,
    ) -> Option<OccupiedEntry<'a, 'b, T>> {
        let mut state = XArrayState::new(self, index);

        // SAFETY: `state.state` is properly initialized by XArrayState::new and the caller holds
        // the lock.
        let ptr = NonNull::new(unsafe { bindings::xas_find(&mut state.state, usize::MAX) })
            .or_else(|| {
                state.state.xa_node = bindings::XAS_RESTART as *mut bindings::xa_node;
                state.state.xa_index = 0;
                // SAFETY: `state.state` is properly initialized and by type invariant, we hold the
                // xarray lock.
                NonNull::new(unsafe { bindings::xas_find(&mut state.state, index) })
            })?;

        Some(OccupiedEntry { state, ptr })
    }

    /// Removes and returns the element at the given index.
    pub fn remove(&mut self, index: usize) -> Option<T> {
        // SAFETY:
        // - `self.xa.xa` is always valid by the type invariant.
        // - The caller holds the lock.
        let ptr = unsafe { bindings::__xa_erase(self.xa.xa.get(), index) }.cast();
        // SAFETY:
        // - `ptr` is either NULL or came from `T::into_foreign`.
        // - `&mut self` guarantees that the lifetimes of [`T::Borrowed`] and [`T::BorrowedMut`]
        // borrowed from `self` have ended.
        unsafe { T::try_from_foreign(ptr) }
    }

    fn store_internal(
        &mut self,
        index: usize,
        value: T,
        gfp: alloc::Flags,
    ) -> Result<(NonNull<c_void>, *mut c_void), StoreError<T>> {
        build_assert!(
            T::FOREIGN_ALIGN >= 4,
            "pointers stored in XArray must be 4-byte aligned"
        );
        let new = value.into_foreign();

        let old = {
            let new = new.cast();
            // SAFETY:
            // - `self.xa.xa` is always valid by the type invariant.
            // - The caller holds the lock.
            //
            // INVARIANT: `new` came from `T::into_foreign`.
            unsafe { bindings::__xa_store(self.xa.xa.get(), index, new, gfp.as_raw()) }
        };

        // SAFETY: `__xa_store` returns the old entry at this index on success or `xa_err` if an
        // error happened.
        let errno = unsafe { bindings::xa_err(old) };
        if errno != 0 {
            // SAFETY: `new` came from `T::into_foreign` and `__xa_store` does not take
            // ownership of the value on error.
            let value = unsafe { T::from_foreign(new) };
            Err(StoreError {
                value,
                error: Error::from_errno(errno),
            })
        } else {
            // SAFETY: `new` came from `T::into_foreign` and is guaranteed non-null.
            Ok((unsafe { NonNull::new_unchecked(new) }, old))
        }
    }

    /// Stores an element at the given index.
    ///
    /// This method may drop the XArray lock to allocate memory. If another
    /// thread acquires the lock during this time, this method will block until
    /// the lock can be reacquired.
    ///
    /// On success, returns the element which was previously at the given index.
    ///
    /// On failure, returns the element which was attempted to be stored.
    pub fn store(
        &mut self,
        index: usize,
        value: T,
        flags: alloc::Flags,
    ) -> Result<Option<T>, StoreError<T>> {
        let (_new, old) = self.store_internal(index, value, flags)?;

        // SAFETY: `ptr` is either NULL or came from `T::into_foreign`.
        //
        // NB: `XA_ZERO_ENTRY` is never returned by functions belonging to the Normal XArray
        // API; such entries present as `NULL`.
        Ok(unsafe { T::try_from_foreign(old) })
    }

    /// Inserts a value and returns an occupied entry for further operations.
    ///
    /// If a value is already present, the operation fails.
    ///
    /// This method may drop the XArray lock to allocate memory. If another
    /// thread acquires the lock during this time, this method will block until
    /// the lock can be reacquired.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{AllocKind, new_xarray, XArray}};
    /// let mut xa: Pin<KBox<XArray<KBox<u32>>>> =
    ///     KBox::pin_init(new_xarray!(AllocKind::Alloc), GFP_KERNEL)?;
    /// let mut guard = xa.lock();
    ///
    /// assert_eq!(guard.get(42), None);
    ///
    /// let value = KBox::new(0x1337u32, GFP_KERNEL)?;
    /// let entry = guard.insert_entry(42, value, None)?;
    /// let borrowed = entry.into_mut();
    /// assert_eq!(borrowed, &0x1337);
    ///
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn insert_entry<'b>(
        &'b mut self,
        index: usize,
        value: T,
        preload: Option<&mut XArrayPreloadBuffer>,
    ) -> Result<OccupiedEntry<'a, 'b, T>, StoreError<T>> {
        match self.get_entry(index) {
            Entry::Vacant(entry) => entry.insert_entry(value, preload),
            Entry::Occupied(_) => Err(StoreError {
                error: EBUSY,
                value,
            }),
        }
    }
}

/// A lock guard.
///
/// The lock is unlocked when the guard goes out of scope.
#[must_use = "the lock unlocks immediately when the guard is unused"]
pub struct Guard<'a, T: ForeignOwnable> {
    xa: &'a XArray<T>,
    _not_send: NotThreadSafe,
}

impl<T: ForeignOwnable> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        // SAFETY:
        // - `self.xa.xa` is always valid by the type invariant.
        // - The caller holds the lock, so it is safe to unlock it.
        unsafe { bindings::xa_unlock(self.xa.xa.get()) };
    }
}

/// The error returned by [`store`](Guard::store).
///
/// Contains the underlying error and the value that was not stored.
pub struct StoreError<T> {
    /// The error that occurred.
    pub error: Error,
    /// The value that was not stored.
    pub value: T,
}

impl<T> core::fmt::Debug for StoreError<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StoreError")
            .field("error", &self.error)
            .finish()
    }
}

impl<T> From<StoreError<T>> for Error {
    fn from(value: StoreError<T>) -> Self {
        value.error
    }
}

/// Internal state for XArray iteration and entry operations.
struct XArrayState<'a, 'b, T: ForeignOwnable> {
    /// Holds the lock guard to ensure exclusive access for the lifetime of `Self`.
    _access: &'b Guard<'a, T>,
    state: bindings::xa_state,
}

impl<'a, 'b, T: ForeignOwnable> Drop for XArrayState<'a, 'b, T> {
    fn drop(&mut self) {
        if !self.state.xa_alloc.is_null() {
            // SAFETY: `xa_alloc` is a valid pointer to a preallocated node when non-null.
            drop(unsafe { XArrayPreloadNode::from_raw(self.state.xa_alloc) })
        }
    }
}

impl<'a, 'b, T: ForeignOwnable> XArrayState<'a, 'b, T> {
    fn new(access: &'b Guard<'a, T>, index: usize) -> Self {
        let ptr = access.xa.xa.get();
        Self {
            _access: access,
            state: bindings::xa_state {
                xa: ptr,
                xa_index: index,
                xa_shift: 0,
                xa_sibs: 0,
                xa_offset: 0,
                xa_pad: 0,
                xa_node: bindings::XAS_RESTART as *mut bindings::xa_node,
                xa_alloc: null_mut(),
                xa_update: None,
                xa_lru: null_mut(),
            },
        }
    }

    fn status(&self) -> Result {
        // SAFETY: `self.state` is properly initialized and valid.
        to_result(unsafe { bindings::xas_error(&self.state) })
    }
}

/// Represents either a vacant or occupied entry in an XArray.
pub enum Entry<'a, 'b, T: ForeignOwnable> {
    /// A vacant entry that can have a value inserted.
    Vacant(VacantEntry<'a, 'b, T>),
    /// An occupied entry containing a value.
    Occupied(OccupiedEntry<'a, 'b, T>),
}

impl<T: ForeignOwnable> Entry<'_, '_, T> {
    /// Returns true if this entry is occupied.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{AllocKind, new_xarray, Entry, XArray}};
    /// let mut xa: Pin<KBox<XArray<KBox<u32>>>> =
    ///     KBox::pin_init(new_xarray!(AllocKind::Alloc), GFP_KERNEL)?;
    /// let mut guard = xa.lock();
    ///
    /// let entry = guard.get_entry(42);
    /// assert_eq!(entry.is_occupied(), false);
    /// drop(entry);
    ///
    /// guard.store(42, KBox::new(0x1337u32, GFP_KERNEL)?, GFP_KERNEL)?;
    /// let entry = guard.get_entry(42);
    /// assert_eq!(entry.is_occupied(), true);
    ///
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn is_occupied(&self) -> bool {
        matches!(self, Entry::Occupied(_))
    }
}

/// A view into a vacant entry in an XArray.
pub struct VacantEntry<'a, 'b, T: ForeignOwnable> {
    state: XArrayState<'a, 'b, T>,
}

impl<'a, 'b, T> VacantEntry<'a, 'b, T>
where
    T: ForeignOwnable,
{
    fn new(guard: &'b mut Guard<'a, T>, index: usize) -> Self {
        Self {
            state: XArrayState::new(guard, index),
        }
    }

    fn insert_internal(
        &mut self,
        value: T,
        mut preload: Option<&mut XArrayPreloadBuffer>,
    ) -> Result<*mut c_void, StoreError<T>> {
        let new = T::into_foreign(value).cast();

        loop {
            // SAFETY: `self.state.state` is properly initialized and `new` came from
            // `T::into_foreign`. We hold the xarray lock.
            unsafe { bindings::xas_store(&mut self.state.state, new) };

            match self.state.status() {
                Ok(()) => break Ok(new),
                Err(ENOMEM) => {
                    debug_assert!(self.state.state.xa_alloc.is_null());
                    let node = match preload.as_mut().map(|node| node.take_one().ok_or(ENOMEM)) {
                        None => break Err(ENOMEM),
                        Some(Err(e)) => break Err(e),
                        Some(Ok(node)) => node,
                    };

                    self.state.state.xa_alloc = node.into_raw();
                    continue;
                }
                Err(e) => break Err(e),
            }
        }
        .map_err(|error| {
            // SAFETY: `new` came from `T::into_foreign` and `xas_store` does not take
            // ownership of the value on error.
            let value = unsafe { T::from_foreign(new) };
            StoreError { value, error }
        })
    }

    /// Inserts a value into this vacant entry.
    ///
    /// Returns a reference to the newly inserted value.
    ///
    /// This method may drop the XArray lock to allocate memory. If another
    /// thread acquires the lock during this time, this method will block until
    /// the lock can be reacquired. When this method resumes, the slot this
    /// entry is representing may be occupied. In this case, the operation will
    /// fail.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{AllocKind, new_xarray, Entry, XArray}};
    /// let mut xa: Pin<KBox<XArray<KBox<u32>>>> =
    ///     KBox::pin_init(new_xarray!(AllocKind::Alloc), GFP_KERNEL)?;
    /// let mut guard = xa.lock();
    ///
    /// assert_eq!(guard.get(42), None);
    ///
    /// if let Entry::Vacant(entry) = guard.get_entry(42) {
    ///     let value = KBox::new(0x1337u32, GFP_KERNEL)?;
    ///     let borrowed = entry.insert(value, None)?;
    ///     assert_eq!(*borrowed, 0x1337);
    /// }
    ///
    /// assert_eq!(guard.get(42).copied(), Some(0x1337));
    ///
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn insert(
        mut self,
        value: T,
        preload: Option<&mut XArrayPreloadBuffer>,
    ) -> Result<T::BorrowedMut<'b>, StoreError<T>> {
        let new = self.insert_internal(value, preload)?;

        // SAFETY: `new` came from `T::into_foreign`. The entry has exclusive ownership of `new`.
        Ok(unsafe { T::borrow_mut(new) })
    }

    /// Inserts a value and returns an occupied entry representing the newly inserted value.
    ///
    /// This method may drop the XArray lock to allocate memory. If another
    /// thread acquires the lock during this time, this method will block until
    /// the lock can be reacquired. When this method resumes, the slot this
    /// entry is representing may be occupied. In this case, the operation will
    /// fail.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{AllocKind, new_xarray, Entry, XArray}};
    /// let mut xa: Pin<KBox<XArray<KBox<u32>>>> =
    ///     KBox::pin_init(new_xarray!(AllocKind::Alloc), GFP_KERNEL)?;
    /// let mut guard = xa.lock();
    ///
    /// assert_eq!(guard.get(42), None);
    ///
    /// if let Entry::Vacant(entry) = guard.get_entry(42) {
    ///     let value = KBox::new(0x1337u32, GFP_KERNEL)?;
    ///     let occupied = entry.insert_entry(value, None)?;
    ///     assert_eq!(occupied.index(), 42);
    /// }
    ///
    /// assert_eq!(guard.get(42).copied(), Some(0x1337));
    ///
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn insert_entry(
        mut self,
        value: T,
        preload: Option<&mut XArrayPreloadBuffer>,
    ) -> Result<OccupiedEntry<'a, 'b, T>, StoreError<T>> {
        let new = self.insert_internal(value, preload)?;

        Ok(OccupiedEntry::<'a, 'b, T> {
            state: self.state,
            // SAFETY: `new` came from `T::into_foreign` and is guaranteed non-null.
            ptr: unsafe { NonNull::new_unchecked(new) },
        })
    }

    /// Returns the index of this vacant entry.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{AllocKind, new_xarray, Entry, XArray}};
    /// let mut xa: Pin<KBox<XArray<KBox<u32>>>> =
    ///     KBox::pin_init(new_xarray!(AllocKind::Alloc), GFP_KERNEL)?;
    /// let mut guard = xa.lock();
    ///
    /// assert_eq!(guard.get(42), None);
    ///
    /// if let Entry::Vacant(entry) = guard.get_entry(42) {
    ///     assert_eq!(entry.index(), 42);
    /// }
    ///
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn index(&self) -> usize {
        self.state.state.xa_index
    }
}

/// A view into an occupied entry in an XArray.
pub struct OccupiedEntry<'a, 'b, T: ForeignOwnable> {
    state: XArrayState<'a, 'b, T>,
    ptr: NonNull<c_void>,
}

impl<'a, 'b, T> OccupiedEntry<'a, 'b, T>
where
    T: ForeignOwnable,
{
    fn new(guard: &'b mut Guard<'a, T>, index: usize, ptr: NonNull<c_void>) -> Self {
        Self {
            state: XArrayState::new(guard, index),
            ptr,
        }
    }

    /// Removes the value from this occupied entry and returns it, consuming the entry.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{AllocKind, new_xarray, Entry, XArray}};
    /// let mut xa: Pin<KBox<XArray<KBox<u32>>>> =
    ///     KBox::pin_init(new_xarray!(AllocKind::Alloc), GFP_KERNEL)?;
    /// let mut guard = xa.lock();
    ///
    /// guard.store(42, KBox::new(0x1337u32, GFP_KERNEL)?, GFP_KERNEL)?;
    /// assert_eq!(guard.get(42).copied(), Some(0x1337));
    ///
    /// if let Entry::Occupied(entry) = guard.get_entry(42) {
    ///     let value = entry.remove();
    ///     assert_eq!(*value, 0x1337);
    /// }
    ///
    /// assert_eq!(guard.get(42), None);
    ///
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn remove(mut self) -> T {
        // NOTE: Storing NULL to an occupied slot never fails.
        // SAFETY: `self.state.state` is properly initialized and valid for XAS operations.
        let ptr = unsafe {
            bindings::xas_result(
                &mut self.state.state,
                bindings::xa_zero_to_null(bindings::xas_store(&mut self.state.state, null_mut())),
            )
        };

        // SAFETY: `ptr` is a valid return value from xas_result.
        let errno = unsafe { bindings::xa_err(ptr) };
        debug_assert!(errno == 0);

        // SAFETY:
        // - `ptr` is either NULL or came from `T::into_foreign`.
        // - `&mut self` guarantees that the lifetimes of [`T::Borrowed`] and [`T::BorrowedMut`]
        // borrowed from `self` have ended.
        unsafe { T::from_foreign(ptr.cast()) }
    }

    /// Returns the index of this occupied entry.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{AllocKind, new_xarray, Entry, XArray}};
    /// let mut xa: Pin<KBox<XArray<KBox<u32>>>> =
    ///     KBox::pin_init(new_xarray!(AllocKind::Alloc), GFP_KERNEL)?;
    /// let mut guard = xa.lock();
    ///
    /// guard.store(42, KBox::new(0x1337u32, GFP_KERNEL)?, GFP_KERNEL)?;
    ///
    /// if let Entry::Occupied(entry) = guard.get_entry(42) {
    ///     assert_eq!(entry.index(), 42);
    /// }
    ///
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn index(&self) -> usize {
        self.state.state.xa_index
    }

    /// Replaces the value in this occupied entry and returns the old value.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{AllocKind, new_xarray, Entry, XArray}};
    /// let mut xa: Pin<KBox<XArray<KBox<u32>>>> =
    ///     KBox::pin_init(new_xarray!(AllocKind::Alloc), GFP_KERNEL)?;
    /// let mut guard = xa.lock();
    ///
    /// guard.store(42, KBox::new(0x1337u32, GFP_KERNEL)?, GFP_KERNEL)?;
    ///
    /// if let Entry::Occupied(mut entry) = guard.get_entry(42) {
    ///     let new_value = KBox::new(0x9999u32, GFP_KERNEL)?;
    ///     let old_value = entry.insert(new_value);
    ///     assert_eq!(*old_value, 0x1337);
    /// }
    ///
    /// assert_eq!(guard.get(42).copied(), Some(0x9999));
    ///
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn insert(&mut self, value: T) -> T {
        // NOTE: Storing to an occupied slot never fails.
        let new = T::into_foreign(value).cast();
        // SAFETY: `new` came from `T::into_foreign` and is guaranteed non-null.
        self.ptr = unsafe { NonNull::new_unchecked(new) };

        // SAFETY: `self.state.state` is properly initialized and valid for XAS operations.
        let old = unsafe {
            bindings::xas_result(
                &mut self.state.state,
                bindings::xa_zero_to_null(bindings::xas_store(&mut self.state.state, new)),
            )
        };

        // SAFETY: `old` is a valid return value from xas_result.
        let errno = unsafe { bindings::xa_err(old) };
        debug_assert!(errno == 0);

        // SAFETY:
        // - `ptr` is either NULL or came from `T::into_foreign`.
        // - `&mut self` guarantees that the lifetimes of [`T::Borrowed`] and [`T::BorrowedMut`]
        // borrowed from `self` have ended.
        unsafe { T::from_foreign(old) }
    }

    /// Converts this occupied entry into a mutable reference to the value in the slot represented
    /// by the entry.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{AllocKind, new_xarray, Entry, XArray}};
    /// let mut xa: Pin<KBox<XArray<KBox<u32>>>> =
    ///     KBox::pin_init(new_xarray!(AllocKind::Alloc), GFP_KERNEL)?;
    /// let mut guard = xa.lock();
    ///
    /// guard.store(42, KBox::new(0x1337u32, GFP_KERNEL)?, GFP_KERNEL)?;
    ///
    /// if let Entry::Occupied(entry) = guard.get_entry(42) {
    ///     let value_ref = entry.into_mut();
    ///     *value_ref = 0x9999;
    /// }
    ///
    /// assert_eq!(guard.get(42).copied(), Some(0x9999));
    ///
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn into_mut(self) -> T::BorrowedMut<'b> {
        // SAFETY: `ptr` came from `T::into_foreign`.
        unsafe { T::borrow_mut(self.ptr.as_ptr()) }
    }

    /// Swaps the value in this entry with the provided value.
    ///
    /// Returns the old value that was in the entry.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{AllocKind, new_xarray, Entry, XArray}};
    /// let mut xa: Pin<KBox<XArray<KBox<u32>>>> =
    ///     KBox::pin_init(new_xarray!(AllocKind::Alloc), GFP_KERNEL)?;
    /// let mut guard = xa.lock();
    ///
    /// guard.store(42, KBox::new(100u32, GFP_KERNEL)?, GFP_KERNEL)?;
    ///
    /// if let Entry::Occupied(mut entry) = guard.get_entry(42) {
    ///     let old_value = entry.swap(200u32);
    ///     assert_eq!(old_value, 100);
    ///     assert_eq!(*entry, 200);
    /// }
    ///
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn swap<U>(&mut self, mut other: U) -> U
    where
        T: for<'c> ForeignOwnable<Borrowed<'c> = &'c U, BorrowedMut<'c> = &'c mut U>,
    {
        core::mem::swap(self.deref_mut(), &mut other);
        other
    }
}

impl<T, U> Deref for OccupiedEntry<'_, '_, T>
where
    T: for<'a> ForeignOwnable<Borrowed<'a> = &'a U, BorrowedMut<'a> = &'a mut U>,
{
    type Target = U;

    fn deref(&self) -> &Self::Target {
        // SAFETY: `ptr` came from `T::into_foreign`.
        unsafe { T::borrow(self.ptr.as_ptr()) }
    }
}

impl<T, U> DerefMut for OccupiedEntry<'_, '_, T>
where
    T: for<'a> ForeignOwnable<Borrowed<'a> = &'a U, BorrowedMut<'a> = &'a mut U>,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: `ptr` came from `T::into_foreign`.
        unsafe { T::borrow_mut(self.ptr.as_ptr()) }
    }
}

// SAFETY: `XArray<T>` has no shared mutable state so it is `Send` iff `T` is `Send`.
unsafe impl<T: ForeignOwnable + Send> Send for XArray<T> {}

// SAFETY: `XArray<T>` serialises the interior mutability it provides so it is `Sync` iff `T` is
// `Send`.
unsafe impl<T: ForeignOwnable + Send> Sync for XArray<T> {}

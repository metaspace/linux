// SPDX-License-Identifier: GPL-2.0

//! XArray abstraction.
//!
//! C header: [`include/linux/xarray.h`](srctree/include/linux/xarray.h)

use core::{
    iter,
    marker::PhantomData,
    pin::Pin,
    ptr::{
        null_mut,
        NonNull, //
    },
};
pub use entry::{
    Entry,
    OccupiedEntry,
    VacantEntry, //
};
use kernel::{
    alloc,
    bindings,
    build_assert, //
    error::{
        to_result,
        Error,
        Result, //
    },
    ffi::c_void,
    types::{
        ForeignOwnable,
        NotThreadSafe,
        Opaque, //
    },
};
use pin_init::{
    pin_data,
    pin_init,
    pinned_drop,
    PinInit, //
};

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
/// use kernel::alloc::KBox;
/// use kernel::xarray::{AllocKind, XArray};
///
/// let xa = KBox::pin_init(XArray::new(AllocKind::Alloc1), GFP_KERNEL)?;
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
    _p: PhantomData<T>,
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

/// Flags passed to [`XArray::new`] to configure the array's allocation tracking behavior.
pub enum AllocKind {
    /// Consider the first element to be at index 0.
    Alloc,
    /// Consider the first element to be at index 1.
    Alloc1,
}

impl<T: ForeignOwnable> XArray<T> {
    /// Creates a new initializer for this type.
    pub fn new(kind: AllocKind) -> impl PinInit<Self> {
        let flags = match kind {
            AllocKind::Alloc => bindings::XA_FLAGS_ALLOC,
            AllocKind::Alloc1 => bindings::XA_FLAGS_ALLOC1,
        };
        pin_init!(Self {
            // SAFETY: `xa` is valid while the closure is called.
            //
            // INVARIANT: `xa` is initialized here to an empty, valid [`bindings::xarray`].
            xa <- Opaque::ffi_init(|xa| unsafe {
                bindings::xa_init_flags(xa, flags)
            }),
            _p: PhantomData,
        })
    }

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

impl<'a, T: ForeignOwnable> Guard<'a, T> {
    fn load(&self, index: usize) -> Option<NonNull<c_void>> {
        XArrayState::new(self, index).load()
    }

    /// Checks if the XArray contains an element at the specified index.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{alloc::{flags::GFP_KERNEL, kbox::KBox}, xarray::{AllocKind, XArray}};
    /// let xa = KBox::pin_init(XArray::new(AllocKind::Alloc), GFP_KERNEL)?;
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

    /// Provides a reference to the element at the given index.
    pub fn get(&self, index: usize) -> Option<T::Borrowed<'_>> {
        let ptr = self.load(index)?;
        // SAFETY: `ptr` came from `T::into_foreign`.
        Some(unsafe { T::borrow(ptr.as_ptr()) })
    }

    /// Provides a mutable reference to the element at the given index.
    pub fn get_mut(&mut self, index: usize) -> Option<T::BorrowedMut<'_>> {
        let ptr = self.load(index)?;

        // SAFETY: `ptr` came from `T::into_foreign`.
        Some(unsafe { T::borrow_mut(ptr.as_ptr()) })
    }

    /// Gets an entry for the specified index, which can be vacant or occupied.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{AllocKind, XArray, Entry}};
    /// let mut xa = KBox::pin_init(XArray::<KBox<u32>>::new(AllocKind::Alloc), GFP_KERNEL)?;
    /// let mut guard = xa.lock();
    ///
    /// assert_eq!(guard.contains_index(42), false);
    ///
    /// match guard.entry(42) {
    ///     Entry::Vacant(entry) => {
    ///         entry.insert(KBox::new(0x1337u32, GFP_KERNEL)?)?;
    ///     }
    ///     Entry::Occupied(_) => unreachable!("We did not insert an entry yet"),
    /// }
    ///
    /// assert_eq!(guard.get(42), Some(&0x1337));
    ///
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn entry<'b>(&'b mut self, index: usize) -> Entry<'a, 'b, T> {
        match self.load(index) {
            None => Entry::Vacant(VacantEntry::new(self, index)),
            Some(ptr) => Entry::Occupied(OccupiedEntry::new(self, index, ptr)),
        }
    }

    fn load_next(&self, index: usize) -> Option<(usize, NonNull<c_void>)> {
        XArrayState::new(self, index).load_next()
    }

    /// Finds the next element starting from the given index.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{AllocKind, XArray}};
    /// let mut xa = KBox::pin_init(XArray::<KBox<u32>>::new(AllocKind::Alloc), GFP_KERNEL)?;
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
    /// # use kernel::{prelude::*, xarray::{AllocKind, XArray}};
    /// let mut xa = KBox::pin_init(XArray::<KBox<u32>>::new(AllocKind::Alloc), GFP_KERNEL)?;
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
    /// # use kernel::{prelude::*, xarray::{AllocKind, XArray}};
    /// let mut xa = KBox::pin_init(XArray::<KBox<u32>>::new(AllocKind::Alloc), GFP_KERNEL)?;
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
        let (_, ptr) = state.load_next()?;
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
    /// # use kernel::{prelude::*, xarray::{AllocKind, XArray}};
    /// let mut xa = KBox::pin_init(XArray::<KBox<u32>>::new(AllocKind::Alloc), GFP_KERNEL)?;
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

    /// Stores an element at the given index.
    ///
    /// May drop the lock if needed to allocate memory, and then reacquire it afterwards.
    ///
    /// On success, returns the element which was previously at the given index.
    ///
    /// On failure, returns the element which was attempted to be stored.
    pub fn store(
        &mut self,
        index: usize,
        value: T,
        gfp: alloc::Flags,
    ) -> Result<Option<T>, StoreError<T>> {
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
            let old = old.cast();
            // SAFETY: `ptr` is either NULL or came from `T::into_foreign`.
            //
            // NB: `XA_ZERO_ENTRY` is never returned by functions belonging to the Normal XArray
            // API; such entries present as `NULL`.
            Ok(unsafe { T::try_from_foreign(old) })
        }
    }
}

/// Internal state for XArray iteration and entry operations.
///
/// # Invariants
///
/// - `state` is always a valid `bindings::xa_state`.
pub(crate) struct XArrayState<'a, 'b, T: ForeignOwnable> {
    /// Holds a reference to the lock guard to ensure the lock is not dropped
    /// while `Self` is live.
    _access: PhantomData<&'b Guard<'a, T>>,
    state: bindings::xa_state,
}

impl<'a, 'b, T: ForeignOwnable> XArrayState<'a, 'b, T> {
    fn new(access: &'b Guard<'a, T>, index: usize) -> Self {
        let ptr = access.xa.xa.get();
        // INVARIANT: We initialize `self.state` to a valid value below.
        Self {
            _access: PhantomData,
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

    fn load(&mut self) -> Option<NonNull<c_void>> {
        // SAFETY: `state.state` is always valid by the type invariant of
        // `XArrayState and we hold the xarray lock`.
        let ptr = unsafe { bindings::xas_load(&raw mut self.state) };
        NonNull::new(ptr.cast())
    }

    fn load_next(&mut self) -> Option<(usize, NonNull<c_void>)> {
        // SAFETY: `self.state` is always valid by the type invariant of
        // `XArrayState` and the we hold the xarray lock.
        let ptr = unsafe { bindings::xas_find(&raw mut self.state, usize::MAX) };
        NonNull::new(ptr).map(|ptr| (self.state.xa_index, ptr))
    }

    fn status(&self) -> Result {
        // SAFETY: `self.state` is properly initialized and valid.
        to_result(unsafe { bindings::xas_error(&self.state) })
    }

    fn insert(&mut self, value: T) -> Result<*mut c_void, StoreError<T>> {
        let new = T::into_foreign(value).cast();

        // SAFETY: `self.state.state` is properly initialized and `new` came from `T::into_foreign`.
        // We hold the xarray lock.
        unsafe { bindings::xas_store(&mut self.state, new) };

        self.status().map(|()| new).map_err(|error| {
            // SAFETY: `new` came from `T::into_foreign` and `xas_store` does not take ownership of
            // the value on error.
            let value = unsafe { T::from_foreign(new) };
            StoreError { value, error }
        })
    }
}

mod entry;

// SAFETY: `XArray<T>` has no shared mutable state so it is `Send` iff `T` is `Send`.
unsafe impl<T: ForeignOwnable + Send> Send for XArray<T> {}

// SAFETY: `XArray<T>` serialises the interior mutability it provides so it is `Sync` iff `T` is
// `Send`.
unsafe impl<T: ForeignOwnable + Send> Sync for XArray<T> {}

// SPDX-License-Identifier: GPL-2.0

use super::{
    Guard,
    StoreError,
    XArrayState, //
};
use core::ptr::NonNull;
use kernel::{
    prelude::*,
    types::ForeignOwnable, //
};

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
    /// # use kernel::{prelude::*, xarray::{AllocKind, XArray, Entry}};
    /// let mut xa = KBox::pin_init(XArray::<KBox<u32>>::new(AllocKind::Alloc), GFP_KERNEL)?;
    /// let mut guard = xa.lock();
    ///
    ///
    /// let entry = guard.get_entry(42);
    /// assert_eq!(entry.is_occupied(), false);
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
    pub(crate) fn new(guard: &'b mut Guard<'a, T>, index: usize) -> Self {
        Self {
            state: XArrayState::new(guard, index),
        }
    }

    fn insert_internal(&mut self, value: T) -> Result<*mut c_void, StoreError<T>> {
        let new = T::into_foreign(value).cast();

        // SAFETY: `self.state.state` is properly initialized and `new` came from `T::into_foreign`.
        // We hold the xarray lock.
        unsafe { bindings::xas_store(&mut self.state.state, new) };

        self.state.status().map(|()| new).map_err(|error| {
            // SAFETY: `new` came from `T::into_foreign` and `xas_store` does not take ownership of
            // the value on error.
            let value = unsafe { T::from_foreign(new) };
            StoreError { value, error }
        })
    }

    /// Inserts a value into this vacant entry.
    ///
    /// Returns a reference to the newly inserted value.
    ///
    /// - This method will fail if the nodes on the path to the index
    ///   represented by this entry are not present in the XArray.
    /// - This method will not drop the XArray lock.
    ///
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{AllocKind, XArray, Entry}};
    /// let mut xa = KBox::pin_init(XArray::<KBox<u32>>::new(AllocKind::Alloc), GFP_KERNEL)?;
    /// let mut guard = xa.lock();
    ///
    /// assert_eq!(guard.get(42), None);
    ///
    /// if let Entry::Vacant(entry) = guard.get_entry(42) {
    ///     let value = KBox::new(0x1337u32, GFP_KERNEL)?;
    ///     let borrowed = entry.insert(value)?;
    ///     assert_eq!(*borrowed, 0x1337);
    /// }
    ///
    /// assert_eq!(guard.get(42).copied(), Some(0x1337));
    ///
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn insert(mut self, value: T) -> Result<T::BorrowedMut<'b>, StoreError<T>> {
        let new = self.insert_internal(value)?;

        // SAFETY: `new` came from `T::into_foreign`. The entry has exclusive
        // ownership of `new` as it holds a mutable reference to `Guard`.
        Ok(unsafe { T::borrow_mut(new) })
    }

    /// Inserts a value and returns an occupied entry representing the newly inserted value.
    ///
    /// - This method will fail if the nodes on the path to the index
    ///   represented by this entry are not present in the XArray.
    /// - This method will not drop the XArray lock.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{AllocKind, XArray, Entry}};
    /// let mut xa = KBox::pin_init(XArray::<KBox<u32>>::new(AllocKind::Alloc), GFP_KERNEL)?;
    /// let mut guard = xa.lock();
    ///
    /// assert_eq!(guard.get(42), None);
    ///
    /// if let Entry::Vacant(entry) = guard.get_entry(42) {
    ///     let value = KBox::new(0x1337u32, GFP_KERNEL)?;
    ///     let occupied = entry.insert_entry(value)?;
    ///     assert_eq!(occupied.index(), 42);
    /// }
    ///
    /// assert_eq!(guard.get(42).copied(), Some(0x1337));
    ///
    /// # Ok::<(), kernel::error::Error>(())
    /// ```
    pub fn insert_entry(mut self, value: T) -> Result<OccupiedEntry<'a, 'b, T>, StoreError<T>> {
        let new = self.insert_internal(value)?;

        Ok(OccupiedEntry::<'a, 'b, T> {
            state: self.state,
            // SAFETY: `new` came from `T::into_foreign` and is guaranteed non-null.
            ptr: unsafe { core::ptr::NonNull::new_unchecked(new) },
        })
    }

    /// Returns the index of this vacant entry.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{AllocKind, XArray, Entry}};
    /// let mut xa = KBox::pin_init(XArray::<KBox<u32>>::new(AllocKind::Alloc), GFP_KERNEL)?;
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
    pub(crate) state: XArrayState<'a, 'b, T>,
    pub(crate) ptr: NonNull<c_void>,
}

impl<'a, 'b, T> OccupiedEntry<'a, 'b, T>
where
    T: ForeignOwnable,
{
    pub(crate) fn new(guard: &'b mut Guard<'a, T>, index: usize, ptr: NonNull<c_void>) -> Self {
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
    /// # use kernel::{prelude::*, xarray::{AllocKind, XArray, Entry}};
    /// let mut xa = KBox::pin_init(XArray::<KBox<u32>>::new(AllocKind::Alloc), GFP_KERNEL)?;
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
                bindings::xa_zero_to_null(bindings::xas_store(
                    &mut self.state.state,
                    core::ptr::null_mut(),
                )),
            )
        };

        // SAFETY: `ptr` is a valid return value from xas_result.
        let errno = unsafe { bindings::xa_err(ptr) };
        debug_assert!(errno == 0);

        // SAFETY:
        // - `ptr` came from `T::into_foreign`.
        // - As this method takes self by value, the lifetimes of any [`T::Borrowed`] and
        //   [`T::BorrowedMut`] we have created must have ended.
        unsafe { T::from_foreign(ptr.cast()) }
    }

    /// Returns the index of this occupied entry.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{AllocKind, XArray, Entry}};
    /// let mut xa = KBox::pin_init(XArray::<KBox<u32>>::new(AllocKind::Alloc), GFP_KERNEL)?;
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
    /// # use kernel::{prelude::*, xarray::{AllocKind, XArray, Entry}};
    /// let mut xa = KBox::pin_init(XArray::<KBox<u32>>::new(AllocKind::Alloc), GFP_KERNEL)?;
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
        // - `ptr` came from `T::into_foreign`.
        // - As this method takes self by value, the lifetimes of any [`T::Borrowed`] and
        //   [`T::BorrowedMut`] we have created must have ended.
        unsafe { T::from_foreign(old) }
    }

    /// Converts this occupied entry into a mutable reference to the value in the slot represented
    /// by the entry.
    ///
    /// # Examples
    ///
    /// ```
    /// # use kernel::{prelude::*, xarray::{AllocKind, XArray, Entry}};
    /// let mut xa = KBox::pin_init(XArray::<KBox<u32>>::new(AllocKind::Alloc), GFP_KERNEL)?;
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
    /// # use kernel::{prelude::*, xarray::{AllocKind, XArray, Entry}};
    /// let mut xa = KBox::pin_init(XArray::<KBox<u32>>::new(AllocKind::Alloc), GFP_KERNEL)?;
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
        use core::ops::DerefMut;
        core::mem::swap(self.deref_mut(), &mut other);
        other
    }
}

impl<T, U> core::ops::Deref for OccupiedEntry<'_, '_, T>
where
    T: for<'a> ForeignOwnable<Borrowed<'a> = &'a U, BorrowedMut<'a> = &'a mut U>,
{
    type Target = U;

    fn deref(&self) -> &Self::Target {
        // SAFETY: `ptr` came from `T::into_foreign`.
        unsafe { T::borrow(self.ptr.as_ptr()) }
    }
}

impl<T, U> core::ops::DerefMut for OccupiedEntry<'_, '_, T>
where
    T: for<'a> ForeignOwnable<Borrowed<'a> = &'a U, BorrowedMut<'a> = &'a mut U>,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: `ptr` came from `T::into_foreign`.
        unsafe { T::borrow_mut(self.ptr.as_ptr()) }
    }
}

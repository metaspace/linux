// SPDX-License-Identifier: GPL-2.0

//! Internal reference counting support.

use core::{marker::PhantomData, mem::ManuallyDrop, ops::Deref, ptr::NonNull};

/// Types that are internally reference counted.
///
/// It allows such types to define their own custom ref increment and decrement functions.
///
/// This is usually implemented by wrappers to existing structures on the C side of the code. For
/// Rust code, the recommendation is to use [`Arc`](crate::sync::Arc) to create reference-counted
/// instances of a type.
///
/// Note: Implementing this trait allows types to be wrapped in an [`ARef<Self>`]. It requires an
/// internal reference count and provides only shared references. If unique references are required
/// [`Ownable`] should be implemented which allows types to be wrapped in an [`Owned<Self>`].
/// Implementing the trait [`OwnableRefCounted`] allows to convert between unique and shared
/// references (i.e. [`Owned<Self>`] and [`ARef<Self>`]).
///
/// # Safety
///
/// Implementers must ensure that increments to the reference count keep the object alive in memory
/// at least until matching decrements are performed.
///
/// Implementers must also ensure that all instances are reference-counted. (Otherwise they
/// won't be able to honour the requirement that [`RefCounted::inc_ref`] keep the object alive.)
pub unsafe trait RefCounted {
    /// Increments the reference count on the object.
    fn inc_ref(&self);

    /// Decrements the reference count on the object.
    ///
    /// Frees the object when the count reaches zero.
    ///
    /// # Safety
    ///
    /// Callers must ensure that there was a previous matching increment to the reference count,
    /// and that the object is no longer used after its reference count is decremented (as it may
    /// result in the object being freed), unless the caller owns another increment on the refcount
    /// (e.g., it calls [`RefCounted::inc_ref`] twice, then calls [`RefCounted::dec_ref`] once).
    unsafe fn dec_ref(obj: NonNull<Self>);
}

/// An extension to RefCounted, which declares that it is allowed to convert from a shared reference
/// `&T` to an owned reference [`ARef<T>`].
///
/// # Safety
///
/// Implementers must ensure that no safety invariants are violated by upgrading an `&T` to an
/// [`ARef<T>`]. In particular that implies [`AlwaysRefCounted`] and [`Ownable`] cannot be
/// implemented for the same type, as this would allow to violate the uniqueness guarantee of
/// [`Owned<T>`] by derefencing it into an `&T` and obtaining an [`ARef`] from that.
pub unsafe trait AlwaysRefCounted: RefCounted {}

/// An owned reference to an always-reference-counted object.
///
/// The object's reference count is automatically decremented when an instance of [`ARef`] is
/// dropped. It is also automatically incremented when a new instance is created via
/// [`ARef::clone`].
///
/// # Invariants
///
/// The pointer stored in `ptr` is non-null and valid for the lifetime of the [`ARef`] instance. In
/// particular, the [`ARef`] instance owns an increment on the underlying object's reference count.
pub struct ARef<T: RefCounted> {
    ptr: NonNull<T>,
    _p: PhantomData<T>,
}

// SAFETY: It is safe to send `ARef<T>` to another thread when the underlying `T` is `Sync` because
// it effectively means sharing `&T` (which is safe because `T` is `Sync`); additionally, it needs
// `T` to be `Send` because any thread that has an `ARef<T>` may ultimately access `T` using a
// mutable reference, for example, when the reference count reaches zero and `T` is dropped.
unsafe impl<T: RefCounted + Sync + Send> Send for ARef<T> {}

// SAFETY: It is safe to send `&ARef<T>` to another thread when the underlying `T` is `Sync`
// because it effectively means sharing `&T` (which is safe because `T` is `Sync`); additionally,
// it needs `T` to be `Send` because any thread that has a `&ARef<T>` may clone it and get an
// `ARef<T>` on that thread, so the thread may ultimately access `T` using a mutable reference, for
// example, when the reference count reaches zero and `T` is dropped.
unsafe impl<T: RefCounted + Sync + Send> Sync for ARef<T> {}

impl<T: RefCounted> ARef<T> {
    /// Creates a new instance of [`ARef`].
    ///
    /// It takes over an increment of the reference count on the underlying object.
    ///
    /// # Safety
    ///
    /// Callers must ensure that the reference count was incremented at least once, and that they
    /// are properly relinquishing one increment. That is, if there is only one increment, callers
    /// must not use the underlying object anymore -- it is only safe to do so via the newly
    /// created [`ARef`].
    pub unsafe fn from_raw(ptr: NonNull<T>) -> Self {
        // INVARIANT: The safety requirements guarantee that the new instance now owns the
        // increment on the refcount.
        Self {
            ptr,
            _p: PhantomData,
        }
    }

    /// Consumes the `ARef`, returning a raw pointer.
    ///
    /// This function does not change the refcount. After calling this function, the caller is
    /// responsible for the refcount previously managed by the `ARef`.
    ///
    /// # Examples
    ///
    /// ```
    /// use core::ptr::NonNull;
    /// use kernel::sync::aref::{ARef, RefCounted};
    ///
    /// struct Empty {}
    ///
    /// // SAFETY: The `RefCounted` implementation for `Empty` does not count references and
    /// // never frees the underlying object. Thus we can act as having a refcount on the object
    /// // that we pass to the newly created `ARef`.
    /// unsafe impl RefCounted for Empty {
    ///     fn inc_ref(&self) {}
    ///     unsafe fn dec_ref(_obj: NonNull<Self>) {}
    /// }
    ///
    /// let mut data = Empty {};
    /// let ptr = NonNull::<Empty>::new(&mut data).unwrap();
    /// // SAFETY: We keep `data` around longer than the `ARef`.
    /// let data_ref: ARef<Empty> = unsafe { ARef::from_raw(ptr) };
    /// let raw_ptr: NonNull<Empty> = ARef::into_raw(data_ref);
    ///
    /// assert_eq!(ptr, raw_ptr);
    /// ```
    pub fn into_raw(me: Self) -> NonNull<T> {
        ManuallyDrop::new(me).ptr
    }
}

impl<T: RefCounted> Clone for ARef<T> {
    fn clone(&self) -> Self {
        self.inc_ref();
        // SAFETY: We just incremented the refcount above.
        unsafe { Self::from_raw(self.ptr) }
    }
}

impl<T: RefCounted> Deref for ARef<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: The type invariants guarantee that the object is valid.
        unsafe { self.ptr.as_ref() }
    }
}

impl<T: AlwaysRefCounted> From<&T> for ARef<T> {
    fn from(b: &T) -> Self {
        b.inc_ref();
        // SAFETY: We just incremented the refcount above.
        unsafe { Self::from_raw(NonNull::from(b)) }
    }
}

impl<T: crate::types::OwnableRefCounted> From<crate::types::Owned<T>> for ARef<T> {
    fn from(b: crate::types::Owned<T>) -> Self {
        T::into_shared(b)
    }
}

impl<T: RefCounted> Drop for ARef<T> {
    fn drop(&mut self) {
        // SAFETY: The type invariants guarantee that the `ARef` owns the reference we're about to
        // decrement.
        unsafe { T::dec_ref(self.ptr) };
    }
}

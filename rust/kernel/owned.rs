// SPDX-License-Identifier: GPL-2.0

//! Unique owned pointer types for objects with custom drop logic.
//!
//! These pointer types are useful for C-allocated objects which by API-contract
//! are owned by Rust, but need to be freed through the C API.

use core::{
    mem::ManuallyDrop,
    ops::{
        Deref,
        DerefMut, //
    },
    pin::Pin,
    ptr::NonNull, //
};
use kernel::{
    sync::aref::ARef,
    types::{
        ForeignOwnable, //
        RefCounted,
    }, //
};

/// Types that specify their own way of performing allocation and destruction. Typically, this trait
/// is implemented on types from the C side.
///
/// Implementing this trait allows types to be referenced via the [`Owned<Self>`] pointer type.
///  - This is useful when it is desirable to tie the lifetime of an object reference to an owned
///    object, rather than pass around a bare reference.
///  - [`Ownable`] types can define custom drop logic that is executed when the owned reference
///    of type [`Owned<_>`] pointing to the object is dropped.
///
/// Note: The underlying object is not required to provide internal reference counting, because it
/// represents a unique, owned reference. If reference counting (on the Rust side) is required,
/// [`RefCounted`] should be implemented. [`OwnableRefCounted`] should be implemented if conversion
/// between unique and shared (reference counted) ownership is needed.
///
/// # Safety
///
/// Implementers must ensure that the [`release()`](Self::release) function frees the underlying
/// object in the correct way for a valid, owned object of this type.
///
/// # Examples
///
/// A minimal example implementation of [`Ownable`] and its usage with [`Owned`] looks like
/// this:
///
/// ```
/// # #![expect(clippy::disallowed_names)]
/// # use core::cell::Cell;
/// # use core::ptr::NonNull;
/// # use kernel::sync::global_lock;
/// # use kernel::alloc::{flags, kbox::KBox, AllocError};
/// # use kernel::types::{Owned, Ownable};
///
/// // Let's count the allocations to see if freeing works.
/// kernel::sync::global_lock! {
///     // SAFETY: we call `init()` right below, before doing anything else.
///     unsafe(uninit) static FOO_ALLOC_COUNT: Mutex<usize> = 0;
/// }
/// // SAFETY: We call `init()` only once, here.
/// unsafe { FOO_ALLOC_COUNT.init() };
///
/// struct Foo;
///
/// impl Foo {
///     fn new() -> Result<Owned<Self>> {
///         // We are just using a `KBox` here to handle the actual allocation, as our `Foo` is
///         // not actually a C-allocated object.
///         let result = KBox::new(
///             Foo {},
///             flags::GFP_KERNEL,
///         )?;
///         let result = NonNull::new(KBox::into_raw(result)).ok_or(ENOMEM)?;
///         // Count new allocation
///         *FOO_ALLOC_COUNT.lock() += 1;
///         // SAFETY: We just allocated the `Self`, thus it is valid and there cannot be any other
///         // Rust references. Calling `into_raw()` makes us responsible for ownership and we won't
///         // use the raw pointer anymore. Thus we can transfer ownership to the `Owned`.
///         Ok(unsafe { Owned::from_raw(result) })
///     }
/// }
///
/// // SAFETY: The implementation of `release` in this trait implementation correctly frees the
/// // owned `Foo`.
/// unsafe impl Ownable for Foo {
///     unsafe fn release(this: NonNull<Self>) {
///         // SAFETY: The [`KBox<Self>`] is still alive. We can pass ownership to the [`KBox`], as
///         // by requirement on calling this function, the `Self` will no longer be used by the
///         // caller.
///         drop(unsafe { KBox::from_raw(this.as_ptr()) });
///         // Count released allocation
///         *FOO_ALLOC_COUNT.lock() -= 1;
///     }
/// }
///
/// {
///    let foo = Foo::new()?;
///    assert!(*FOO_ALLOC_COUNT.lock() == 1);
/// }
/// // `foo` is out of scope now, so we expect no live allocations.
/// assert!(*FOO_ALLOC_COUNT.lock() == 0);
/// # Ok::<(), Error>(())
/// ```
pub unsafe trait Ownable {
    /// Releases the object.
    ///
    /// # Safety
    ///
    /// Callers must ensure that:
    /// - `this` points to a valid `Self`.
    /// - `*this` is no longer used after this call.
    unsafe fn release(this: NonNull<Self>);
}

/// A mutable reference to an owned `T`.
///
/// The [`Ownable`] is automatically freed or released when an instance of [`Owned`] is
/// dropped.
///
/// # Invariants
///
/// - The [`Owned<T>`] has exclusive access to the instance of `T`.
/// - The instance of `T` will stay alive at least as long as the [`Owned<T>`] is alive.
#[repr(transparent)]
pub struct Owned<T: Ownable> {
    ptr: NonNull<T>,
}

impl<T: Ownable> Owned<T> {
    /// Creates a new instance of [`Owned`].
    ///
    /// This function takes over ownership of the underlying object.
    ///
    /// # Safety
    ///
    /// Callers must ensure that:
    /// - `ptr` points to a valid instance of `T`.
    /// - Ownership of the underlying `T` can be transferred to the `Self<T>` (i.e. operations
    ///   which require ownership will be safe).
    /// - An `Owned<T>` is a mutable reference to the underlying object. As such,
    ///   the object must not be accessed (read or mutated) through any pointer
    ///   other than the created `Owned<T>`. Opt-out is still possible similar to
    ///   a mutable reference (e.g. by using [`Opaque`]).
    ///
    /// [`Opaque`]: kernel::types::Opaque
    pub unsafe fn from_raw(ptr: NonNull<T>) -> Self {
        // INVARIANT: By function safety requirement:
        // - The resulting object has exclusive access to the `T` pointed to by `ptr`.
        // - The `T` object pointed to by `ptr` is alive at least as long as the returned `Self`.
        Self { ptr }
    }

    /// Consumes the [`Owned`], returning a raw pointer.
    ///
    /// This function does not drop the underlying `T`. When this function returns, ownership of the
    /// underlying `T` is with the caller.
    pub fn into_raw(me: Self) -> NonNull<T> {
        ManuallyDrop::new(me).ptr
    }

    /// Get a pinned mutable reference to the data owned by this `Owned<T>`.
    pub fn get_pin_mut(&mut self) -> Pin<&mut T> {
        // SAFETY: The type invariants guarantee that the object is valid, and that we can safely
        // return a mutable reference to it.
        let unpinned = unsafe { self.ptr.as_mut() };

        // SAFETY: We never hand out unpinned mutable references to the data in
        // `Self`, unless the contained type is `Unpin`.
        unsafe { Pin::new_unchecked(unpinned) }
    }
}

// SAFETY: It is safe to send an [`Owned<T>`] to another thread when the underlying `T` is [`Send`],
// because of the ownership invariant. Sending an [`Owned<T>`] is equivalent to sending the `T`.
unsafe impl<T: Ownable + Send> Send for Owned<T> {}

// SAFETY: It is safe to send [`&Owned<T>`] to another thread when the underlying `T` is [`Sync`],
// because of the ownership invariant. Sending an [`&Owned<T>`] is equivalent to sending the `&T`.
unsafe impl<T: Ownable + Sync> Sync for Owned<T> {}

impl<T: Ownable> Deref for Owned<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: The type invariants guarantee that the object is valid.
        unsafe { self.ptr.as_ref() }
    }
}

impl<T: Ownable + Unpin> DerefMut for Owned<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: The type invariants guarantee that the object is valid, and that we can safely
        // return a mutable reference to it.
        unsafe { self.ptr.as_mut() }
    }
}

impl<T: Ownable> Drop for Owned<T> {
    fn drop(&mut self) {
        // SAFETY: The type invariants guarantee that the `Owned` owns the object we're about to
        // release.
        unsafe { T::release(self.ptr) };
    }
}

// SAFETY: We derive the pointer to `T` from a valid `T`, so the returned
// pointer satisfy alignment requirements of `T`.
unsafe impl<T: Ownable + 'static> ForeignOwnable for Owned<T> {
    const FOREIGN_ALIGN: usize = core::mem::align_of::<Owned<T>>();

    type Borrowed<'a> = &'a T;
    type BorrowedMut<'a> = Pin<&'a mut T>;

    fn into_foreign(self) -> *mut kernel::ffi::c_void {
        let ptr = self.ptr.as_ptr().cast();
        core::mem::forget(self);
        ptr
    }

    unsafe fn from_foreign(ptr: *mut kernel::ffi::c_void) -> Self {
        Self {
            // SAFETY: By function safety contract, `ptr` came from
            // `into_foreign` and cannot be null.
            ptr: unsafe { NonNull::new_unchecked(ptr.cast()) },
        }
    }

    unsafe fn borrow<'a>(ptr: *mut kernel::ffi::c_void) -> Self::Borrowed<'a> {
        // SAFETY: By function safety requirements, `ptr` is valid for use as a
        // reference for `'a`.
        unsafe { &*ptr.cast() }
    }

    unsafe fn borrow_mut<'a>(ptr: *mut kernel::ffi::c_void) -> Self::BorrowedMut<'a> {
        // SAFETY: By function safety requirements, `ptr` is valid for use as a
        // unique reference for `'a`.
        let inner = unsafe { &mut *ptr.cast() };

        // SAFETY: We never move out of inner, and we do not hand out mutable
        // references when `T: !Unpin`.
        unsafe { Pin::new_unchecked(inner) }
    }
}

/// A trait for objects that can be wrapped in either one of the reference types [`Owned`] and
/// [`ARef`].
///
/// # Examples
///
/// A minimal example implementation of [`OwnableRefCounted`], [`Ownable`] and its usage with
/// [`ARef`] and [`Owned`] looks like this:
///
/// ```
/// # #![expect(clippy::disallowed_names)]
/// # use core::cell::Cell;
/// # use core::ptr::NonNull;
/// # use kernel::alloc::{flags, kbox::KBox, AllocError};
/// # use kernel::sync::aref::{ARef, RefCounted};
/// # use kernel::types::{Owned, Ownable, OwnableRefCounted};
///
/// // An internally refcounted struct for demonstration purposes.
/// //
/// // # Invariants
/// //
/// // - `refcount` is always non-zero for a valid object.
/// // - `refcount` is >1 if there is more than one Rust reference to it.
/// //
/// struct Foo {
///     refcount: Cell<usize>,
/// }
///
/// impl Foo {
///     fn new() -> Result<Owned<Self>> {
///         // We are just using a `KBox` here to handle the actual allocation, as our `Foo` is
///         // not actually a C-allocated object.
///         let result = KBox::new(
///             Foo {
///                 refcount: Cell::new(1),
///             },
///             flags::GFP_KERNEL,
///         )?;
///         let result = NonNull::new(KBox::into_raw(result)).ok_or(ENOMEM)?;
///         // SAFETY: We just allocated the `Self`, thus it is valid and there cannot be any other
///         // Rust references. Calling `into_raw()` makes us responsible for ownership and
///         // we won't use the raw pointer anymore, thus we can transfer ownership to the `Owned`.
///         Ok(unsafe { Owned::from_raw(result) })
///     }
/// }
///
/// // SAFETY: We increment and decrement each time the respective function is called and only free
/// // the `Foo` when the refcount reaches zero.
/// unsafe impl RefCounted for Foo {
///     fn inc_ref(&self) {
///         self.refcount.replace(self.refcount.get() + 1);
///     }
///
///     unsafe fn dec_ref(this: NonNull<Self>) {
///         // SAFETY: By requirement on calling this function, the refcount is non-zero,
///         // implying the underlying object is valid.
///         let refcount = unsafe { &this.as_ref().refcount };
///         let new_refcount = refcount.get() - 1;
///         if new_refcount == 0 {
///             // The `Foo` will be dropped when `KBox` goes out of scope.
///             // SAFETY: The [`KBox<Foo>`] is still alive as the old refcount is 1. We can pass
///             // ownership to the [`KBox`] as by requirement on calling this function,
///             // the `Self` will no longer be used by the caller.
///             unsafe { KBox::from_raw(this.as_ptr()) };
///         } else {
///             refcount.replace(new_refcount);
///         }
///     }
/// }
///
/// impl OwnableRefCounted for Foo {
///     fn try_from_shared(this: ARef<Self>) -> Result<Owned<Self>, ARef<Self>> {
///         if this.refcount.get() == 1 {
///             // SAFETY: The `Foo` is still alive and has no other Rust references as the refcount
///             // is 1.
///             Ok(unsafe { Owned::from_raw(ARef::into_raw(this)) })
///         } else {
///             Err(this)
///         }
///     }
/// }
///
/// // SAFETY: This implementation of `release()` is safe for any valid `Self`.
/// unsafe impl Ownable for Foo {
///     unsafe fn release(this: NonNull<Self>) {
///         // SAFETY: Using `dec_ref()` from [`RefCounted`] to release is okay, as the refcount is
///         // always 1 for an [`Owned<Foo>`].
///         unsafe{ Foo::dec_ref(this) };
///     }
/// }
///
/// let foo = Foo::new()?;
/// let mut foo = ARef::from(foo);
/// {
///     let bar = foo.clone();
///     assert!(Owned::try_from(bar).is_err());
/// }
/// assert!(Owned::try_from(foo).is_ok());
/// # Ok::<(), Error>(())
/// ```
pub trait OwnableRefCounted: RefCounted + Ownable + Sized {
    /// Checks if the [`ARef`] is unique and converts it to an [`Owned`] if that is the case.
    /// Otherwise it returns again an [`ARef`] to the same underlying object.
    fn try_from_shared(this: ARef<Self>) -> Result<Owned<Self>, ARef<Self>>;

    /// Converts the [`Owned`] into an [`ARef`].
    fn into_shared(this: Owned<Self>) -> ARef<Self> {
        // SAFETY: Safe by the requirements on implementing the trait.
        unsafe { ARef::from_raw(Owned::into_raw(this)) }
    }
}

impl<T: OwnableRefCounted> TryFrom<ARef<T>> for Owned<T> {
    type Error = ARef<T>;
    /// Tries to convert the [`ARef`] to an [`Owned`] by calling
    /// [`try_from_shared()`](OwnableRefCounted::try_from_shared). In case the [`ARef`] is not
    /// unique, it returns again an [`ARef`] to the same underlying object.
    fn try_from(b: ARef<T>) -> Result<Owned<T>, Self::Error> {
        T::try_from_shared(b)
    }
}

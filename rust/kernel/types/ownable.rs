// SPDX-License-Identifier: GPL-2.0

//! Owned reference types.

use crate::{
    prelude::*,
    sync::aref::{ARef, RefCounted},
    types::ForeignOwnable,
};
use core::{
    marker::PhantomData,
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

/// Types that may be owned by Rust code or borrowed, but have a lifetime managed by C code.
///
/// It allows such types to define their own custom destructor function to be called when a
/// Rust-owned reference is dropped.
///
/// This is usually implemented by wrappers to existing structures on the C side of the code.
///
/// Note: Implementing this trait allows types to be wrapped in an [`Owned<Self>`]. This does not
/// provide reference counting but represents a unique, owned reference. If reference counting is
/// required [`RefCounted`] should be implemented which allows types to be wrapped in an
/// [`ARef<Self>`]. Implementing the trait [`OwnableRefCounted`] allows to convert between unique
/// and shared references (i.e. [`Owned<Self>`] and [`ARef<Self>`]).
///
/// # Safety
///
/// Implementers must ensure that:
/// - The [`release()`](Ownable::release) method leaves the underlying object in a state which the
///   kernel expects after ownership has been relinquished (i.e. no dangling references in the
///   kernel is case it frees the object, etc.).
pub unsafe trait Ownable {
    /// Releases the object (frees it or returns it to foreign ownership).
    ///
    /// # Safety
    ///
    /// Callers must ensure that:
    /// - `this` points to a valid `Self`.
    /// - The object is no longer referenced after this call.
    unsafe fn release(this: NonNull<Self>);
}

/// Type where [`Owned<Self>`] derefs to `&mut Self`.
///
/// # Safety
///
/// Implementers must ensure that access to a `&mut T` is safe, implying that:
/// - It is safe to call [`core::mem::swap`] on the [`Ownable`]. This excludes pinned types
///   (i.e. most kernel types).
/// - The kernel will never access the underlying object (excluding internal mutability that follows
///   the usual rules) while Rust owns it.
pub unsafe trait OwnableMut: Ownable {}

/// An owned reference to an ownable kernel object.
///
/// The object is automatically freed or released when an instance of [`Owned`] is
/// dropped.
///
/// # Invariants
///
/// The pointer stored in `ptr` can be considered owned by the [`Owned`] instance.
#[repr(transparent)]
pub struct Owned<T: Ownable> {
    ptr: NonNull<T>,
    _p: PhantomData<T>,
}

// SAFETY: It is safe to send `Owned<T>` to another thread when the underlying `T` is `Send` because
// it effectively means sending a `&mut T` (which is safe because `T` is `Send`).
unsafe impl<T: Ownable + Send> Send for Owned<T> {}

// SAFETY: It is safe to send `&Owned<T>` to another thread when the underlying `T` is `Sync`
// because it effectively means sharing `&T` (which is safe because `T` is `Sync`).
unsafe impl<T: Ownable + Sync> Sync for Owned<T> {}

impl<T: Ownable> Owned<T> {
    /// Creates a new instance of [`Owned`].
    ///
    /// It takes over ownership of the underlying object.
    ///
    /// # Safety
    ///
    /// Callers must ensure that:
    /// - Ownership of the underlying object can be transferred to the `Owned<T>` (i.e. operations
    ///   which require ownership will be safe).
    /// - No other Rust references to the underlying object exist. This implies that the underlying
    ///   object is not accessed through `ptr` anymore after the function call (at least until the
    ///   the `Owned<T>` is dropped).
    /// - The C code follows the usual shared reference requirements. That is, the kernel will never
    ///   mutate or free the underlying object (excluding interior mutability that follows the usual
    ///   rules) while Rust owns it.
    /// - In case `T` implements [`OwnableMut`] the previous requirement is extended from shared to
    ///   mutable reference requirements. That is, the kernel will not mutate or free the underlying
    ///   object and is okay with it being modified by Rust code.
    pub unsafe fn from_raw(ptr: NonNull<T>) -> Self {
        // INVARIANT: The safety requirements guarantee that the new instance now owns the
        // reference.
        Self {
            ptr,
            _p: PhantomData,
        }
    }

    /// Consumes the [`Owned`], returning a raw pointer.
    ///
    /// This function does not actually relinquish ownership of the object. After calling this
    /// function, the caller is responsible for ownership previously managed
    /// by the [`Owned`].
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

impl<T: Ownable> Deref for Owned<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: The type invariants guarantee that the object is valid.
        unsafe { self.ptr.as_ref() }
    }
}

impl<T: OwnableMut + Unpin> DerefMut for Owned<T> {
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
            ptr: unsafe { NonNull::new_unchecked(ptr.cast()) },
            _p: PhantomData,
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
        unsafe {Pin::new_unchecked(inner)}
    }
}

/// A trait for objects that can be wrapped in either one of the reference types [`Owned`] and
/// [`ARef`].
///
/// # Safety
///
/// Implementers must ensure that:
///
/// - [`try_from_shared()`](OwnableRefCounted::into_shared) only returns an [`Owned<Self>`] if
///   exactly one [`ARef<Self>`] exists.
/// - [`into_shared()`](OwnableRefCounted::into_shared) set the reference count to the value which
///   the returned [`ARef<Self>`] expects for an object with a single reference in existence. This
///   implies that if [`into_shared()`](OwnableRefCounted::into_shared) is left on the default
///   implementation, which just rewraps the underlying object, the reference count needs not to be
///   modified when converting an [`Owned<Self>`] to an [`ARef<Self>`].
///
/// # Examples
///
/// A minimal example implementation of [`OwnableRefCounted`], [`Ownable`] and its usage with
/// [`ARef`] and [`Owned`] looks like this:
///
/// ```
/// # #![expect(clippy::disallowed_names)]
/// use core::cell::Cell;
/// use core::ptr::NonNull;
/// use kernel::alloc::{flags, kbox::KBox, AllocError};
/// use kernel::sync::aref::{ARef, RefCounted};
/// use kernel::types::{Owned, Ownable, OwnableRefCounted};
///
/// struct Foo {
///     refcount: Cell<usize>,
/// }
///
/// impl Foo {
///     fn new() -> Result<Owned<Self>, AllocError> {
///         // Use a `KBox` to handle the actual allocation.
///         let result = KBox::new(
///             Foo {
///                 refcount: Cell::new(1),
///             },
///             flags::GFP_KERNEL,
///         )?;
///         let result = NonNull::new(KBox::into_raw(result))
///             .expect("Raw pointer to newly allocation KBox is null, this should never happen.");
///         // SAFETY: We just allocated the `Foo`, thus it is valid.
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
///         // SAFETY: The underlying object is always valid when the function is called.
///         let refcount = unsafe { &this.as_ref().refcount };
///         let new_refcount = refcount.get() - 1;
///         if new_refcount == 0 {
///             // The `Foo` will be dropped when `KBox` goes out of scope.
///             // SAFETY: The `Box<Foo>` is still alive as the old refcount is 1.
///             unsafe { KBox::from_raw(this.as_ptr()) };
///         } else {
///             refcount.replace(new_refcount);
///         }
///     }
/// }
///
/// // SAFETY: We only convert into an `Owned` when the refcount is 1.
/// unsafe impl OwnableRefCounted for Foo {
///     fn try_from_shared(this: ARef<Self>) -> Result<Owned<Self>, ARef<Self>> {
///         if this.refcount.get() == 1 {
///             // SAFETY: The `Foo` is still alive as the refcount is 1.
///             Ok(unsafe { Owned::from_raw(ARef::into_raw(this)) })
///         } else {
///             Err(this)
///         }
///     }
/// }
///
/// // SAFETY: We are not `AlwaysRefCounted`.
/// unsafe impl Ownable for Foo {
///     unsafe fn release(this: NonNull<Self>) {
///         // SAFETY: Using `dec_ref()` from `RefCounted` to release is okay, as the refcount is
///         // always 1 for an `Owned<Foo>`.
///         unsafe{ Foo::dec_ref(this) };
///     }
/// }
///
/// let foo = Foo::new().unwrap();
/// let mut foo = ARef::from(foo);
/// {
///     let bar = foo.clone();
///     assert!(Owned::try_from(bar).is_err());
/// }
/// assert!(Owned::try_from(foo).is_ok());
/// ```
pub unsafe trait OwnableRefCounted: RefCounted + Ownable + Sized {
    /// Checks if the [`ARef`] is unique and convert it to an [`Owned`] it that is that case.
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

// SPDX-License-Identifier: GPL-2.0

//! Owned reference types.

use crate::types::{ARef, RefCounted};
use core::{
    marker::PhantomData,
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

/// Types that may be owned by Rust code or borrowed, but have a lifetime managed by C code.
///
/// It allows such types to define their own custom destructor function to be called when
/// a Rust-owned reference is dropped.
///
/// This is usually implemented by wrappers to existing structures on the C side of the code.
///
/// # Safety
///
/// Implementers must ensure that:
/// - Any objects owned by Rust as [`Owned<T>`] stay alive while that owned reference exists (i.e.
///   until the [`release()`](Ownable::release) trait method is called).
/// - That the C code follows the usual mutable reference requirements. That is, the kernel will
///   never mutate the [`Ownable`] (excluding internal mutability that follows the usual rules)
///   while Rust owns it.
pub unsafe trait Ownable {
    /// Releases the object (frees it or returns it to foreign ownership).
    ///
    /// # Safety
    ///
    /// Callers must ensure that the object is no longer referenced after this call.
    unsafe fn release(this: NonNull<Self>);
}

/// A subtrait of Ownable that asserts that an [`Owned<T>`] or `&mut Owned<T>` Rust reference
/// may be dereferenced into a `&mut T`.
///
/// # Safety
///
/// Implementers must ensure that access to a `&mut T` is safe, implying that it is okay to call
/// [`core::mem::swap`] on the `Ownable`. This excludes pinned types (meaning: most kernel types).
pub unsafe trait OwnableMut: Ownable {}

/// An owned reference to an ownable kernel object.
///
/// The object is automatically freed or released when an instance of [`Owned`] is
/// dropped.
///
/// # Invariants
///
/// The pointer stored in `ptr` is valid for the lifetime of the [`Owned`] instance.
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
    /// Callers must ensure that the underlying object is acquired and can be considered owned by
    /// Rust.
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
    /// This function does not actually relinquish ownership of the object.
    /// After calling this function, the caller is responsible for ownership previously managed
    /// by the [`Owned`].
    pub fn into_raw(me: Self) -> NonNull<T> {
        ManuallyDrop::new(me).ptr
    }
}

impl<T: Ownable> Deref for Owned<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: The type invariants guarantee that the object is valid.
        unsafe { self.ptr.as_ref() }
    }
}

impl<T: OwnableMut> DerefMut for Owned<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: The type invariants guarantee that the object is valid,
        // and that we can safely return a mutable reference to it.
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

/// A trait for objects that can be wrapped in either one of the reference types [`Owned`] and
/// [`ARef`].
///
/// # Safety
///
/// Implementers must ensure that:
///
/// - Both the safety requirements for [`Ownable`] and [`RefCounted`] are fulfilled.
/// - [`try_from_shared()`](OwnableRefCounted::into_shared) only returns an [`Owned`] if exactly
///   one [`ARef`] exists.
/// - [`into_shared()`](OwnableRefCounted::into_shared) set the reference count to the value which
///   the returned [`ARef`] expects for an object with a single reference
///   in existence. This implies that if [`into_shared()`](OwnableRefCounted::into_shared) is left
///   on the default implementation, which just rewraps the underlying object, the reference count
///   needs not to be modified when converting a [`Owned`] to an [`ARef`].
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
/// use kernel::types::{
///     ARef, RefCounted, Owned, Ownable, OwnableRefCounted,
/// };
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

/// This trait allows to implement [`Ownable`] and [`OwnableRefCounted`] together in a simplified
/// way, only requiring to implement [`RefCounted`] and providing the method
/// [`is_unique()`](SimpleOwnableRefCounted::is_unique).
///
/// For non-standard cases where conversion between [`Ownable`] and [`RefCounted`] does not allow
/// [`Ownable::release()`] and [`RefCounted::dec_ref()`] to be the same method, [`Ownable`]
/// and [`OwnableRefCounted`] should be implemented separately.
///
/// # Safety
///
/// Implementers must ensure that:
///
/// - The safety requirements for [`Ownable`] are fulfilled and [`RefCounted::dec_ref()`] can
///   be used for [`Ownable::release()`].
/// - [`is_unique`](SimpleOwnableRefCounted::is_unique) must only return `true` in case only one
///   [`ARef`] exists and it is impossible for one to be obtained other than by cloning an existing
///   [`ARef`] or converting an [`Owned`] to an [`ARef`].
/// - It is safe to convert an unique [`ARef`] into an [`Owned`] simply by re-wrapping the
///   underlying object without modifying the refcount.
///
/// # Examples
///
/// A minimal example implementation of [`RefCounted`] and [`SimpleOwnableRefCounted`]
/// and its usage with [`ARef`] and [`Owned`] looks like this:
///
/// ```
/// # #![expect(clippy::disallowed_names)]
/// use core::cell::Cell;
/// use core::ptr::NonNull;
/// use kernel::alloc::{flags, kbox::KBox, AllocError};
/// use kernel::types::{
///     ARef, Owned, RefCounted, SimpleOwnableRefCounted,
/// };
///
/// struct Foo {
///     refcount: Cell<usize>,
/// }
///
/// impl Foo {
///     fn new() -> Result<Owned<Self>, AllocError> {
///         // Use a KBox to handle the actual allocation.
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
/// // SAFETY: we ensure that:
/// // - The `Foo` is only dropped when the refcount is zero.
/// // - `is_unique()` only returns `true` when the refcount is 1.
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
///             // The `Foo` will be dropped when KBox goes out of scope.
///             // SAFETY: The `Box<Foo>` is still alive as the old refcount is 1.
///             unsafe { KBox::from_raw(this.as_ptr()) };
///         } else {
///             refcount.replace(new_refcount);
///         }
///     }
/// }
///
/// // SAFETY: we ensure that:
/// // - `is_unique()` only returns `true` when the refcount is 1.
/// unsafe impl SimpleOwnableRefCounted for Foo {
///     fn is_unique(&self) -> bool {
///         self.refcount.get() == 1
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
pub unsafe trait SimpleOwnableRefCounted: RefCounted {
    /// Checks if exactly one [`ARef`] to the object exists. In case the object is [`Sync`], the
    /// check needs to be race-free.
    fn is_unique(&self) -> bool;
}

#[cfg_attr(RUSTC_HAS_DO_NOT_RECOMMEND, diagnostic::do_not_recommend)]
// SAFETY: Safe by the requirements on implementation of [`SimpleOwnableRefCounted`].
unsafe impl<T: SimpleOwnableRefCounted> OwnableRefCounted for T {
    fn try_from_shared(this: ARef<Self>) -> Result<Owned<Self>, ARef<Self>> {
        if T::is_unique(&*this) {
            // SAFETY: Safe by the requirements on implementation of [`SimpleOwnable`].
            Ok(unsafe { Owned::from_raw(ARef::into_raw(this)) })
        } else {
            Err(this)
        }
    }
}

#[cfg_attr(RUSTC_HAS_DO_NOT_RECOMMEND, diagnostic::do_not_recommend)]
// SAFETY: Safe by the requirements on implementation of [`SimpleOwnableRefCounted`].
unsafe impl<T: SimpleOwnableRefCounted> Ownable for T {
    unsafe fn release(this: NonNull<Self>) {
        // SAFETY: Safe by the requirements on implementation of
        // [`SimpleOwnableRefCounted::dec_ref()`].
        unsafe { RefCounted::dec_ref(this) };
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

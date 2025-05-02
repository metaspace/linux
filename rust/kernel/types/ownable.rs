// SPDX-License-Identifier: GPL-2.0

//! Owned reference types.

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

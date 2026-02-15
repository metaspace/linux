// SPDX-License-Identifier: GPL-2.0

use core::marker::PhantomData;

use crate::{owned::Owned, types::Opaque};

use super::{IdleRequest, Operations};

/// A list of [`Request`].
///
/// # INVARIANTS
///
/// - `self.inner` is always a valid list, meaning the `next` and `prev`
///   pointers point to valid requests, or are both null.
/// - All requests in the list are valid for use as `IdleRequest<T>`.
#[repr(transparent)]
pub struct RequestList<T: Operations> {
    inner: Opaque<bindings::rq_list>,
    _p: PhantomData<T>,
}

impl<T: Operations> RequestList<T> {
    /// Create a new [`RequestList`].
    pub fn new() -> Self {
        let this = Self {
            inner: Opaque::zeroed(),
            _p: PhantomData,
        };

        // NOTE: We are actually good to go, but we call the C initializer for forward
        // compatibility.
        // SAFETY: `this.inner` is a valid allocation for use as `bindings::rq_list!.
        unsafe { bindings::rq_list_init(this.inner.get()) }

        //INVARIANT: `self.inner` was initialized above and is empty.
        this
    }

    /// Create a mutable reference to a [`RequestList`] from a raw pointer.
    ///
    /// # SAFETY
    /// - The list pointed to by `ptr` must satisfy the invariants of `Self`.
    /// - The list pointed to by `ptr` must remain valid for use as a mutable reference for the
    ///   duration of `'a`.
    pub unsafe fn from_raw<'a>(ptr: *mut bindings::rq_list) -> &'a mut Self {
        // SAFETY:
        // - RequestList is transparent.
        // - By function safety requirements, `ptr` is valid for us as a mutable reference.
        unsafe { &mut (*ptr.cast()) }
    }

    /// Check if the list is empty.
    pub fn empty(&self) -> bool {
        // SAFETY: By type invariant, self.inner is valid.
        let ret = unsafe { bindings::rq_list_empty(self.inner.get()) };
        ret != 0
    }

    /// Pop a request from the list.
    ///
    /// Returns [`None`] if the list is empty.
    pub fn pop(&mut self) -> Option<Owned<IdleRequest<T>>> {
        // SAFETY: By type invariant `self.inner` is a valid list.
        let ptr = unsafe { bindings::rq_list_pop(self.inner.get()) };

        if !ptr.is_null() {
            // SAFETY: If `rq_list_pop` returns a non-null pointer, it points to a valid request. By
            // type invariant all requests in this list are valid for use as `IdleRequest`.
            Some(unsafe { IdleRequest::from_raw(ptr) })
        } else {
            None
        }
    }

    /// Push a request on the tail of the list.
    pub fn push_tail(&mut self, rq: Owned<IdleRequest<T>>) {
        let ptr = rq.as_raw();
        core::mem::forget(rq);
        // INVARIANT: rq is an `IdleRequest<T>`.
        // SAFETY: By type invariant, `self.inner` is a valid list.
        unsafe { bindings::rq_list_add_tail(self.inner.get(), ptr) };
    }

    /// Peek at the head of the list.
    ///
    /// Returns a null pointer if the list is empty.
    pub fn peek_raw(&self) -> *mut bindings::request {
        // SAFETY: By type invariant, `self.inner` is a valid list.
        unsafe { bindings::rq_list_peek(self.inner.get()) }
    }
}

impl<T: Operations> Default for RequestList<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Operations> Drop for RequestList<T> {
    fn drop(&mut self) {
        while let Some(rq) = self.pop() {
            drop(rq)
        }
    }
}

impl<T: Operations> Iterator for &mut RequestList<T> {
    type Item = Owned<IdleRequest<T>>;

    fn next(&mut self) -> Option<Self::Item> {
        self.pop()
    }
}

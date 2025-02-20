// SPDX-License-Identifier: GPL-2.0

//! This module provides a wrapper for the C `struct request` type.
//!
//! C header: [`include/linux/blk-mq.h`](srctree/include/linux/blk-mq.h)

use crate::{
    bindings,
    block::mq::Operations,
    sync::Refcount,
    types::{ARef, AlwaysRefCounted, Opaque, URef, UniqueRefCounted},
};
use core::{
    marker::PhantomData,
    ptr::{addr_of_mut, NonNull},
    sync::atomic::Ordering,
};

/// A wrapper around a blk-mq [`struct request`]. This represents an IO request.
///
/// # Implementation details
///
/// There are tree states for a request that the Rust bindings care about:
///
/// 1. Request is owned by block layer (refcount 0) or a [`URef`] is referencing the request.
/// 2. Request is owned by driver but with no [`ARef`] or [`URef`] referencing
///    the request (refcount 1).
/// 3. Request is owned by driver with exactly one [`URef`] referencing the request
///    (refcount >= 2).
///
/// We need to track 1 and 2 to make sure that `tag_to_rq` does not issue any
/// [`ARef`] to requests not owned by the driver, or to requests that have a
/// [`URef`] referencing it.
///
/// We need to track 3 to know when it is safe to convert an [`ARef`] to a
/// [`URef`].
///
/// Note that driver can still obtain new `ARef` even if there is no `ARef`s in existence by using
/// `tag_to_rq`, hence the need to distinct B and C.
///
/// The states are tracked through the private `refcount` field of
/// `RequestDataWrapper`. This structure lives in the private data area of the C
/// [`struct request`].
///
/// # Invariants
///
/// * `self.0` is a valid [`struct request`] created by the C portion of the
///   kernel.
/// * The private data area associated with this request must be an initialized
///   and valid `RequestDataWrapper<T>`.
/// * `self` is reference counted by atomic modification of
///   `self.wrapper_ref().refcount()`.
///
/// [`struct request`]: srctree/include/linux/blk-mq.h
///
#[repr(transparent)]
pub struct Request<T>(Opaque<bindings::request>, PhantomData<T>);

impl<T: Operations> Request<T> {
    /// Create an [`ARef<Request>`] from a [`struct request`] pointer.
    ///
    /// # Safety
    ///
    /// * The caller must own a refcount on `ptr` that is transferred to the
    ///   returned [`ARef`].
    /// * The refcount must be >= 2.
    /// * The type invariants for [`Request`] must hold for the pointee of `ptr`.
    ///
    /// [`struct request`]: srctree/include/linux/blk-mq.h
    #[expect(dead_code)]
    pub(crate) unsafe fn aref_from_raw(ptr: *mut bindings::request) -> ARef<Self> {
        // INVARIANT: By the safety requirements of this function, invariants are upheld.
        // SAFETY: By the safety requirement of this function, we own a
        // reference count that we can pass to `ARef`.
        unsafe { ARef::from_raw(NonNull::new_unchecked(ptr as *const Self as *mut Self)) }
    }

    /// Return a pointer to the [`RequestDataWrapper`] stored in the private area
    /// of the request structure.
    ///
    /// # Safety
    ///
    /// - `this` must point to a valid allocation of size at least size of
    ///   [`Self`] plus size of [`RequestDataWrapper`].
    pub(crate) unsafe fn wrapper_ptr(this: *mut Self) -> NonNull<RequestDataWrapper> {
        let request_ptr = this.cast::<bindings::request>();
        // SAFETY: By safety requirements for this function, `this` is a
        // valid allocation.
        let wrapper_ptr =
            unsafe { bindings::blk_mq_rq_to_pdu(request_ptr).cast::<RequestDataWrapper>() };
        // SAFETY: By C API contract, wrapper_ptr points to a valid allocation
        // and is not null.
        unsafe { NonNull::new_unchecked(wrapper_ptr) }
    }

    /// Return a reference to the [`RequestDataWrapper`] stored in the private
    /// area of the request structure.
    pub(crate) fn wrapper_ref(&self) -> &RequestDataWrapper {
        // SAFETY: By type invariant, `self.0` is a valid allocation. Further,
        // the private data associated with this request is initialized and
        // valid. The existence of `&self` guarantees that the private data is
        // valid as a shared reference.
        unsafe { Self::wrapper_ptr(self as *const Self as *mut Self).as_ref() }
    }
}

/// A wrapper around data stored in the private area of the C [`struct request`].
///
/// [`struct request`]: srctree/include/linux/blk-mq.h
pub(crate) struct RequestDataWrapper {
    /// The Rust request refcount has the following states:
    ///
    /// - 0: The request is owned by C block layer or is uniquely referenced
    /// - 1: The request is owned by Rust abstractions but is not referenced.
    /// - 2+: There is one or more [`ARef`] instances referencing the request.
    refcount: Refcount,
}

impl RequestDataWrapper {
    /// Return a reference to the refcount of the request that is embedding
    /// `self`.
    pub(crate) fn refcount(&self) -> &Refcount {
        &self.refcount
    }

    /// Return a pointer to the refcount of the request that is embedding the
    /// pointee of `this`.
    ///
    /// # Safety
    ///
    /// - `this` must point to a live allocation of at least the size of `Self`.
    pub(crate) unsafe fn refcount_ptr(this: *mut Self) -> *mut Refcount {
        // SAFETY: Because of the safety requirements of this function, the
        // field projection is safe.
        unsafe { addr_of_mut!((*this).refcount) }
    }
}

// SAFETY: Exclusive access is thread-safe for `Request`. `Request` has no `&mut
// self` methods and `&self` methods that mutate `self` are internally
// synchronized.
unsafe impl<T: Operations> Send for Request<T> {}

// SAFETY: Shared access is thread-safe for `Request`. `&self` methods that
// mutate `self` are internally synchronized`
unsafe impl<T: Operations> Sync for Request<T> {}

// SAFETY: All instances of `Request<T>` are reference counted. This
// implementation of `AlwaysRefCounted` ensure that increments to the ref count
// keeps the object alive in memory at least until a matching reference count
// decrement is executed.
unsafe impl<T: Operations> AlwaysRefCounted for Request<T> {
    fn inc_ref(&self) {
        let refcount = &self.wrapper_ref().refcount().as_atomic();

        // Load acquire, store relaxed. We sync with store release of `UniqueRequestRef::into_aref`.
        // After that all unique references are dead and we have shared access. We can use relaxed
        // ordering for the store.
        #[cfg_attr(not(CONFIG_DEBUG_MISC), allow(unused_variables))]
        let old = refcount.fetch_add(1, Ordering::Acquire);

        #[cfg(CONFIG_DEBUG_MISC)]
        if old <= 1 {
            panic!("Request refcount zero or one on clone\n");
        }
    }

    unsafe fn dec_ref(obj: core::ptr::NonNull<Self>) {
        // SAFETY: The type invariants of `ARef` guarantee that `obj` is valid
        // for read.
        let wrapper_ptr = unsafe { Self::wrapper_ptr(obj.as_ptr()).as_ptr() };
        // SAFETY: The type invariant of `Request` guarantees that the private
        // data area is initialized and valid.
        let refcount = unsafe { &*RequestDataWrapper::refcount_ptr(wrapper_ptr) };

        // Store release ordering to sync with acquire load in
        // `UniqueRequestRef::try_into_unique`.
        #[cfg_attr(not(CONFIG_DEBUG_MISC), allow(unused_variables))]
        let old = refcount.as_atomic().fetch_sub(1, Ordering::Release);

        #[cfg(CONFIG_DEBUG_MISC)]
        if old == 1 {
            panic!("Request reached refcount zero in Rust abstractions\n");
        }
    }
}

impl<T: Operations> URef<Request<T>> {
    /// Notify the block layer that a request is going to be processed now.
    ///
    /// The block layer uses this hook to do proper initializations such as
    /// starting the timeout timer. It is a requirement that block device
    /// drivers call this function when starting to process a request.
    ///
    /// # Safety
    ///
    /// The caller must have exclusive ownership of `self`, that is
    /// `self.wrapper_ref().refcount() == 2`.
    pub(crate) unsafe fn start_unchecked(&mut self) {
        // SAFETY: By type invariant, `self.0` is a valid `struct request` and
        // we have exclusive access.
        unsafe { bindings::blk_mq_start_request(self.0.get()) };
    }

    /// Notify the block layer that the request has been completed without errors.
    ///
    /// This function will return [`Err`] if `this` is not the only [`ARef`]
    /// referencing the request.
    pub fn end_ok(self) {
        let request_ptr = self.0.get().cast();
        core::mem::forget(self);

        // SAFETY: By type invariant, `this.0` was a valid `struct request`. The
        // success of the call to `try_set_end` guarantees that there are no
        // `ARef`s pointing to this request. Therefore it is safe to hand it
        // back to the block layer.
        unsafe { bindings::blk_mq_end_request(request_ptr, bindings::BLK_STS_OK as _) };
    }
}

unsafe impl<T: Operations> UniqueRefCounted for Request<T> {
    fn try_shared_to_unique(this: ARef<Self>) -> core::result::Result<URef<Self>, ARef<Self>> {
        // Load acquire to sync with decrement store release to make sure all
        // shared access has ended.
        let updated = this.wrapper_ref().refcount().as_atomic().compare_exchange(
            2,
            0,
            Ordering::Acquire,
            Ordering::Relaxed,
        );

        match updated {
            Ok(_) => Ok(
                // SAFETY: We achieved unique ownership above.
                unsafe { URef::from_raw(ARef::into_raw(this)) },
            ),
            Err(_) => Err(this),
        }
    }

    fn unique_to_shared(this: URef<Self>) -> ARef<Self> {
        // Store release to sync with future increments using load acquire to
        // make sure exclusive access has ended before shared access start.
        #[cfg_attr(not(CONFIG_DEBUG_MISC), allow(unused_variables))]
        let old = this
            .wrapper_ref()
            .refcount()
            .as_atomic()
            .fetch_add(2, Ordering::Release);

        #[cfg(CONFIG_DEBUG_MISC)]
        if old != 0 {
            panic!("Invalid refcount when upgrading `URef<Request<T>>`\n");
        }

        // SAFETY: We incremented the refcount above.
        unsafe { ARef::from_raw(URef::into_raw(this)) }
    }

    unsafe fn dec_ref(obj: core::ptr::NonNull<Self>) {
        // SAFETY: The type invariants of `URef` guarantee that `obj` is valid
        // for read.
        let wrapper_ptr = unsafe { Self::wrapper_ptr(obj.as_ptr()).as_ptr() };
        // SAFETY: The type invariant of `Request` guarantees that the private
        // data area is initialized and valid.
        let refcount = unsafe { &*RequestDataWrapper::refcount_ptr(wrapper_ptr) };

        // Store release ordering to sync with acquire load in
        // `TagSet::tag_to_rq()`.
        #[cfg_attr(not(CONFIG_DEBUG_MISC), allow(unused_variables))]
        let old = refcount.as_atomic().swap(1, Ordering::Release);

        #[cfg(CONFIG_DEBUG_MISC)]
        if old != 0 {
            panic!("Invalid refcount when dropping `URef<Request<T>>`\n");
        }
    }
}

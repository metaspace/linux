// SPDX-License-Identifier: GPL-2.0

//! This module provides a wrapper for the C `struct request` type.
//!
//! C header: [`include/linux/blk-mq.h`](srctree/include/linux/blk-mq.h)

use crate::{
    bindings,
    block::mq::Operations,
    sync::{
        aref::{ARef, RefCounted},
        Refcount,
    },
    time::hrtimer::{
        HasHrTimer, HrTimer, HrTimerCallback, HrTimerHandle, HrTimerMode, HrTimerPointer,
    },
    types::{Opaque, Ownable, OwnableRefCounted, Owned},
};
use core::{ffi::c_void, marker::PhantomData, ptr::NonNull, sync::atomic::Ordering};

/// A wrapper around a blk-mq [`struct request`]. This represents an IO request.
///
/// # Implementation details
///
/// There are three states for a request that the Rust bindings care about:
///
/// - 0: The request is owned by C block layer or is uniquely referenced (by [`Owned<_>`]).
/// - 1: The request is owned by Rust abstractions but is not referenced.
/// - 2+: There is one or more [`ARef`] instances referencing the request.
///
/// We need to track 1 and 2 to make sure that `tag_to_rq` does not issue any
/// [`ARef`] to requests not owned by the driver, or to requests that have a
/// [`Owned`] referencing it.
///
/// We need to track 3 to know when it is safe to convert an [`ARef`] to a
/// [`Owned`].
///
/// Note that the driver can still obtain new `ARef` even if there is no `ARef`s in existence by using
/// `tag_to_rq`, hence the need to distinct 1 and 2.
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
    pub(crate) unsafe fn aref_from_raw(ptr: *mut bindings::request) -> ARef<Self> {
        // INVARIANT: By the safety requirements of this function, invariants are upheld.
        // SAFETY: By the safety requirement of this function, we own a
        // reference count that we can pass to `ARef`.
        unsafe { ARef::from_raw(NonNull::new_unchecked(ptr.cast())) }
    }

    /// Complete the request by scheduling `Operations::complete` for
    /// execution.
    ///
    /// The function may be scheduled locally, via SoftIRQ or remotely via IPMI.
    /// See `blk_mq_complete_request_remote` in [`blk-mq.c`] for details.
    ///
    /// [`blk-mq.c`]: srctree/block/blk-mq.c
    pub fn complete(this: ARef<Self>) {
        let ptr = ARef::into_raw(this).cast::<bindings::request>().as_ptr();
        // SAFETY: By type invariant, `self.0` is a valid `struct request`
        if !unsafe { bindings::blk_mq_complete_request_remote(ptr) } {
            // SAFETY: We released a refcount above that we can reclaim here.
            let this = unsafe { Request::aref_from_raw(ptr) };
            T::complete(this);
        }
    }

    /// Return a pointer to the [`RequestDataWrapper`] stored in the private area
    /// of the request structure.
    ///
    /// # Safety
    ///
    /// - `this` must point to a valid allocation of size at least size of
    ///   [`Self`] plus size of [`RequestDataWrapper`].
    pub(crate) unsafe fn wrapper_ptr(this: *mut Self) -> NonNull<RequestDataWrapper<T>> {
        let request_ptr = this.cast::<bindings::request>();
        // SAFETY: By safety requirements for this function, `this` is a
        // valid allocation.
        let wrapper_ptr =
            unsafe { bindings::blk_mq_rq_to_pdu(request_ptr).cast::<RequestDataWrapper<T>>() };
        // SAFETY: By C API contract, `wrapper_ptr` points to a valid allocation
        // and is not null.
        unsafe { NonNull::new_unchecked(wrapper_ptr) }
    }

    /// Return a reference to the [`RequestDataWrapper`] stored in the private
    /// area of the request structure.
    pub(crate) fn wrapper_ref(&self) -> &RequestDataWrapper<T> {
        // SAFETY: By type invariant, `self.0` is a valid allocation. Further,
        // the private data associated with this request is initialized and
        // valid. The existence of `&self` guarantees that the private data is
        // valid as a shared reference.
        unsafe { Self::wrapper_ptr(core::ptr::from_ref(self).cast_mut()).as_ref() }
    }

    /// Return a reference to the per-request data associated with this request.
    pub fn data_ref(&self) -> &T::RequestData {
        &self.wrapper_ref().data
    }
}

/// A wrapper around data stored in the private area of the C [`struct request`].
///
/// [`struct request`]: srctree/include/linux/blk-mq.h
pub(crate) struct RequestDataWrapper<T: Operations> {
    /// The Rust request refcount has the following states:
    ///
    /// - 0: The request is owned by C block layer.
    /// - 1: The request is owned by Rust abstractions but there are no [`ARef`] references to it.
    /// - 2+: There are [`ARef`] references to the request.
    refcount: Refcount,

    /// Driver managed request data
    data: T::RequestData,
}

impl<T: Operations> RequestDataWrapper<T> {
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
        unsafe { &raw mut (*this).refcount }
    }

    /// Return a pointer to the `data` field of the `Self` pointed to by `this`.
    ///
    /// # Safety
    ///
    /// - `this` must point to a live allocation of at least the size of `Self`.
    pub(crate) unsafe fn data_ptr(this: *mut Self) -> *mut T::RequestData {
        // SAFETY: Because of the safety requirements of this function, the
        // field projection is safe.
        unsafe { &raw mut (*this).data }
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
// implementation of `RefCounted` ensure that increments to the ref count keeps
// the object alive in memory at least until a matching reference count
// decrement is executed.
unsafe impl<T: Operations> RefCounted for Request<T> {
    fn inc_ref(&self) {
        let refcount = &self.wrapper_ref().refcount().as_atomic();

        // Load acquire, store relaxed. We sync with store release of `OwnableRefCounted::into_shared`.
        // After that all unique references are dead and we have shared access. We can use relaxed
        // ordering for the store.
        #[cfg_attr(not(debug_assertions), allow(unused_variables))]
        let old = refcount.fetch_add(1, Ordering::Acquire);

        debug_assert!(old > 1, "Request refcount zero or one on clone");
    }

    unsafe fn dec_ref(obj: core::ptr::NonNull<Self>) {
        // SAFETY: The type invariants of `RefCounted` guarantee that `obj` is valid
        // for read.
        let wrapper_ptr = unsafe { Self::wrapper_ptr(obj.as_ptr()).as_ptr() };
        // SAFETY: The type invariant of `Request` guarantees that the private
        // data area is initialized and valid.
        let refcount = unsafe { &*RequestDataWrapper::refcount_ptr(wrapper_ptr) };

        // Store release to sync with load acquire in
        // `OwnableRefCounted::try_from_shared`.
        #[cfg_attr(not(debug_assertions), allow(unused_variables))]
        let old = refcount.as_atomic().fetch_sub(1, Ordering::Release);

        debug_assert!(
            old > 1,
            "Request reached refcount zero in Rust abstractions"
        );
    }
}

impl<T: Operations> Owned<Request<T>> {
    /// Notify the block layer that a request is going to be processed now.
    ///
    /// The block layer uses this hook to do proper initializations such as
    /// starting the timeout timer. It is a requirement that block device
    /// drivers call this function when starting to process a request.
    ///
    /// # Safety
    ///
    /// The caller must have exclusive ownership of `self`, that is
    /// `self.wrapper_ref().refcount() == 0`.
    ///
    /// This can only be called once in the request life cycle.
    pub(crate) unsafe fn start_unchecked(&mut self) {
        // SAFETY: By type invariant, `self.0` is a valid `struct request` and
        // we have exclusive access.
        unsafe { bindings::blk_mq_start_request(self.0.get()) };
    }

    /// Notify the block layer that the request has been completed without errors.
    pub fn end_ok(self) {
        let request_ptr = self.0.get().cast();
        core::mem::forget(self);
        // SAFETY: By type invariant, `this.0` was a valid `struct request`. The
        // existence of `self` guarantees that there are no `ARef`s pointing to
        // this request. Therefore it is safe to hand it back to the block
        // layer.
        unsafe { bindings::blk_mq_end_request(request_ptr, bindings::BLK_STS_OK as _) };
    }
}

unsafe impl<T: Operations> Ownable for Request<T> {
    unsafe fn release(this: NonNull<Self>) {
        // SAFETY: The safety requirements of this function guarantee that `this`
        // is valid for read.
        let wrapper_ptr = unsafe { Self::wrapper_ptr(this.as_ptr()).as_ptr() };
        // SAFETY: The type invariant of `Request` guarantees that the private
        // data area is initialized and valid.
        let refcount = unsafe { &*RequestDataWrapper::refcount_ptr(wrapper_ptr) };

        // Store release to sync with load acquire when converting back to owned.
        #[cfg_attr(not(debug_assertions), allow(unused_variables))]
        let old = refcount.as_atomic().fetch_add(1, Ordering::Release);

        debug_assert!(
            old == 0,
            "Invalid refcount when releasing `Owned<Request<T>>`"
        );
    }
}

unsafe impl<T: Operations> OwnableRefCounted for Request<T> {
    fn try_from_shared(this: ARef<Self>) -> core::result::Result<Owned<Self>, ARef<Self>> {
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
                unsafe { Owned::from_raw(ARef::into_raw(this)) },
            ),
            Err(_) => Err(this),
        }
    }

    fn into_shared(this: Owned<Self>) -> ARef<Self> {
        // Store release to sync with future increments using load acquire to
        // make sure exclusive access has ended before shared access start.
        #[cfg_attr(not(debug_assertions), allow(unused_variables))]
        let old = this
            .wrapper_ref()
            .refcount()
            .as_atomic()
            .fetch_add(2, Ordering::Release);

        debug_assert!(
            old == 0,
            "Invalid refcount when upgrading `Owned<Request<T>>`"
        );

        // SAFETY: We incremented the refcount above.
        unsafe { ARef::from_raw(Owned::into_raw(this)) }
    }
}

/// A handle for a timer that is embedded in a [`Request`] private data area.
pub struct RequestTimerHandle<T>
where
    T: Operations,
    T::RequestData: HasHrTimer<T::RequestData>,
{
    inner: ARef<Request<T>>,
}

unsafe impl<T> HrTimerHandle for RequestTimerHandle<T>
where
    T: Operations,
    T::RequestData: HasHrTimer<T::RequestData>,
{
    fn cancel(&mut self) -> bool {
        let request_data_ptr = &self.inner.wrapper_ref().data as *const T::RequestData;

        // SAFETY: As we obtained `self_ptr` from a valid reference above, it
        // must point to a valid `U`.
        let timer_ptr = unsafe {
            <T::RequestData as HasHrTimer<T::RequestData>>::raw_get_timer(request_data_ptr)
        };

        // SAFETY: As `timer_ptr` points into `U` and `U` is valid, `timer_ptr`
        // must point to a valid `HrTimer` instance.
        unsafe { HrTimer::<T::RequestData>::raw_cancel(timer_ptr) }
    }
}

impl<T> RequestTimerHandle<T>
where
    T: Operations,
    T::RequestData: HasHrTimer<T::RequestData>,
{
    /// Drop the timer handle without cancelling the timer.
    ///
    /// This is safe because [`Request`] is not dropped during normal operations.
    pub fn dismiss(mut self) {
        unsafe { core::ptr::drop_in_place(&mut self.inner as *mut ARef<Request<T>>) };
        core::mem::forget(self);
    }
}

impl<T> Drop for RequestTimerHandle<T>
where
    T: Operations,
    T::RequestData: HasHrTimer<T::RequestData>,
{
    fn drop(&mut self) {
        self.cancel();
    }
}

impl<T> HrTimerPointer for ARef<Request<T>>
where
    T: Operations,
    T::RequestData: HasHrTimer<T::RequestData>,
    T::RequestData: Sync,
{
    type TimerMode = <T::RequestData as HasHrTimer<T::RequestData>>::TimerMode;
    type TimerHandle = RequestTimerHandle<T>;

    fn start(self, expires: <Self::TimerMode as HrTimerMode>::Expires) -> RequestTimerHandle<T> {
        let pdu_ptr = self.data_ref() as *const T::RequestData;

        unsafe { T::RequestData::start(pdu_ptr, expires) };

        RequestTimerHandle { inner: self }
    }
}

impl<T> kernel::time::hrtimer::RawHrTimerCallback for ARef<Request<T>>
where
    T: Operations,
    T::RequestData: HasHrTimer<T::RequestData>,
    T::RequestData: for<'a> HrTimerCallback<Pointer<'a> = ARef<Request<T>>>,
    T::RequestData: Sync,
{
    type CallbackTarget<'a> = Self;

    unsafe extern "C" fn run(ptr: *mut bindings::hrtimer) -> bindings::hrtimer_restart {
        // `HrTimer` is `repr(transparent)`
        let timer_ptr = ptr.cast::<kernel::time::hrtimer::HrTimer<T::RequestData>>();

        // SAFETY: By C API contract `ptr` is the pointer we passed when
        // enqueing the timer, so it is a `HrTimer<T::RequestData>` embedded in a `T::RequestData`
        let request_data_ptr = unsafe { T::RequestData::timer_container_of(timer_ptr) };

        let offset = core::mem::offset_of!(RequestDataWrapper<T>, data);

        // SAFETY: This sub stays withing the `bindings::request` allocation and does not wrap
        let pdu_ptr = unsafe {
            request_data_ptr
                .cast::<u8>()
                .sub(offset)
                .cast::<RequestDataWrapper<T>>()
        };

        // SAFETY: This request pointer was passed to us by the kernel in `init_request_callback`.
        let request_ptr = unsafe { bindings::blk_mq_rq_from_pdu(pdu_ptr.cast::<c_void>()) };

        // SAFETY: By C API contract, we have ownership of the request.
        let request_ref = unsafe { &*(request_ptr as *const Request<T>) };

        request_ref.inc_ref();
        // SAFETY: We just incremented the refcount above.
        let aref: ARef<Request<T>> = unsafe { ARef::from_raw(NonNull::from(request_ref)) };

        T::RequestData::run(aref).into_c()
    }
}

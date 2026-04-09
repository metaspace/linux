// SPDX-License-Identifier: GPL-2.0

//! This module provides a wrapper for the C `struct request` type.
//!
//! C header: [`include/linux/blk-mq.h`](srctree/include/linux/blk-mq.h)

use crate::{
    bindings,
    block::{error::BlkResult, mq::Operations},
    device::Device,
    dma::DataDirection,
    prelude::*,
    sync::{
        aref::{ARef, RefCounted},
        atomic::ordering,
        Refcount,
    },
    time::hrtimer::{
        HasHrTimer, HrTimer, HrTimerCallback, HrTimerHandle, HrTimerMode, HrTimerPointer,
    },
    types::{ForeignOwnable, Opaque, Ownable, OwnableRefCounted, Owned},
};
use core::{ffi::c_void, marker::PhantomData, ops::Deref, pin::Pin, ptr::NonNull};

use super::{
    dma_map_iter::{DmaMapIter, DmaMapMempool},
    RequestQueue,
};
use crate::block::bio::Bio;
use crate::block::bio::BioIterator;

mod command;
pub use command::Command;

mod flag;
pub use flag::{Flag, Flags};

/// A [`Request`] that a driver has not yet begun to process.
///
/// A driver can convert an `IdleRequest` to a [`Request`] by calling [`IdleRequest::start`].
///
/// # Invariants
///
/// - This request has not been started yet.
#[repr(transparent)]
pub struct IdleRequest<T>(RequestInner<T>);

impl<T: Operations> IdleRequest<T> {
    /// Mark the request as processing.
    ///
    /// This converts the [`IdleRequest`] into a [`Request`].
    pub fn start(self: Owned<Self>) -> Owned<Request<T>> {
        // SAFETY: By type invariant `self.0.0` is a valid request. Because we have an `Owned<_>`,
        // the refcount is zero.
        let mut request = unsafe { Request::from_raw(self.0 .0.get()) };

        debug_assert!(
            request
                .wrapper_ref()
                .refcount()
                .as_atomic()
                .load(ordering::Acquire)
                == 0
        );

        // SAFETY: We have exclusive access and the refcount is 0. By type invariant `request` was
        // not started yet.
        unsafe { request.start_unchecked() };

        request
    }

    /// Create a [`Self`] from a raw request pointer.
    ///
    /// # Safety
    ///
    /// - The request pointed to by `ptr` must satisfythe invariants of both [`Request`] and
    ///   [`Self`].
    /// - The refcount of the request pointed to by `ptr` must be 0.
    pub(crate) unsafe fn from_raw(ptr: *mut bindings::request) -> Owned<Self> {
        // SAFETY: By function safety requirements, `ptr` is valid for use as an `IdleRequest`.
        unsafe { Owned::from_raw(NonNull::<Self>::new_unchecked(ptr.cast())) }
    }

    /// Requeue this request at the block layer.
    ///
    /// If `kick_requeue_list` is true, this method will schedule processing of
    /// the requeue list on a workqueue.
    pub fn requeue(self: Owned<Self>, kick_requeue_list: bool) {
        let ptr = self.0 .0.get();
        core::mem::forget(self);

        // SAFETY: By type invariant, the wrapped request is valid.
        unsafe { bindings::blk_mq_requeue_request(ptr, kick_requeue_list) };
    }
}

// SAFETY: The `release` implementation leaks the `IdleRequest`, which is a valid state for a
// [`Request`] with refcount 0.
unsafe impl<T: Operations> Ownable for IdleRequest<T> {
    unsafe fn release(_this: NonNull<Self>) {}
}

impl<T: Operations> Deref for IdleRequest<T> {
    type Target = RequestInner<T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub struct RequestInner<T>(Opaque<bindings::request>, PhantomData<T>);

impl<T: Operations> RequestInner<T> {
    /// Get the command identifier for the request
    fn command_raw(&self) -> u32 {
        // SAFETY: By C API contract and type invariant, `cmd_flags` is valid for read
        unsafe { (*self.0.get()).cmd_flags & ((1 << bindings::REQ_OP_BITS) - 1) }
    }

    fn is_write(&self) -> bool {
        self.command_raw() & 1 != 0
    }

    /// Get the command of this request.
    pub fn command(&self) -> Command {
        // SAFETY: By type invariant of `Self`, `self.0` is valid and live.
        unsafe { Command::from_raw(self.command_raw()) }
    }

    pub fn flags(&self) -> Flags {
        // SAFETY: By C API contract and type invariant, `cmd_flags` is valid for read
        let flags = unsafe { (*self.0.get()).cmd_flags & !((1 << bindings::REQ_OP_BITS) - 1) };
        Flags::try_from(flags).expect("Request should have valid falgs")
    }

    /// Get the target sector for the request.
    #[inline(always)]
    pub fn sector(&self) -> u64 {
        // SAFETY: By type invariant of `Self`, `self.0` is valid and live.
        unsafe { (*self.0.get()).__sector }
    }

    /// Get the size of the request in number of sectors.
    #[inline(always)]
    pub fn sectors(&self) -> u32 {
        self.bytes() >> crate::block::SECTOR_SHIFT
    }

    /// Get the size of the request in bytes.
    #[inline(always)]
    pub fn bytes(&self) -> u32 {
        // SAFETY: By type invariant of `Self`, `self.0` is valid and live.
        unsafe { (*self.0.get()).__data_len }
    }

    /// Borrow the queue data from the request queue associated with this request.
    pub fn queue_data(&self) -> <T::QueueData as ForeignOwnable>::Borrowed<'_> {
        // SAFETY: By type invariants of `Request`, `self.0` is a valid request.
        unsafe { T::QueueData::borrow((*(*self.0.get()).q).queuedata) }
    }

    /// Get the request queue associated with this request.
    pub fn queue(&self) -> &RequestQueue<T> {
        // SAFETY: By type invariant, self.0 is guaranteed to be valid.
        unsafe { RequestQueue::from_raw((*self.0.get()).q) }
    }

    /// Return a raw pointer to the underlying C structure.
    pub fn as_raw(&self) -> *mut bindings::request {
        self.0.get()
    }

    // Return a valid hctx pointer.
    fn hctx_raw(&self) -> *mut bindings::blk_mq_hw_ctx {
        // SAFETY: The requests is guaranteed to be associated with a hardware
        // context while we have access to it.
        unsafe { (*self.0.get()).mq_hctx }
    }

    /// Get a reference to the [`T::HwData`] for the hardware context that this
    /// request is associated with.
    pub fn hw_data(&self) -> <T::HwData as ForeignOwnable>::Borrowed<'_> {
        let hctx = self.hctx_raw();

        // SAFETY: `hctx` is valid and `driver_data` was produced by a call to
        // `into_foreign` in `Operations::init_hctx_callback`.
        unsafe { T::HwData::borrow((*hctx).driver_data) }
    }

    /// Get the queue index for the hardware queue associated with this request.
    pub fn queue_index(&self) -> u32 {
        // SAFETY: The requests is guaranteed to be associated with a hardware
        // context while we have access to it.
        unsafe { (*self.hctx_raw()).queue_num }
    }

    pub fn is_poll(&self) -> bool {
        let hctx = self.hctx_raw();

        u32::from(
            // SAFETY: `hctx_raw` returns a valid pointer.
            unsafe { (*hctx).type_ },
        ) == bindings::hctx_type_HCTX_TYPE_POLL
    }
}

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
/// Note that the driver can still obtain new `ARef` even if there is no `ARef`s in existence by
/// using `tag_to_rq`, hence the need to distinct 1 and 2.
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
pub struct Request<T>(RequestInner<T>);

impl<T: Operations> Deref for Request<T> {
    type Target = RequestInner<T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T: Operations> Request<T> {
    /// Create a `Owned<Request>` from a request pointer.
    ///
    /// # Safety
    ///
    /// - `ptr` must satisfy invariants of `Request`.
    /// - The refcount of the request pointed to by `ptr` must be 0.
    pub(crate) unsafe fn from_raw(ptr: *mut bindings::request) -> Owned<Self> {
        // SAFETY: By function safety requirements, `ptr` is valid for use as `Owned<Request>`.
        unsafe { Owned::from_raw(NonNull::<Self>::new_unchecked(ptr.cast())) }
    }

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

    /// Get a reference to the first [`Bio`] in this request.
    #[inline(always)]
    pub fn bio(&self) -> Option<&Bio> {
        // SAFETY: By type invariant of `Self`, `self.0` is valid and the deref
        // is safe.
        let ptr = unsafe { (*self.0 .0.get()).bio };
        // SAFETY: By C API contract, if `bio` is not null it will have a
        // positive refcount at least for the duration of the lifetime of
        // `&self`.
        unsafe { Bio::from_raw(ptr) }
    }

    /// Get a mutable reference to the first [`Bio`] in this request.
    #[inline(always)]
    pub fn bio_mut(&mut self) -> Option<&mut Bio> {
        // SAFETY: By type invariant of `Self`, `self.0` is valid and the deref
        // is safe.
        let ptr = unsafe { (*self.0 .0.get()).bio };
        // SAFETY: By C API contract, if `bio` is not null it will have a
        // positive refcount at least for the duration of the lifetime of
        // `&self`.
        unsafe { Bio::from_raw_mut(ptr) }
    }

    /// Get an iterator over all bio structurs in this request.
    #[inline(always)]
    pub fn bio_iter_mut<'a>(self: &'a mut Owned<Self>) -> BioIterator<'a> {
        // INVARIANT: By C API contract, if the bio pointer is not null, it is a valid `struct bio`.
        // `NonNull::new` will return `None` if the pointer is null.
        BioIterator {
            // SAFETY: By type invariant `self.0` is a valid `struct request`.
            bio: NonNull::new(unsafe { (*self.0 .0.get()).bio.cast() }),
            _p: PhantomData,
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

    /// Set the target sector for the request.
    #[inline(always)]
    pub fn set_sector(self: Pin<&mut Self>, sector: u64) {
        // SAFETY: By type invariant of `Self`, `self.0` is valid and live.
        unsafe { (*self.0 .0.get()).__sector = sector }
    }

    /// Create a DMA mapping iterator for this request.
    pub fn dma_map_iter<const N: usize>(
        self: ARef<Self>,
        device: &Device,
        mempool: DmaMapMempool<N>,
    ) -> BlkResult<DmaMapIter<N, T>> {
        DmaMapIter::new(self, device, mempool)
    }

    /// Return the DMA data direction for this request.
    pub fn dma_direction(&self) -> kernel::dma::DataDirection {
        if self.is_write() {
            DataDirection::ToDevice
        } else {
            DataDirection::FromDevice
        }
    }
}

/// A synchronous request to be submitted to a queue.
#[repr(transparent)]
pub struct SyncRequest<T: Operations> {
    inner: RequestInner<T>,
}

impl<T: Operations> SyncRequest<T> {
    /// Create a `Owned<Self>` from a request pointer.
    ///
    /// # Safety
    ///
    /// - `ptr` must be represent an unsubmitted requiest allocated by
    ///   `RequestQueue::alloc_sync_request`.
    /// - Ownership of of `ptr` is transferred to this function.
    pub(crate) unsafe fn from_raw(ptr: *mut bindings::request) -> Owned<Self> {
        // SAFETY: By function safety requirements, `ptr` is valid for use as `Owned<Request>`.
        unsafe { Owned::from_raw(NonNull::<Self>::new_unchecked(ptr.cast())) }
    }

    /// Submits the request for execution by the request queue to which it belongs.
    pub fn execute(self: Pin<&mut Self>, at_head: bool) -> Result {
        let status = unsafe { bindings::blk_execute_rq(self.inner.as_raw(), at_head as _) };
        let ret = unsafe { bindings::blk_status_to_errno(status) };
        if ret < 0 {
            Err(Error::from_errno(ret))
        } else {
            Ok(())
        }
    }
}

impl<T: Operations> Drop for SyncRequest<T> {
    fn drop(&mut self) {}
}

impl<T: Operations> Deref for SyncRequest<T> {
    type Target = RequestInner<T>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

unsafe impl<T: Operations> Ownable for SyncRequest<T> {
    unsafe fn release(this: NonNull<Self>) {
        unsafe { bindings::blk_mq_free_request(this.as_ptr().cast()) };
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

// SAFETY: All instances of `Request<T>` are reference counted. This implementation of `RefCounted`
// ensure that increments to the ref count keeps the object alive in memory at least until a
// matching reference count decrement is executed.
unsafe impl<T: Operations> RefCounted for Request<T> {
    fn inc_ref(&self) {
        let refcount = &self.wrapper_ref().refcount().as_atomic();

        // Load acquire, store relaxed. We sync with store release of
        // `OwnableRefCounted::into_shared`. After that all unique references are dead and we have
        // shared access. We can use relaxed ordering for the store.
        #[cfg_attr(not(debug_assertions), allow(unused_variables))]
        let old = refcount.fetch_add(1, ordering::Acquire);

        debug_assert!(old >= 1, "Request refcount zero clone");
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
        let old = refcount.as_atomic().fetch_sub(1, ordering::Release);

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
    pub unsafe fn start_unchecked(&mut self) {
        // SAFETY: By type invariant, `self.0` is a valid `struct request` and
        // we have exclusive access.
        unsafe { bindings::blk_mq_start_request(self.0 .0.get()) };
    }

    /// Notify the block layer that the request has been completed without errors.
    pub fn end_ok(self) {
        self.end(bindings::BLK_STS_OK)
    }

    /// Notify the block layer that the request has been completed.
    pub fn end(self, status: u8) {
        let request_ptr = self.0 .0.get().cast();
        core::mem::forget(self);
        // SAFETY: By type invariant, `this.0` was a valid `struct request`. The
        // existence of `self` guarantees that there are no `ARef`s pointing to
        // this request. Therefore it is safe to hand it back to the block
        // layer.
        unsafe { bindings::blk_mq_end_request(request_ptr, status) };
    }
}

// SAFETY: The `release` implementation frees the underlying request according to the reference
// counting scheme for `Request`.
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
        let old = refcount.as_atomic().fetch_add(1, ordering::Release);

        debug_assert!(
            old == 0,
            "Invalid refcount when releasing `Owned<Request<T>>`"
        );
    }
}

impl<T: Operations> OwnableRefCounted for Request<T> {
    fn try_from_shared(this: ARef<Self>) -> core::result::Result<Owned<Self>, ARef<Self>> {
        // Load acquire to sync with decrement store release to make sure all
        // shared access has ended.
        let updated = this
            .wrapper_ref()
            .refcount()
            .as_atomic()
            .cmpxchg(2, 0, ordering::Acquire);

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
            .fetch_add(2, ordering::Release);

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

// SAFETY: The drop implementation of `RequestTimerHandle` calls `cancel`, which cancels the timer
// if it is running. `drop` will block if the timer handler is running. This is ensured via a call
// to `HrTimer::raw_cancel` in the implementation of `cancel`.
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
    /// This is safe because dropping the last [`ARef<Request>`] does not drop the [`Request`].
    pub fn dismiss(mut self) {
        let inner = core::ptr::from_mut(&mut self.inner);

        // SAFETY: `inner` is valid for reads and writes, is properly aligned and nonnull. We have
        // exclusive access to `inner` and we do not access `inner` after this call.
        unsafe { core::ptr::drop_in_place(inner) };

        debug_assert!(
            self.inner
                .wrapper_ref()
                .refcount()
                .as_atomic()
                .load(ordering::Relaxed)
                >= 2,
            "Request refcount must be at least two when an ARef<Request> exist"
        );

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

        // SAFETY: `pdu_pointer` is coerced from a live reference to a `T` and this points to a
        // valid `T`. The reference is valid until `T` is dropped, and the timer will be canceled
        // before this.
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
        // enqueuing the timer, so it is a `HrTimer<T::RequestData>` embedded in a `T::RequestData`
        let request_data_ptr = unsafe { T::RequestData::timer_container_of(timer_ptr) };

        let offset = core::mem::offset_of!(RequestDataWrapper<T>, data);

        // SAFETY: This sub stays within the `bindings::request` allocation and does not wrap.
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

        // SAFETY:
        // - By C API contract `timer_ptr` is the pointer that we passed when queuing the timer, so
        //   it is a valid pointer to a `HrTimer<T>` embedded in a `T`.
        // - We are within `RawHrTimerCallback::run`
        let context = unsafe { kernel::time::hrtimer::HrTimerCallbackContext::from_raw(timer_ptr) };

        T::RequestData::run(aref, context).into_c()
    }
}

mod private {
    /// Supertrait used to seal [`super::ConvertRaw`].
    ///
    /// External code (and even other modules in this crate) cannot name
    /// `private::Sealed`, so no new implementations of [`super::ConvertRaw`] can
    /// appear outside this module.
    pub trait Sealed {}
}

pub trait ConvertRaw: private::Sealed {
    /// Create a [`Self`] from a raw request pointer.
    ///
    /// # Safety
    ///
    /// - The request pointed to by `ptr` must satisfythe invariants of both [`Request`] and
    ///   [`Self`].
    /// - The refcount of the request pointed to by `ptr` must satisfy the safety requirements of
    ///   the request pointer type that `ConvertRaw` is implemented on.
    unsafe fn from_raw(ptr: *mut bindings::request) -> Self;

    /// Return a raw pointer to the underlying C structure.
    fn as_raw(&self) -> *mut bindings::request;
}

impl<T: Operations> private::Sealed for Owned<IdleRequest<T>> {}
impl<T: Operations> ConvertRaw for Owned<IdleRequest<T>> {
    unsafe fn from_raw(ptr: *mut bindings::request) -> Self {
        unsafe { IdleRequest::from_raw(ptr) }
    }

    fn as_raw(&self) -> *mut bindings::request {
        // Reach through Owned -> IdleRequest -> RequestInner to the raw pointer.
        // Calling `self.as_raw()` would resolve to this very method (infinite recursion).
        (**self).0 .0.get()
    }
}

impl<T: Operations> private::Sealed for ARef<Request<T>> {}
impl<T: Operations> ConvertRaw for ARef<Request<T>> {
    unsafe fn from_raw(ptr: *mut bindings::request) -> Self {
        unsafe { Request::aref_from_raw(ptr) }
    }

    fn as_raw(&self) -> *mut bindings::request {
        // Call the inherent `RequestInner::as_raw` directly to avoid resolving back to
        // the ConvertRaw trait method (which would be infinite recursion). Deref
        // coercion turns `&ARef<Request<T>>` into `&RequestInner<T>`.
        RequestInner::<T>::as_raw(self)
    }
}

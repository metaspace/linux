// SPDX-License-Identifier: GPL-2.0

//! This module provides the `TagSet` struct to wrap the C `struct blk_mq_tag_set`.
//!
//! C header: [`include/linux/blk-mq.h`](srctree/include/linux/blk-mq.h)

use crate::{
    block::mq::{
        operations::OperationsVTable, request::RequestDataWrapper, request::RequestInner,
        Operations,
    },
    error::{self, Result},
    pr_warn,
    prelude::{ENOMEM, *},
    sync::atomic::ordering,
    types::{ARef, ForeignOwnable, Opaque},
};
use core::{convert::TryInto, marker::PhantomData, pin::Pin};
use pin_init::{pin_data, pinned_drop, PinInit};

mod flags;
pub use flags::Flag;
pub use flags::Flags;

use super::Request;

/// A wrapper for the C `struct blk_mq_tag_set`.
///
/// `struct blk_mq_tag_set` contains a `struct list_head` and so must be pinned.
///
/// # Invariants
///
/// - `inner` is initialized and valid.
#[pin_data(PinnedDrop)]
#[repr(transparent)]
pub struct TagSet<T: Operations> {
    #[pin]
    inner: Opaque<bindings::blk_mq_tag_set>,
    _p: PhantomData<T>,
}

impl<T: Operations> TagSet<T> {
    /// Try to create a new tag se }t
    pub fn new(
        nr_hw_queues: u32,
        tagset_data: T::TagSetData,
        num_tags: u32,
        num_maps: u32,
        numa_node: i32,
        flags: Flags,
    ) -> impl PinInit<Self, error::Error> {
        // SAFETY: `blk_mq_tag_set` only contains integers and pointers, which
        // all are allowed to be 0.
        let tag_set: bindings::blk_mq_tag_set = unsafe { core::mem::zeroed() };
        let tag_set: Result<_> = core::mem::size_of::<RequestDataWrapper<T>>()
            .try_into()
            .map(|cmd_size| {
                bindings::blk_mq_tag_set {
                    ops: OperationsVTable::<T>::build(),
                    nr_hw_queues,
                    timeout: 0, // 0 means default which is 30Hz in C
                    numa_node,
                    queue_depth: num_tags,
                    cmd_size,
                    flags: flags.into_inner(),
                    driver_data: tagset_data.into_foreign(),
                    nr_maps: num_maps,
                    ..tag_set
                }
            })
            .map(Opaque::new)
            .map_err(|e| e.into());

        try_pin_init!(TagSet {
            inner <- tag_set.pin_chain(|tag_set| {
                // SAFETY: we do not move out of `tag_set`.
                let tag_set: &mut Opaque<_> = unsafe { Pin::get_unchecked_mut(tag_set) };
                // SAFETY: `tag_set` is a reference to an initialized `blk_mq_tag_set`.
                let status = error::to_result(
                    unsafe { bindings::blk_mq_alloc_tag_set(tag_set.get())}
                );
                if status.is_err() {
                    // SAFETY: We created `driver_data` above with `into_foreign`
                    unsafe { T::TagSetData::from_foreign((*tag_set.get()).driver_data) };
                }
                status
            }),
            _p: PhantomData,
        })
    }

    /// Return the pointer to the wrapped `struct blk_mq_tag_set`
    pub(crate) fn raw_tag_set(&self) -> *mut bindings::blk_mq_tag_set {
        self.inner.get()
    }

    /// Create a `TagSet<T>` from a raw pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must be a pointer to a valid and initialized `TagSet<T>`. There
    /// may be no other mutable references to the tag set. The pointee must be
    /// live and valid at least for the duration of the returned lifetime `'a`.
    pub(crate) unsafe fn from_ptr<'a>(ptr: *mut bindings::blk_mq_tag_set) -> &'a Self {
        // SAFETY: By the safety requirements of this function, `ptr` is valid
        // for use as a reference for the duration of `'a`.
        unsafe { &*(ptr.cast::<Self>()) }
    }

    /// Create a `TagSet<T>` from a raw pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must be a pointer to a valid and initialized `TagSet<T>`. There
    /// may be no other mutable references to the tag set. The pointee must be
    /// live and valid at least for the duration of the returned lifetime `'a`.
    pub(crate) unsafe fn from_ptr_mut<'a>(ptr: *mut bindings::blk_mq_tag_set) -> Pin<&'a mut Self> {
        // SAFETY: By function safety requirements, `ptr` is valid for use as a mutable reference.
        let mref = unsafe { &mut *(ptr.cast::<Self>()) };

        // SAFETY: We never move out of `mref`.
        unsafe { Pin::new_unchecked(mref) }
    }

    /// Helper function to invoke a closure each hardware queue type supported.
    ///
    /// This function invokes `cb` for each variant of [`QueueType`] that this [`TagSet`] supports.
    /// This is helpful for setting up CPU to hardware queue maps in the [`Operations::map_queues`]
    /// callback.
    pub fn update_maps(self: Pin<&mut Self>, mut cb: impl FnMut(QueueMap)) -> Result {
        // SAFETY: By type invariant, `self.inner` is valid.
        let nr_maps = unsafe { (*self.inner.get()).nr_maps };
        for i in 0..nr_maps {
            cb(QueueMap {
                // SAFETY: By type invariant, `self.inner` is valid.
                map: unsafe { &raw mut (*self.inner.get()).map[i as usize] },
                kind: i.try_into()?,
            });
        }

        Ok(())
    }

    /// Return the number of hardware queues for this tag set.
    pub fn hw_queue_count(&self) -> u32 {
        // SAFETY: By type invariant, `self.inner` is valid.
        unsafe { (*self.inner.get()).nr_hw_queues }
    }

    /// Update the number of hardware queues for this tag set.
    ///
    /// This operation may fail if memory for tags cannot be allocated.
    pub fn update_hw_queue_count(&self, nr_hw_queues: u32) -> Result {
        // SAFETY: blk_mq_update_nr_hw_queues applies internal synchronization.
        unsafe { bindings::blk_mq_update_nr_hw_queues(self.inner.get(), nr_hw_queues) }

        if self.hw_queue_count() == nr_hw_queues {
            Ok(())
        } else {
            Err(ENOMEM)
        }
    }

    /// Borrow the [`T::TagSetData`] associated with this tag set.
    pub fn data(&self) -> <T::TagSetData as ForeignOwnable>::Borrowed<'_> {
        // SAFETY: By type invariant, `self.inner` is valid.
        let ptr = unsafe { (*self.inner.get()).driver_data };

        // SAFETY: `ptr` was created by `into_foreign` during initialization and the target is not
        // converted back with `from_foreign` while `&self` is live.
        unsafe { T::TagSetData::borrow(ptr) }
    }

    /// Obtain a shared reference to a request.
    ///
    /// This method will hang if the request is not owned by the driver, or if
    /// the driver holds an [`Ownable<Request>`] reference to the request.
    pub fn tag_to_rq(&self, qid: u32, tag: u32) -> Option<ARef<Request<T>>> {
        if qid >= self.hw_queue_count() {
            // TODO: Use pr_warn_once!
            pr_warn!("Invalid queue id: {qid}\n");
            return None;
        }

        // SAFETY: We checked that `qid` is within bounds.
        let tags = unsafe { *(*self.inner.get()).tags.add(qid as usize) };

        // SAFETY: We checked `qid` for overflow above, so `tags` is valid.
        let rq_ptr = unsafe { bindings::blk_mq_tag_to_rq(tags, tag) };
        if rq_ptr.is_null() {
            None
        } else {
            // SAFETY: if `rq_ptr`is not null, it is a valid request pointer.
            let refcount_ptr = unsafe {
                RequestDataWrapper::refcount_ptr(
                    RequestInner::wrapper_ptr(rq_ptr.cast::<RequestInner<T>>()).as_ptr(),
                )
            };

            // SAFETY: The refcount was initialized in `init_request_callback` and is never
            // referenced mutably.
            let refcount_ref = unsafe { &*refcount_ptr };

            let atomic_ref = refcount_ref.as_atomic();

            // It is possible for an interrupt to arrive faster than the last
            // change to the refcount, so retry if the refcount is not what we
            // think it should be.
            loop {
                // Load acquire to sync with store release of `Owned<Request>`
                // being destroyed (prevent mutable access overlapping shared
                // access).
                let prev = atomic_ref.load(ordering::Acquire);

                if prev >= 1 {
                    // Store relaxed as no other operations need to happen strictly
                    // before or after the increment.
                    match atomic_ref.cmpxchg(prev, prev + 1, ordering::Relaxed) {
                        Ok(_) => break,
                        // NOTE: We cannot use the load part of a failed cmpxchg as it is always
                        // relaxed.
                        Err(_) => continue,
                    }
                } else {
                    // We are probably waiting to observe a refcount increment.
                    core::hint::spin_loop();
                    pr_info!("Waiting to acquire request\n");
                    continue;
                };
            }

            // SAFETY: We checked above that `rq_ptr` is valid for use as an `ARef`.
            Some(unsafe { Request::aref_from_raw(rq_ptr) })
        }
    }
}

#[pinned_drop]
impl<T: Operations> PinnedDrop for TagSet<T> {
    fn drop(self: Pin<&mut Self>) {
        // SAFETY: By type invariant `inner` is valid and has been properly
        // initialised during construction.
        let tagset_data = unsafe { (*self.inner.get()).driver_data };

        // SAFETY: `inner` is valid and has been properly initialised during construction.
        unsafe { bindings::blk_mq_free_tag_set(self.inner.get()) };

        // SAFETY: `tagset_data` was created by a call to
        // `ForeignOwnable::into_foreign` in `TagSet::try_new()`
        unsafe { T::TagSetData::from_foreign(tagset_data) };
    }
}

// SAFETY: It is safe to transfer ownership of `TagSet` across thread boundaries.
unsafe impl<T: Operations> Sync for TagSet<T> {}

// SAFETY: It is safe to share references to `TagSet` across thread boundaries.
unsafe impl<T: Operations> Send for TagSet<T> {}

/// A [`TagSet`] CPU to hardware queue mapping.
///
/// # Invariants
///
/// - `self.map` points to a valid `blk_mq_queue_map`
pub struct QueueMap {
    map: *mut bindings::blk_mq_queue_map,
    kind: QueueType,
}

impl QueueMap {
    /// Set the number of queues for this mapping kind.
    pub fn set_queue_count(&mut self, nr_queues: u32) {
        // SAFETY: By type invariant, `self.map` is valid.
        unsafe { (*self.map).nr_queues = nr_queues }
    }

    /// First hardware queue to map this queue kind onto. Used by the PCIe NVMe driver to map each
    /// hardware queue type ([`QueueType`]) onto a distinct set of hardware queues.
    pub fn set_offset(&mut self, offset: u32) {
        // SAFETY: By type invariant, `self.map` is valid.
        unsafe { (*self.map).queue_offset = offset }
    }

    /// Effectuate the mapping described by [`Self`].
    pub fn map_queues(&self) {
        // SAFETY: By type invariant, `self.map` is valid.
        unsafe { bindings::blk_mq_map_queues(self.map) }
    }

    /// Return the kind of this queue mapping.
    pub fn kind(&self) -> QueueType {
        self.kind
    }
}

/// Type of hardware queue.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum QueueType {
    /// All I/O not otherwise accounted for.
    Default = bindings::hctx_type_HCTX_TYPE_DEFAULT,
    /// Just for READ I/O.
    Read = bindings::hctx_type_HCTX_TYPE_READ,
    ///  Polled I/O of any kind.
    Poll = bindings::hctx_type_HCTX_TYPE_POLL,
}

impl TryFrom<u32> for QueueType {
    type Error = kernel::error::Error;

    fn try_from(value: u32) -> core::result::Result<Self, Self::Error> {
        match value {
            bindings::hctx_type_HCTX_TYPE_DEFAULT => Ok(QueueType::Default),
            bindings::hctx_type_HCTX_TYPE_READ => Ok(QueueType::Read),
            bindings::hctx_type_HCTX_TYPE_POLL => Ok(QueueType::Poll),
            _ => Err(kernel::error::code::EINVAL),
        }
    }
}

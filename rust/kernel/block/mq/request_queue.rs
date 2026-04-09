// SPDX-License-Identifier: GPL-2.0

use super::{request::SyncRequest, Command, Operations};
use crate::{
    error::from_err_ptr,
    owned::Owned,
    prelude::*,
    types::{ForeignOwnable, Opaque},
};
use core::marker::PhantomData;

/// A structure describing the queues associated with a block device.
///
/// Owned by a [`GenDisk`].
///
/// # Invariants
///
/// - `self.0` is a valid `bindings::request_queue`.
/// - `self.0.queuedata` is a valid `T::QueueData`.
#[repr(transparent)]
pub struct RequestQueue<T>(Opaque<bindings::request_queue>, PhantomData<T>);

impl<T> RequestQueue<T>
where
    T: Operations,
{
    /// Create a [`RequestQueue`] from a raw `bindings::request_queue` pointer
    ///
    /// # Safety
    ///
    /// - `ptr` must be valid for use as a reference for the duration of `'a`.
    /// - `ptr` must have been initialized as part of [`GenDiskBuilder::build`].
    pub(crate) unsafe fn from_raw<'a>(ptr: *const bindings::request_queue) -> &'a Self {
        // INVARIANT:
        // - By function safety requirements, `ptr` is a valid `request_queue`.
        // - By function safety requirement `ptr` was initialized by [`GenDiskBuilder::build`], and
        //   thus `queuedata` was set to point to a valid `T::QueueData`.
        //
        // SAFETY: By function safety requirements `ptr` is valid for use as a reference.
        unsafe { &*ptr.cast() }
    }

    /// Get the driver private data associated with this [`RequestQueue`].
    pub fn queue_data(&self) -> <T::QueueData as ForeignOwnable>::Borrowed<'_> {
        // SAFETY: By type invariant, `queuedata` is a valid `T::QueueData`.
        unsafe { T::QueueData::borrow((*self.0.get()).queuedata) }
    }

    /// Stop all hardware queues of this [`RequestQueue`].
    pub fn stop_hw_queues(&self) {
        // SAFETY: By type invariant, `self.0` is a valid `request_queue`.
        unsafe { bindings::blk_mq_stop_hw_queues(self.0.get()) }
    }

    /// Start all hardware queues of this [`RequestQueue`].
    ///
    /// This function will mark the queues as ready and if necessary, schedule the queues to run.
    pub fn start_stopped_hw_queues_async(&self) {
        // SAFETY: By type invariant, `self.0` is a valid `request_queue`.
        unsafe { bindings::blk_mq_start_stopped_hw_queues(self.0.get(), true) }
    }

    /// Allocate a synchronous request.
    pub fn alloc_sync_request(&self, command: Command) -> Result<Owned<SyncRequest<T>>> {
        let rq = from_err_ptr(unsafe {
            bindings::blk_mq_alloc_request(self.0.get(), command.as_raw(), 0)
        })?;
        // SAFETY: `rq` is valid and will be owned by new `SyncRequest`.
        Ok(unsafe { SyncRequest::from_raw(rq) })
    }
}

// SPDX-License-Identifier: GPL-2.0

//! This module provides the `TagSet` struct to wrap the C `struct blk_mq_tag_set`.
//!
//! C header: [`include/linux/blk-mq.h`](srctree/include/linux/blk-mq.h)

use core::pin::Pin;

use crate::{
    bindings,
    block::mq::{operations::OperationsVTable, request::RequestDataWrapper, Operations},
    error::{self, Result},
    try_pin_init,
    types::{ForeignOwnable, Opaque},
};
use core::{convert::TryInto, marker::PhantomData};
use pin_init::{pin_data, pinned_drop, PinInit};

mod flags;
pub use flags::Flags;

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
    /// Try to create a new tag set
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
                let status = error::to_result( unsafe { bindings::blk_mq_alloc_tag_set(tag_set.get())});
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

unsafe impl<T: Operations> Sync for TagSet<T> {}
unsafe impl<T: Operations> Send for TagSet<T> {}

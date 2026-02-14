// SPDX-License-Identifier: GPL-2.0

//! This module provides an interface for blk-mq drivers to implement.
//!
//! C header: [`include/linux/blk-mq.h`](srctree/include/linux/blk-mq.h)

use crate::{
    bindings,
    block::{
        error::BlkResult,
        mq::{gen_disk::GenDiskRef, request::RequestDataWrapper, IdleRequest, Request, TagSet},
    },
    error::{from_result, to_result, Result},
    prelude::*,
    sync::{aref::ARef, atomic::ordering, Refcount},
    types::{ForeignOwnable, Owned},
};
use core::{marker::PhantomData, ptr::NonNull};
use pin_init::PinInit;

type ForeignBorrowed<'a, T> = <T as ForeignOwnable>::Borrowed<'a>;

/// Implement this trait to interface blk-mq as block devices.
///
/// To implement a block device driver, implement this trait as described in the
/// [module level documentation]. The kernel will use the implementation of the
/// functions defined in this trait to interface a block device driver. Note:
/// There is no need for an exit_request() implementation, because the `drop`
/// implementation of the [`Request`] type will be invoked by automatically by
/// the C/Rust glue logic.
///
/// [module level documentation]: kernel::block::mq
#[macros::vtable]
pub trait Operations: Sized {
    /// Data associated with a request. This data is located next to the request
    /// structure.
    ///
    /// To be able to handle accessing this data from interrupt context, this
    /// data must be `Sync`.
    ///
    /// The `RequestData` object is initialized when the requests are allocated
    /// during queue initialization, and it is are dropped when the requests are
    /// dropped during queue teardown.
    type RequestData: Sized + Sync;

    /// Data associated with the `struct request_queue` that is allocated for
    /// the `GenDisk` associated with this `Operations` implementation.
    type QueueData: ForeignOwnable;

    /// Data associated with a dispatch queue. This is stored as a pointer in
    /// the C `struct blk_mq_hw_ctx` that represents a hardware queue.
    type HwData: ForeignOwnable;

    /// Data associated with a `TagSet`. This is stored as a pointer in `struct
    /// blk_mq_tag_set`.
    type TagSetData: ForeignOwnable;

    /// Called by the kernel to get an initializer for a `Pin<&mut RequestData>`.
    fn new_request_data() -> impl PinInit<Self::RequestData>;

    /// Called by the kernel to queue a request with the driver. If `is_last` is
    /// `false`, the driver is allowed to defer committing the request.
    fn queue_rq(
        hw_data: ForeignBorrowed<'_, Self::HwData>,
        queue_data: ForeignBorrowed<'_, Self::QueueData>,
        rq: Owned<IdleRequest<Self>>,
        is_last: bool,
    ) -> BlkResult;

    /// Called by the kernel to indicate that queued requests should be submitted.
    fn commit_rqs(
        hw_data: ForeignBorrowed<'_, Self::HwData>,
        queue_data: ForeignBorrowed<'_, Self::QueueData>,
    );

    /// Called by the kernel to allocate and initialize a driver specific hardware context data.
    fn init_hctx(
        tagset_data: ForeignBorrowed<'_, Self::TagSetData>,
        hctx_idx: u32,
    ) -> Result<Self::HwData>;

    /// Called by the kernel when the request is completed.
    fn complete(rq: ARef<Request<Self>>);

    /// Called by the kernel to poll the device for completed requests. Only
    /// used for poll queues.
    fn poll(_hw_data: ForeignBorrowed<'_, Self::HwData>) -> bool {
        build_error!(crate::error::VTABLE_DEFAULT_ERROR)
    }

    /// Called by the kernel to get a zone report from the driver.
    ///
    /// The driver must call `callback` once for each zone on `disk` and populate the first argument
    /// with a zone descriptor and the second argument when the zone index.
    // TODO: We cannot gate this on CONFIG_BLK_DEV_ZONED due to limitations of the `vtable` macro.
    fn report_zones(
        _disk: &GenDiskRef<Self>,
        _sector: u64,
        _nr_zones: u32,
        _callback: impl Fn(&bindings::blk_zone, u32) -> Result,
    ) -> Result<u32> {
        Err(ENOTSUPP)
    }

    /// Called by the kernel to map hardware queues to CPU cores.
    fn map_queues(_tag_set: Pin<&mut TagSet<Self>>) {
        build_error!(crate::error::VTABLE_DEFAULT_ERROR)
    }
}

/// A vtable for blk-mq to interact with a block device driver.
///
/// A `bindings::blk_mq_ops` vtable is constructed from pointers to the `extern
/// "C"` functions of this struct, exposed through the `OperationsVTable::VTABLE`.
///
/// For general documentation of these methods, see the kernel source
/// documentation related to `struct blk_mq_operations` in
/// [`include/linux/blk-mq.h`].
///
/// [`include/linux/blk-mq.h`]: srctree/include/linux/blk-mq.h
pub(crate) struct OperationsVTable<T: Operations>(PhantomData<T>);

impl<T: Operations> OperationsVTable<T> {
    /// This function is called by the C kernel. A pointer to this function is
    /// installed in the `blk_mq_ops` vtable for the driver.
    ///
    /// # Safety
    ///
    /// - The caller of this function must ensure that the pointee of `bd` is
    ///   valid for reads for the duration of this function.
    /// - This function must be called for an initialized and live `hctx`. That
    ///   is, `Self::init_hctx_callback` was called and
    ///   `Self::exit_hctx_callback()` was not yet called.
    /// - `(*bd).rq` must point to an initialized and live `bindings:request`.
    ///   That is, `Self::init_request_callback` was called but
    ///   `Self::exit_request_callback` was not yet called for the request.
    /// - `(*bd).rq` must be owned by the driver. That is, the block layer must
    ///   promise to not access the request until the driver calls
    ///   `bindings::blk_mq_end_request` for the request.
    unsafe extern "C" fn queue_rq_callback(
        hctx: *mut bindings::blk_mq_hw_ctx,
        bd: *const bindings::blk_mq_queue_data,
    ) -> bindings::blk_status_t {
        // SAFETY: `bd.rq` is valid as required by the safety requirement for
        // this function.
        let request = unsafe { &*(*bd).rq.cast::<Request<T>>() };

        debug_assert!(
            request
                .wrapper_ref()
                .refcount()
                .as_atomic()
                .load(ordering::Acquire)
                == 0
        );

        // INVARIANT: By C API contract, `bd.rq` has not been started yet.
        // SAFETY:
        //  - By API contract, we own the request.
        //  - By the safety requirements of this function, `request` is a valid
        //    `struct request` and the private data is properly initialized.
        //  - `rq` will be alive until `blk_mq_end_request` is called and is
        //    reference counted by until then.
        let rq = unsafe { IdleRequest::from_raw((*bd).rq) };

        // SAFETY: The safety requirement for this function ensure that `hctx`
        // is valid and that `driver_data` was produced by a call to
        // `into_foreign` in `Self::init_hctx_callback`.
        let hw_data = unsafe { T::HwData::borrow((*hctx).driver_data) };

        // SAFETY: `hctx` is valid as required by this function.
        let queue_data = unsafe { (*(*hctx).queue).queuedata };

        // SAFETY: `queue.queuedata` was created by `GenDiskBuilder::build` with
        // a call to `ForeignOwnable::into_foreign` to create `queuedata`.
        // `ForeignOwnable::from_foreign` is only called when the tagset is
        // dropped, which happens after we are dropped.
        let queue_data = unsafe { T::QueueData::borrow(queue_data) };

        let ret = T::queue_rq(
            hw_data,
            queue_data,
            rq,
            // SAFETY: `bd` is valid as required by the safety requirement for
            // this function.
            unsafe { (*bd).last },
        );

        if let Err(e) = ret {
            e.to_blk_status()
        } else {
            bindings::BLK_STS_OK
        }
    }

    /// This function is called by the C kernel. A pointer to this function is
    /// installed in the `blk_mq_ops` vtable for the driver.
    ///
    /// # Safety
    ///
    /// This function may only be called by blk-mq C infrastructure. The caller
    /// must ensure that `hctx` is valid.
    unsafe extern "C" fn commit_rqs_callback(hctx: *mut bindings::blk_mq_hw_ctx) {
        // SAFETY: `driver_data` was installed by us in `init_hctx_callback` as
        // the result of a call to `into_foreign`.
        let hw_data = unsafe { T::HwData::borrow((*hctx).driver_data) };

        // SAFETY: `hctx` is valid as required by this function.
        let queue_data = unsafe { (*(*hctx).queue).queuedata };

        // SAFETY: `queue.queuedata` was created by `GenDisk::try_new()` with a
        // call to `ForeignOwnable::into_foreign()` to create `queuedata`.
        // `ForeignOwnable::from_foreign()` is only called when the tagset is
        // dropped, which happens after we are dropped.
        let queue_data = unsafe { T::QueueData::borrow(queue_data) };
        T::commit_rqs(hw_data, queue_data)
    }

    /// This function is called by the C kernel. A pointer to this function is
    /// installed in the `blk_mq_ops` vtable for the driver.
    ///
    /// # Safety
    ///
    /// This function may only be called by blk-mq C infrastructure. `rq` must
    /// point to a valid request that has been marked as completed. The pointee
    /// of `rq` must be valid for write for the duration of this function.
    unsafe extern "C" fn complete_callback(rq: *mut bindings::request) {
        // SAFETY: This function can only be dispatched through
        // `Request::complete`. We leaked a refcount then which we pick back up
        // now.
        let aref = unsafe { Request::aref_from_raw(rq) };
        T::complete(aref);
    }

    /// This function is called by the C kernel. A pointer to this function is
    /// installed in the `blk_mq_ops` vtable for the driver.
    ///
    /// # Safety
    ///
    /// This function may only be called by blk-mq C infrastructure. `hctx` must
    /// be a pointer to a valid and aligned `struct blk_mq_hw_ctx` that was
    /// previously initialized by a call to `init_hctx_callback`.
    unsafe extern "C" fn poll_callback(
        hctx: *mut bindings::blk_mq_hw_ctx,
        _iob: *mut bindings::io_comp_batch,
    ) -> crate::ffi::c_int {
        // SAFETY: By function safety requirement, `hctx` was initialized by
        // `init_hctx_callback` and thus `driver_data` came from a call to
        // `into_foreign`.
        let hw_data = unsafe { T::HwData::borrow((*hctx).driver_data) };
        T::poll(hw_data).into()
    }

    /// This function is called by the C kernel. A pointer to this function is
    /// installed in the `blk_mq_ops` vtable for the driver.
    ///
    /// # Safety
    ///
    /// This function may only be called by blk-mq C infrastructure.
    /// `tagset_data` must be initialized by the initializer returned by
    /// `TagSet::try_new` as part of tag set initialization. `hctx` must be a
    /// pointer to a valid `blk_mq_hw_ctx` where the `driver_data` field was not
    /// yet initialized. This function may only be called onece before
    /// `exit_hctx_callback` is called for the same context.
    unsafe extern "C" fn init_hctx_callback(
        hctx: *mut bindings::blk_mq_hw_ctx,
        tagset_data: *mut core::ffi::c_void,
        hctx_idx: crate::ffi::c_uint,
    ) -> core::ffi::c_int {
        from_result(|| {
            // SAFETY: By the safety requirements of this function,
            // `tagset_data` came from a call to `into_foreign` when the
            // `TagSet` was initialized.
            let tagset_data = unsafe { T::TagSetData::borrow(tagset_data) };
            let data = T::init_hctx(tagset_data, hctx_idx)?;

            // SAFETY: by the safety requirments of this function, `hctx` is
            // valid for write
            unsafe { (*hctx).driver_data = data.into_foreign().cast() };
            Ok(0)
        })
    }

    /// This function is called by the C kernel. A pointer to this function is
    /// installed in the `blk_mq_ops` vtable for the driver.
    ///
    /// # Safety
    ///
    /// This function may only be called by blk-mq C infrastructure. `hctx` must
    /// be a valid pointer that was previously initialized by a call to
    /// `init_hctx_callback`. This function may be called only once after
    /// `init_hctx_callback` was called.
    unsafe extern "C" fn exit_hctx_callback(
        hctx: *mut bindings::blk_mq_hw_ctx,
        _hctx_idx: crate::ffi::c_uint,
    ) {
        // SAFETY: By the safety requirements of this function, `hctx` is valid for read.
        let ptr = unsafe { (*hctx).driver_data };

        // SAFETY: By the safety requirements of this function, `ptr` came from
        // a call to `into_foreign` in `init_hctx_callback`
        unsafe { T::HwData::from_foreign(ptr) };
    }

    /// This function is called by the C kernel. A pointer to this function is
    /// installed in the `blk_mq_ops` vtable for the driver.
    ///
    /// # Safety
    ///
    /// - This function may only be called by blk-mq C infrastructure.
    /// - `_set` must point to an initialized `TagSet<T>`.
    /// - `rq` must point to an initialized `bindings::request`.
    /// - The allocation pointed to by `rq` must be at the size of `Request`
    ///   plus the size of `RequestDataWrapper`.
    unsafe extern "C" fn init_request_callback(
        _set: *mut bindings::blk_mq_tag_set,
        rq: *mut bindings::request,
        _hctx_idx: crate::ffi::c_uint,
        _numa_node: crate::ffi::c_uint,
    ) -> crate::ffi::c_int {
        from_result(|| {
            // SAFETY: By the safety requirements of this function, `rq` points
            // to a valid allocation.
            let pdu = unsafe { Request::wrapper_ptr(rq.cast::<Request<T>>()) };

            // SAFETY: The refcount field is allocated but not initialized, so
            // it is valid for writes.
            unsafe { RequestDataWrapper::refcount_ptr(pdu.as_ptr()).write(Refcount::new(0)) };

            let initializer = T::new_request_data();

            // SAFETY: `pdu` is a valid pointer as established above. We do not
            // touch `pdu` if `__pinned_init` returns an error. We promise ot to
            // move the pointee of `pdu`.
            unsafe { initializer.__pinned_init(RequestDataWrapper::data_ptr(pdu.as_ptr()))? };

            Ok(0)
        })
    }

    /// This function is called by the C kernel. A pointer to this function is
    /// installed in the `blk_mq_ops` vtable for the driver.
    ///
    /// # Safety
    ///
    /// - This function may only be called by blk-mq C infrastructure.
    /// - `_set` must point to an initialized `TagSet<T>`.
    /// - `rq` must point to an initialized and valid `Request`.
    unsafe extern "C" fn exit_request_callback(
        _set: *mut bindings::blk_mq_tag_set,
        rq: *mut bindings::request,
        _hctx_idx: crate::ffi::c_uint,
    ) {
        // SAFETY: The tagset invariants guarantee that all requests are allocated with extra memory
        // for the request data.
        let pdu = unsafe { bindings::blk_mq_rq_to_pdu(rq) }.cast::<RequestDataWrapper<T>>();

        // SAFETY: `pdu` is valid for read and write and is properly initialised.
        unsafe { core::ptr::drop_in_place(pdu) };
    }

    /// This function is a callback hook for the C kernel. A pointer to this function is
    /// installed in the `blk_mq_ops` vtable for the driver.
    ///
    /// # Safety
    ///
    /// - This function may only be called by blk-mq C infrastructure.
    /// - `disk_ptr` must be a pointer to a gendisk initialized by `GenDisk::build`.
    pub(crate) unsafe extern "C" fn report_zones_callback(
        disk_ptr: *mut bindings::gendisk,
        sector: u64,
        nr_zones: u32,
        args: *mut bindings::blk_report_zones_args,
    ) -> i32 {
        // SAFETY: As `disk_ptr` is a gendisk initialized by `GenDisk::build`, `private_data` is not
        // null.
        let disk_ref_ptr = unsafe { NonNull::new_unchecked((*disk_ptr).private_data.cast()) };

        // SAFETY: `disk_ptr.private_data` is a pointer to the `GenDisk` owner of `disk_ptr` that we
        // installed when we initialized `disk_ptr`. It is valid for use as a reference for the
        // duration of this call.
        let disk = unsafe { GenDiskRef::from_ptr(disk_ref_ptr) };

        from_result(|| {
            T::report_zones(&disk, sector, nr_zones, |zone, idx| -> Result {
                to_result(
                    // SAFETY: `disk_ptr` is valid by function safety requirements.
                    unsafe {
                        bindings::disk_report_zone(
                            disk_ptr,
                            core::ptr::from_ref(zone).cast_mut(),
                            idx,
                            args,
                        )
                    },
                )
            })
            .and_then(|v: u32| -> Result<_> { Ok(v.try_into()?) })
        })
    }

    /// This function is called by the C kernel. A pointer to this function is
    /// installed in the `blk_mq_ops` vtable for the driver.
    ///
    /// # Safety
    ///
    /// This function may only be called by blk-mq C infrastructure. `tag_set`
    /// must be a pointer to a valid and initialized `TagSet<T>`. The pointee
    /// must be valid for use as a reference at least the duration of this call.
    unsafe extern "C" fn map_queues_callback(tag_set: *mut bindings::blk_mq_tag_set) {
        // SAFETY: By C API contract `tag_set` is the tag set registered with the `GenDisk` created
        // by `GenDiskBuilder`.
        let tag_set = unsafe { TagSet::from_ptr_mut(tag_set) };
        T::map_queues(tag_set);
    }

    const VTABLE: bindings::blk_mq_ops = bindings::blk_mq_ops {
        queue_rq: Some(Self::queue_rq_callback),
        queue_rqs: None,
        commit_rqs: Some(Self::commit_rqs_callback),
        get_budget: None,
        put_budget: None,
        set_rq_budget_token: None,
        get_rq_budget_token: None,
        timeout: None,
        poll: if T::HAS_POLL {
            Some(Self::poll_callback)
        } else {
            None
        },
        complete: Some(Self::complete_callback),
        init_hctx: Some(Self::init_hctx_callback),
        exit_hctx: Some(Self::exit_hctx_callback),
        init_request: Some(Self::init_request_callback),
        exit_request: Some(Self::exit_request_callback),
        cleanup_rq: None,
        busy: None,
        map_queues: if T::HAS_MAP_QUEUES {
            Some(Self::map_queues_callback)
        } else {
            None
        },
        #[cfg(CONFIG_BLK_DEBUG_FS)]
        show_rq: None,
    };

    pub(crate) const fn build() -> &'static bindings::blk_mq_ops {
        &Self::VTABLE
    }
}

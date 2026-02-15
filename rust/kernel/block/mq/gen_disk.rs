// SPDX-License-Identifier: GPL-2.0

//! Generic disk abstraction.
//!
//! C header: [`include/linux/blkdev.h`](srctree/include/linux/blkdev.h)
//! C header: [`include/linux/blk-mq.h`](srctree/include/linux/blk-mq.h)

use crate::{
    bindings,
    block::mq::{operations::OperationsVTable, Feature, Operations, RequestQueue, TagSet},
    error::{self, from_err_ptr, Result},
    fmt::{self, Write},
    prelude::*,
    revocable::Revocable,
    static_lock_class,
    str::NullTerminatedFormatter,
    sync::{Arc, UniqueArc},
    types::{ForeignOwnable, ScopeGuard},
};
use core::{marker::PhantomData, ptr::NonNull};

/// A builder for [`GenDisk`].
///
/// Use this struct to configure and add new [`GenDisk`] to the VFS.
pub struct GenDiskBuilder<T> {
    rotational: bool,
    logical_block_size: u32,
    physical_block_size: u32,
    capacity_sectors: u64,
    max_hw_discard_sectors: u32,
    #[cfg(CONFIG_BLK_DEV_ZONED)]
    zoned: bool,
    #[cfg(CONFIG_BLK_DEV_ZONED)]
    zone_size_sectors: u32,
    #[cfg(CONFIG_BLK_DEV_ZONED)]
    zone_append_max_sectors: u32,
    write_cache: bool,
    forced_unit_access: bool,
    max_sectors: u32,
    virt_boundary_mask: usize,
    _p: PhantomData<T>,
}

impl<T> Default for GenDiskBuilder<T> {
    fn default() -> Self {
        Self {
            rotational: false,
            logical_block_size: bindings::PAGE_SIZE as u32,
            physical_block_size: bindings::PAGE_SIZE as u32,
            capacity_sectors: 0,
            max_hw_discard_sectors: 0,
            #[cfg(CONFIG_BLK_DEV_ZONED)]
            zoned: false,
            #[cfg(CONFIG_BLK_DEV_ZONED)]
            zone_size_sectors: 0,
            #[cfg(CONFIG_BLK_DEV_ZONED)]
            zone_append_max_sectors: 0,
            write_cache: false,
            forced_unit_access: false,
            max_sectors: 0,
            virt_boundary_mask: 0,
            _p: PhantomData,
        }
    }
}

impl<T: Operations> GenDiskBuilder<T> {
    /// Create a new instance.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the rotational media attribute for the device to be built.
    pub fn rotational(mut self, rotational: bool) -> Self {
        self.rotational = rotational;
        self
    }

    /// Validate block size by verifying that it is between 512 and `PAGE_SIZE`,
    /// and that it is a power of two.
    pub fn validate_block_size(size: u32) -> Result {
        if !(512..=bindings::PAGE_SIZE as u32).contains(&size) || !size.is_power_of_two() {
            Err(error::code::EINVAL)
        } else {
            Ok(())
        }
    }

    /// Set the logical block size of the device to be built.
    ///
    /// This method will check that block size is a power of two and between 512
    /// and 4096. If not, an error is returned and the block size is not set.
    ///
    /// This is the smallest unit the storage device can address. It is
    /// typically 4096 bytes.
    pub fn logical_block_size(mut self, block_size: u32) -> Result<Self> {
        Self::validate_block_size(block_size)?;
        self.logical_block_size = block_size;
        Ok(self)
    }

    /// Set the physical block size of the device to be built.
    ///
    /// This method will check that block size is a power of two and between 512
    /// and 4096. If not, an error is returned and the block size is not set.
    ///
    /// This is the smallest unit a physical storage device can write
    /// atomically. It is usually the same as the logical block size but may be
    /// bigger. One example is SATA drives with 4096 byte physical block size
    /// that expose a 512 byte logical block size to the operating system.
    pub fn physical_block_size(mut self, block_size: u32) -> Result<Self> {
        Self::validate_block_size(block_size)?;
        self.physical_block_size = block_size;
        Ok(self)
    }

    /// Set the capacity of the device to be built, in sectors (512 bytes).
    pub fn capacity_sectors(mut self, capacity: u64) -> Self {
        self.capacity_sectors = capacity;
        self
    }

    /// Set the maximum amount of sectors the underlying hardware device can
    /// discard/trim in a single operation.
    ///
    /// Setting 0 (default) here will cause the disk to report discard not
    /// supported.
    pub fn max_hw_discard_sectors(mut self, max_hw_discard_sectors: u32) -> Self {
        self.max_hw_discard_sectors = max_hw_discard_sectors;
        self
    }

    /// Mark this device as a zoned block device.
    #[cfg(CONFIG_BLK_DEV_ZONED)]
    pub fn zoned(mut self, enable: bool) -> Self {
        self.zoned = enable;
        self
    }

    /// Set the zone size of this block device.
    #[cfg(CONFIG_BLK_DEV_ZONED)]
    pub fn zone_size(mut self, sectors: u32) -> Self {
        self.zone_size_sectors = sectors;
        self
    }

    /// Set the max zone append size for this block device.
    #[cfg(CONFIG_BLK_DEV_ZONED)]
    pub fn zone_append_max(mut self, sectors: u32) -> Self {
        self.zone_append_max_sectors = sectors;
        self
    }

    /// Declare that this device supports forced unit access.
    pub fn forced_unit_access(mut self, enable: bool) -> Self {
        self.forced_unit_access = enable;
        self
    }

    /// Declare that this device has a write-back cache.
    pub fn write_cache(mut self, enable: bool) -> Self {
        self.write_cache = enable;
        self
    }

    /// Maximum size of a command in 512 byte sectors.
    pub fn max_sectors(mut self, sectors: u32) -> Self {
        self.max_sectors = sectors;
        self
    }

    /// Set the I/O segment memory alignment mask for the block device. I/O requests to this device
    /// will be split between segments wherever either the memory address of the end of the previous
    /// segment or the memory address of the beginning of the current segment is not aligned to
    /// virt_boundary_mask + 1 bytes.
    pub fn virt_boundary_mask(mut self, mask: usize) -> Self {
        self.virt_boundary_mask = mask;
        self
    }

    /// Build a new `GenDisk` and add it to the VFS.
    pub fn build(
        self,
        name: fmt::Arguments<'_>,
        tagset: Arc<TagSet<T>>,
        queue_data: T::QueueData,
    ) -> Result<Arc<GenDisk<T>>> {
        let data = queue_data.into_foreign();
        let recover_data = ScopeGuard::new(|| {
            // SAFETY: T::QueueData was created by the call to `into_foreign()` above
            drop(unsafe { T::QueueData::from_foreign(data) });
        });

        // SAFETY: `bindings::queue_limits` contain only fields that are valid when zeroed.
        let mut lim: bindings::queue_limits = unsafe { core::mem::zeroed() };

        lim.logical_block_size = self.logical_block_size;
        lim.physical_block_size = self.physical_block_size;
        lim.max_hw_discard_sectors = self.max_hw_discard_sectors;
        lim.max_sectors = self.max_sectors;
        lim.virt_boundary_mask = self.virt_boundary_mask;
        if self.rotational {
            lim.features = Feature::Rotational.into();
        }

        #[cfg(CONFIG_BLK_DEV_ZONED)]
        if self.zoned {
            if !T::HAS_REPORT_ZONES {
                return Err(error::code::EINVAL);
            }

            //lim.features |= request::Feature::Zoned.into();
            lim.chunk_sectors = self.zone_size_sectors;
            lim.max_hw_zone_append_sectors = self.zone_append_max_sectors;
        }

        if self.write_cache {
            lim.features |= Feature::WriteCache;
        }

        if self.forced_unit_access {
            lim.features |= Feature::ForcedUnitAccess;
        }

        // SAFETY: `tagset.raw_tag_set()` points to a valid and initialized tag set
        let gendisk = from_err_ptr(unsafe {
            bindings::__blk_mq_alloc_disk(
                tagset.raw_tag_set(),
                &mut lim,
                data,
                static_lock_class!().as_ptr(),
            )
        })?;

        // SAFETY: `gendisk` is a valid pointer as we initialized it above
        unsafe { (*gendisk).fops = Self::build_vtable() };

        let mut writer = NullTerminatedFormatter::new(
            // SAFETY: `gendisk` points to a valid and initialized instance. We
            // have exclusive access, since the disk is not added to the VFS
            // yet.
            unsafe { &mut (*gendisk).disk_name },
        )
        .ok_or(EINVAL)?;
        writer.write_fmt(name)?;

        // SAFETY: `gendisk` points to a valid and initialized instance of
        // `struct gendisk`. `set_capacity` takes a lock to synchronize this
        // operation, so we will not race.
        unsafe { bindings::set_capacity(gendisk, self.capacity_sectors) };

        recover_data.dismiss();

        // INVARIANT: `gendisk` was initialized above.
        // INVARIANT: `gendisk` was added to the VFS via `device_add_disk` above.
        // INVARIANT: `gendisk.queue.queue_data` is set to `data` in the call to
        // `__blk_mq_alloc_disk` above.
        let mut disk = UniqueArc::new(
            GenDisk {
                tag_set: tagset,
                gendisk,
                backref: Arc::pin_init(
                    // INVARIANT: We break `GenDiskRef` invariant here, but we restore it below.
                    Revocable::new(GenDiskRef(NonNull::dangling())),
                    GFP_KERNEL,
                )?,
            },
            GFP_KERNEL,
        )?;

        disk.backref = Arc::pin_init(
            // INVARIANT: The `GenDisk` in `disk` is a valid for use as a reference.
            Revocable::new(GenDiskRef(
                NonNull::new(disk.as_ptr().cast_mut()).expect("Should not be null"),
            )),
            GFP_KERNEL,
        )?;

        let disk: Arc<_> = disk.into();

        // SAFETY: `disk.gendisk` is valid for write as we initialized it above. We have exclusive
        // access.
        unsafe { (*disk.gendisk).private_data = Arc::as_ptr(&disk).cast_mut().cast() };

        #[cfg(CONFIG_BLK_DEV_ZONED)]
        if self.zoned {
            // SAFETY: `disk.gendisk` is valid as we initialized it above. We have exclusive access.
            unsafe { bindings::blk_revalidate_disk_zones(gendisk) };
        }

        crate::error::to_result(
            // SAFETY: `gendisk` points to a valid and initialized instance of
            // `struct gendisk`.
            unsafe {
                bindings::device_add_disk(core::ptr::null_mut(), gendisk, core::ptr::null_mut())
            },
        )?;

        Ok(disk)
    }

    const VTABLE: bindings::block_device_operations = bindings::block_device_operations {
        submit_bio: None,
        open: None,
        release: None,
        ioctl: None,
        compat_ioctl: None,
        check_events: None,
        unlock_native_capacity: None,
        getgeo: None,
        set_read_only: None,
        swap_slot_free_notify: None,
        report_zones: if T::HAS_REPORT_ZONES {
            Some(OperationsVTable::<T>::report_zones_callback)
        } else {
            None
        },
        devnode: None,
        alternative_gpt_sector: None,
        get_unique_id: None,
        // TODO: Set to THIS_MODULE. Waiting for const_refs_to_static feature to
        // be merged (unstable in rustc 1.78 which is staged for linux 6.10)
        // <https://github.com/rust-lang/rust/issues/119618>
        owner: core::ptr::null_mut(),
        pr_ops: core::ptr::null_mut(),
        free_disk: None,
        poll_bio: None,
    };

    pub(crate) const fn build_vtable() -> &'static bindings::block_device_operations {
        &Self::VTABLE
    }
}

/// A generic block device.
///
/// # Invariants
///
///  - `gendisk` must always point to an initialized and valid `struct gendisk`.
///  - `gendisk` was added to the VFS through a call to
///    `bindings::device_add_disk`.
///  - `self.gendisk.queue.queuedata` is initialized by a call to `ForeignOwnable::into_foreign`.
pub struct GenDisk<T: Operations> {
    tag_set: Arc<TagSet<T>>,
    gendisk: *mut bindings::gendisk,
    backref: Arc<Revocable<GenDiskRef<T>>>,
}

impl<T: Operations> GenDisk<T> {
    /// Get a `GenDiskRef` referencing this `GenDisk`.
    pub fn get_ref(&self) -> Arc<Revocable<GenDiskRef<T>>> {
        self.backref.clone()
    }

    /// Get the [`RequestQueue`] associated with this [`GenDisk`].
    pub fn queue(&self) -> &RequestQueue<T> {
        // SAFETY: By type invariant, self is a valid gendisk.
        unsafe { RequestQueue::from_raw((*self.gendisk).queue) }
    }

    /// Get the queue data associated with this [`GenDisk`].
    pub fn queue_data(&self) -> <T::QueueData as ForeignOwnable>::Borrowed<'_> {
        // SAFETY: By type invariant, self is a valid gendisk.
        unsafe { T::QueueData::borrow((*(*self.gendisk).queue).queuedata) }
    }

    /// Get a reference to the `TagSet` used by this `GenDisk`.
    pub fn tag_set(&self) -> &Arc<TagSet<T>> {
        &self.tag_set
    }
}

// SAFETY: `GenDisk` is an owned pointer to a `struct gendisk` and an `Arc` to a
// `TagSet` It is safe to send this to other threads as long as T is Send.
unsafe impl<T: Operations + Send> Send for GenDisk<T> {}

// SAFETY: `GenDisk` is an owned pointer to a `struct gendisk` and an `Arc` to a `TagSet`. It is
// safe to reference these from multiple threads.
unsafe impl<T: Operations> Sync for GenDisk<T> {}

impl<T: Operations> Drop for GenDisk<T> {
    fn drop(&mut self) {
        // SAFETY: By type invariant of `Self`, `self.gendisk` points to a valid
        // and initialized instance of `struct gendisk`, and, `queuedata` was
        // initialized with the result of a call to
        // `ForeignOwnable::into_foreign`.
        let queue_data = unsafe { (*(*self.gendisk).queue).queuedata };

        // SAFETY: By type invariant, `self.gendisk` points to a valid and
        // initialized instance of `struct gendisk`, and it was previously added
        // to the VFS.
        unsafe { bindings::del_gendisk(self.gendisk) };

        // SAFETY: `queue.queuedata` was created by `GenDiskBuilder::build` with
        // a call to `ForeignOwnable::into_foreign` to create `queuedata`.
        // `ForeignOwnable::from_foreign` is only called here.
        drop(unsafe { T::QueueData::from_foreign(queue_data) });
    }
}

/// A reference to a `GenDisk`.
///
/// # Invariants
///
/// `self.0` is valid for use as a reference.
pub struct GenDiskRef<T: Operations>(NonNull<GenDisk<T>>);

impl<T: Operations> GenDiskRef<T> {
    /// Create a `GenDiskRef` from a pointer to a `GenDisk`.
    ///
    /// # Safety
    ///
    /// `ptr` must be valid for use as a `GenDisk` reference for the lifetime of the returned
    /// `GenDiskRef`.
    pub(crate) unsafe fn from_ptr(ptr: NonNull<GenDisk<T>>) -> GenDiskRef<T> {
        Self(ptr)
    }
}

// SAFETY: It is safe to transfer ownership of `GenDiskRef` across thread boundaries.
unsafe impl<T: Operations> Send for GenDiskRef<T> {}

// SAFETY: It is safe to share references to `GenDiskRef` across thread boundaries.
unsafe impl<T: Operations> Sync for GenDiskRef<T> {}

impl<T: Operations> core::ops::Deref for GenDiskRef<T> {
    type Target = GenDisk<T>;

    fn deref(&self) -> &Self::Target {
        // SAFETY: By type invariant, `self.0` is valid for use as a reference.
        unsafe { self.0.as_ref() }
    }
}

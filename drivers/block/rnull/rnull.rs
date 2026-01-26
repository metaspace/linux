// SPDX-License-Identifier: GPL-2.0

//! This is a Rust implementation of the C null block driver.

#![recursion_limit = "256"]

mod configfs;
mod disk_storage;
mod util;
#[cfg(CONFIG_BLK_DEV_ZONED)]
mod zoned;

use configfs::{IRQMode, QueueConfig};
use core::{error, option::Option::Some};
use disk_storage::{
    DiskStorage,
    NullBlockPage,
    TreeContainer, //
};
use kernel::{
    bindings,
    block::{
        self,
        badblocks::{
            self,
            BadBlocks, //
        },
        bio::Segment,
        error::{code::BLK_STS_DEV_RESOURCE, BlkError, BlkResult},
        mq::{
            self,
            gen_disk::{
                self,
                GenDisk,
                GenDiskRef, //
            },
            IdleRequest,
            IoCompletionBatch,
            Operations,
            Request,
            RequestList,
            RequestTimeoutStatus,
            TagSet, //
        },
        SECTOR_SHIFT,
    },
    error::{
        code,
        Result, //
    },
    ffi,
    impl_has_hr_timer,
    new_mutex,
    new_spinlock,
    prelude::*,
    revocable::Revocable,
    str::CString,
    sync::{
        atomic::{
            ordering,
            Atomic, //
        },
        Arc,
        ArcBorrow,
        Mutex,
        SetOnce,
        SpinLock,
        SpinLockGuard, //
    },
    time::{
        hrtimer::{
            self,
            ArcHrTimerHandle,
            HrTimer,
            HrTimerCallback,
            HrTimerCallbackContext,
            HrTimerPointer,
            HrTimerRestart, //
        },
        Delta,
    },
    types::{
        ARef,
        BorrowIterator,
        OwnableRefCounted,
        Owned, //
    },
    xarray, //
};
use pin_init::PinInit;
use util::*;

#[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
use kernel::fault_injection::FaultConfig;

module! {
    type: NullBlkModule,
    name: "rnull_mod",
    authors: ["Andreas Hindborg"],
    description: "Rust implementation of the C null block driver",
    license: "GPL v2",
    params: {
        gb: u64 {
            default: 4,
            description: "Device capacity in GiB",
        },
        rotational: u8 {
            default: 0,
            description: "Set the rotational feature for the device (0 for false, 1 for true). Default: 0",
        },
        bs: u32 {
            default: 4096,
            description: "Block size (in bytes)",
        },
        nr_devices: u64 {
            default: 1,
            description: "Number of devices to register",
        },
        irqmode: u8 {
            default: 0,
            description:  "IRQ completion handler. 0-none, 1-softirq, 2-timer",
        },
        completion_nsec: u64 {
            default: 10_000,
            description:  "Time in ns to complete a request in hardware. Default: 10,000ns",
        },
        memory_backed: u8 {
            default: 0,
            description: "Create a memory-backed block device. 0-false, 1-true. Default: 0",
        },
        submit_queues: u32 {
            default: 1,
            description: "Number of submission queues",
        },
        use_per_node_hctx: u8 {
            default: 0,
            description:  "Use per-node allocation for hardware context queues, 0-false, 1-true. Default: 0-false",
        },
        home_node: i32 {
            default: -1,
            description: "Home node for the device. Default: -1 (no node)",
        },
        discard: u8 {
            default: 0,
            description: "Support discard operations (requires memory-backed null_blk device). Default: false",
        },
        no_sched: u8 {
            default: 0,
            description: "No IO scheduler",
        },
        mbps: u32 {
            default: 0,
            description: "Max bandwidth in MiB/s. 0 means no limit.",
        },
        blocking: u8 {
            default: 0,
            description: "Register as a blocking blk-mq driver device",
        },
        shared_tags: u8 {
            default: 0,
            description: "Share tag set between devices for blk-mq",
        },
        hw_queue_depth: u32 {
            default: 64,
            description:  "Queue depth for each hardware queue. Default: 64",
        },
        zoned: u8 {
            default: 0,
            description: "Make device as a host-managed zoned block device. Default: 0",
        },
        zone_size: u32 {
            default: 256,
            description: "Zone size in MB when block device is zoned. Must be power-of-two: Default: 256",
        },
        zone_capacity: u32 {
            default: 0,
            description: "Zone capacity in MB when block device is zoned. Can be less than or equal to zone size. Default: Zone size",
        },
        zone_nr_conv: u32 {
            default: 0,
            description: "Number of conventional zones when block device is zoned. Default: 0",
        },
        zone_max_open: u32 {
            default: 0,
            description: "Maximum number of open zones when block device is zoned. Default: 0 (no limit)",
        },
        zone_max_active: u32 {
            default: 0,
            description: "Maximum number of active zones when block device is zoned. Default: 0 (no limit)",
        },
        zone_append_max_sectors: u32 {
            default: 0,
            description: "Maximum size of a zone append command (in 512B sectors). Specify 0 for no zone append.",
        },
        poll_queues: u32 {
            default: 0,
            description: "Number of IOPOLL submission queues.",
        },
        fua: u8 {
            default: 1,
            description: "Enable/disable FUA support when cache_size is used. Default: 1 (true)",
        },
        max_sectors: u32 {
            default: 0,
            description: "Maximum size of a command (in 512B sectors)",
        },
    },
}

// TODO: Fault inject via params - requires module_params string support.

#[pin_data]
struct NullBlkModule {
    #[pin]
    configfs_subsystem: kernel::configfs::Subsystem<configfs::Config>,
    #[pin]
    param_disks: Mutex<KVec<Arc<GenDisk<NullBlkDevice>>>>,
}

impl kernel::InPlaceModule for NullBlkModule {
    fn init(_module: &'static ThisModule) -> impl PinInit<Self, Error> {
        pr_info!("Rust null_blk loaded\n");

        let mut disks = KVec::new();

        let defer_init = move || -> Result<_, Error> {
            let completion_time: i64 = (*module_parameters::completion_nsec.value()).try_into()?;
            for i in 0..(*module_parameters::nr_devices.value()) {
                let name = CString::try_from_fmt(fmt!("rnullb{}", i))?;

                let submit_queues = if *module_parameters::use_per_node_hctx.value() != 0 {
                    kernel::num_online_nodes()
                } else {
                    *module_parameters::submit_queues.value()
                };

                let poll_queues = *module_parameters::poll_queues.value();

                let block_size_bytes = *module_parameters::bs.value();
                let disk = NullBlkDevice::new(
                    &name,
                    block_size_bytes,
                    *module_parameters::rotational.value() != 0,
                    *module_parameters::gb.value() * 1024,
                    (*module_parameters::irqmode.value()).try_into()?,
                    Delta::from_nanos(completion_time),
                    *module_parameters::memory_backed.value() != 0,
                    Arc::pin_init(
                        new_mutex!(QueueConfig {
                            submit_queues,
                            poll_queues
                        }),
                        GFP_KERNEL,
                    )?,
                    *module_parameters::home_node.value(),
                    *module_parameters::discard.value() != 0,
                    *module_parameters::no_sched.value() != 0,
                    Arc::pin_init(BadBlocks::new(false), GFP_KERNEL)?,
                    false,
                    false,
                    Arc::pin_init(DiskStorage::new(0, block_size_bytes), GFP_KERNEL)?,
                    (*module_parameters::mbps.value()) as u64 * 2u64.pow(20),
                    *module_parameters::blocking.value() != 0,
                    *module_parameters::shared_tags.value() != 0,
                    *module_parameters::hw_queue_depth.value(),
                    *module_parameters::zoned.value() != 0,
                    *module_parameters::zone_size.value(),
                    *module_parameters::zone_capacity.value(),
                    *module_parameters::zone_nr_conv.value(),
                    *module_parameters::zone_max_open.value(),
                    *module_parameters::zone_max_active.value(),
                    *module_parameters::zone_append_max_sectors.value(),
                    *module_parameters::fua.value() != 0,
                    #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
                    Arc::pin_init(FaultConfig::new(c"requeue_inject"), GFP_KERNEL)?,
                    #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
                    Arc::pin_init(FaultConfig::new(c"init_hctx_fault_inject"), GFP_KERNEL)?,
                    #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
                    Arc::pin_init(FaultConfig::new(c"timeout_inject"), GFP_KERNEL)?,
                    *module_parameters::max_sectors.value(),
                )?;
                disks.push(disk, GFP_KERNEL)?;
            }

            Ok(disks)
        };

        try_pin_init!(Self {
            configfs_subsystem <- configfs::subsystem(),
            param_disks <- new_mutex!(defer_init()?),
        })
    }
}

static SHARED_TAG_SET: SetOnce<Arc<TagSet<NullBlkDevice>>> = SetOnce::new();

#[pin_data]
struct NullBlkDevice {
    storage: Arc<DiskStorage>,
    irq_mode: IRQMode,
    completion_time: Delta,
    memory_backed: bool,
    block_size_bytes: u32,
    bad_blocks: Arc<BadBlocks>,
    bad_blocks_once: bool,
    bad_blocks_partial_io: bool,
    bandwidth_limit: u64,
    #[pin]
    bandwidth_timer: HrTimer<Self>,
    bandwidth_bytes: Atomic<u64>,
    #[pin]
    bandwidth_timer_handle: SpinLock<Option<ArcHrTimerHandle<Self>>>,
    disk: SetOnce<Arc<Revocable<GenDiskRef<Self>>>>,
    #[cfg(CONFIG_BLK_DEV_ZONED)]
    #[pin]
    zoned: zoned::ZoneOptions,
    #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
    requeue_inject: Arc<FaultConfig>,
    #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
    requeue_selector: kernel::sync::atomic::Atomic<u64>,
    #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
    timeout_inject: Arc<FaultConfig>,
}

impl NullBlkDevice {
    const BANDWIDTH_TIMER_INTERVAL: Delta = Delta::from_millis(20);

    // TODO: Change to "attach"
    fn new(
        name: &CStr,
        block_size_bytes: u32,
        rotational: bool,
        device_capacity_mib: u64,
        irq_mode: IRQMode,
        completion_time: Delta,
        memory_backed: bool,
        queue_config: Arc<Mutex<QueueConfig>>,
        home_node: i32,
        discard: bool,
        no_sched: bool,
        bad_blocks: Arc<BadBlocks>,
        bad_blocks_once: bool,
        bad_blocks_partial_io: bool,
        storage: Arc<DiskStorage>,
        bandwidth_limit: u64,
        blocking: bool,
        shared_tagset: bool,
        hw_queue_depth: u32,
        zoned: bool,
        zone_size_mib: u32,
        zone_capacity_mib: u32,
        zone_nr_conv: u32,
        zone_max_open: u32,
        zone_max_active: u32,
        zone_append_max_sectors: u32,
        forced_unit_access: bool,
        #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)] requeue_inject: Arc<FaultConfig>,
        #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)] init_hctx_inject: Arc<FaultConfig>,
        #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)] timeout_inject: Arc<FaultConfig>,
        max_sectors: u32,
    ) -> Result<Arc<GenDisk<Self>>> {
        let mut flags = mq::TagSetFlags::default();

        if blocking || memory_backed {
            flags |= mq::TagSetFlags::BLOCKING;
        }

        if no_sched {
            flags |= mq::TagSetFlags::NO_DEFAULT_SCHEDULER;
        }

        if home_node > kernel::num_online_nodes().try_into()? {
            return Err(code::EINVAL);
        }

        let queue_config_guard = queue_config.lock();
        let submit_queues = queue_config_guard.submit_queues;
        let poll_queues = queue_config_guard.poll_queues;
        drop(queue_config_guard);

        let tagset_ctor = || -> Result<Arc<_>> {
            Ok(Arc::pin_init(
                TagSet::new(
                    submit_queues + poll_queues,
                    KBox::new(
                        NullBlkTagsetData {
                            queue_depth: hw_queue_depth,
                            queue_config,
                            #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
                            init_hctx_inject,
                        },
                        GFP_KERNEL,
                    )?,
                    hw_queue_depth,
                    if poll_queues == 0 { 1 } else { 3 },
                    home_node,
                    flags,
                ),
                GFP_KERNEL,
            )?)
        };

        let tagset = if shared_tagset {
            SHARED_TAG_SET.as_ref_or_populate_with(tagset_ctor)?.clone()
        } else {
            tagset_ctor()?
        };

        let device_capacity_sectors = mib_to_sectors(device_capacity_mib);

        let s = storage.clone();
        let queue_data = Arc::try_pin_init(
            try_pin_init!(Self {
                storage: s,
                irq_mode,
                completion_time,
                memory_backed,
                block_size_bytes,
                bad_blocks,
                bad_blocks_once,
                bad_blocks_partial_io,
                bandwidth_limit: bandwidth_limit / 50,
                bandwidth_timer <- HrTimer::new(),
                bandwidth_bytes: Atomic::new(0),
                bandwidth_timer_handle <- new_spinlock!(None),
                disk: SetOnce::new(),
                #[cfg(CONFIG_BLK_DEV_ZONED)]
                zoned <- zoned::ZoneOptions::new(
                    zoned,
                    device_capacity_mib,
                    *block_size_bytes,
                    zone_size_mib,
                    zone_capacity_mib,
                    zone_nr_conv,
                    zone_max_open,
                    zone_max_active,
                    zone_append_max_sectors,
                )?,
                #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
                requeue_inject,
                #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
                requeue_selector: Atomic::new(0),
                #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
                timeout_inject,
            }),
            GFP_KERNEL,
        )?;

        let mut builder = gen_disk::GenDiskBuilder::new()
            .capacity_sectors(device_capacity_sectors)
            .logical_block_size(block_size_bytes)?
            .physical_block_size(block_size_bytes)?
            .zone_append_max(zone_append_max_sectors)
            .rotational(rotational)
            .write_cache(storage.cache_enabled())
            .forced_unit_access(forced_unit_access && storage.cache_enabled());

        #[cfg(CONFIG_BLK_DEV_ZONED)]
        {
            builder = builder
                .zoned(zoned)
                .zone_size(queue_data.zoned.size_sectors);
        }

        // TODO: Warn on invalid discard configuration (zoned, memory)
        if memory_backed && discard && !zoned {
            builder = builder
                // Max IO size is u32::MAX bytes
                .max_hw_discard_sectors(ffi::c_uint::MAX >> block::SECTOR_SHIFT);
        }

        let disk = builder.build(fmt!("{}", name.to_str()?), tagset, queue_data)?;
        let queue_data: ArcBorrow<'_, Self> = disk.queue_data();
        queue_data.disk.populate(disk.get_ref());
        Ok(disk)
    }

    fn preload<'b, 'c>(
        tree_guard: &'b mut SpinLockGuard<'c, Pin<KBox<TreeContainer>>>,
        hw_data_guard: &'b mut SpinLockGuard<'c, HwQueueContext>,
        block_size_bytes: u32,
    ) -> Result {
        let free_count = hw_data_guard.preload.free_count();
        if free_count > 0 {
            let mut preload = tree_guard.do_unlocked(|| {
                hw_data_guard.do_unlocked(|| -> Result<_> {
                    let mut v = KVec::new();
                    for _ in 0..free_count {
                        v.push(xarray::XArrayPreloadNode::new(GFP_KERNEL)?, GFP_KERNEL)?
                    }
                    Ok(v)
                })
            })?;
            hw_data_guard.preload.preload_with(&mut preload)?;
        }

        if hw_data_guard.page.is_none() {
            hw_data_guard.page = Some(tree_guard.do_unlocked(|| {
                hw_data_guard.do_unlocked(|| NullBlockPage::new(block_size_bytes))
            })?);
        }

        Ok(())
    }

    #[inline(always)]
    fn write<'a, 'b, 'c>(
        &'a self,
        mut tree_guard: &'b mut SpinLockGuard<'c, Pin<KBox<TreeContainer>>>,
        mut hw_data_guard: &'b mut SpinLockGuard<'c, HwQueueContext>,
        mut sector: u64,
        mut segment: Segment<'_>,
        bypass_cache: bool,
    ) -> Result {
        while !segment.is_empty() {
            Self::preload(&mut tree_guard, &mut hw_data_guard, self.block_size_bytes)?;

            let mut access = self.storage.access(&mut tree_guard, &mut hw_data_guard);

            if bypass_cache {
                if let Some(page) = access.get_cache_page(sector) {
                    page.set_free(sector);
                }
            }

            let page = access.get_write_page(sector, bypass_cache)?;
            page.set_occupied(sector);
            let page_offset = (sector & block::SECTOR_MASK as u64) << block::SECTOR_SHIFT;

            sector += segment.copy_to_page_limit(
                page.page_mut().get_pin_mut(),
                page_offset as usize,
                self.block_size_bytes.try_into()?,
            ) as u64
                >> block::SECTOR_SHIFT;
        }
        Ok(())
    }

    #[inline(always)]
    fn read<'a, 'b, 'c>(
        &'a self,
        mut tree_guard: &'b mut SpinLockGuard<'c, Pin<KBox<TreeContainer>>>,
        mut hw_data_guard: &'b mut SpinLockGuard<'c, HwQueueContext>,
        mut sector: u64,
        mut segment: Segment<'_>,
    ) -> Result {
        let access = self.storage.access(&mut tree_guard, &mut hw_data_guard);

        while !segment.is_empty() {
            let page = access.get_read_page(sector);

            match page {
                Some(page) => {
                    let page_offset = (sector & block::SECTOR_MASK as u64) << block::SECTOR_SHIFT;
                    sector += segment.copy_from_page(page.page(), page_offset as usize) as u64
                        >> block::SECTOR_SHIFT;
                }
                None => sector += bytes_to_sectors(segment.zero_page() as u64),
            }
        }

        Ok(())
    }

    #[inline(never)]
    fn transfer(
        &self,
        hw_data: &Pin<&SpinLock<HwQueueContext>>,
        rq: &mut Owned<mq::Request<Self>>,
        command: mq::Command,
        sectors: u32,
    ) -> Result {
        let mut sector = rq.sector();
        let end_sector = sector + <u32 as Into<u64>>::into(sectors);

        // TODO: Use `PerCpu` to get rid of this lock
        let mut hw_data_guard = hw_data.lock();
        let mut tree_guard = self.storage.lock();

        let skip_cache = rq.flags().contains(mq::RequestFlag::ForcedUnitAccess);

        for bio in rq.bio_iter_mut() {
            let mut segment_iter = bio.segment_iter();
            while let Some(segment) = segment_iter.next() {
                // Length might be limited by bad blocks.
                let length = segment
                    .len()
                    .min((end_sector - sector) as u32 >> SECTOR_SHIFT);
                match command {
                    mq::Command::Write => self.write(
                        &mut tree_guard,
                        &mut hw_data_guard,
                        sector,
                        segment,
                        skip_cache,
                    )?,
                    mq::Command::Read => {
                        self.read(&mut tree_guard, &mut hw_data_guard, sector, segment)?
                    }
                    _ => (),
                }
                sector += length as u64 >> SECTOR_SHIFT;

                if sector >= end_sector {
                    return Ok(());
                }
            }
        }

        Ok(())
    }

    fn handle_regular_command(
        &self,
        hw_data: &Pin<&SpinLock<HwQueueContext>>,
        rq: &mut Owned<mq::Request<Self>>,
    ) -> Result {
        let mut sectors = rq.sectors();

        self.handle_bad_blocks(rq, &mut sectors)?;

        if self.memory_backed {
            if rq.command() == mq::Command::Discard {
                self.storage.discard(&hw_data, rq.sector(), sectors);
            } else {
                self.transfer(&hw_data, rq, rq.command(), sectors)?;
            }
        }

        Ok(())
    }

    fn handle_bad_blocks(&self, rq: &mut Owned<mq::Request<Self>>, sectors: &mut u32) -> Result {
        if self.bad_blocks.enabled() {
            let start = rq.sector();
            let end = start + *sectors as u64;
            match self.bad_blocks.check(start..end) {
                badblocks::BlockStatus::None => {}
                badblocks::BlockStatus::Acknowledged(mut range)
                | badblocks::BlockStatus::Unacknowledged(mut range) => {
                    rq.data_ref()
                        .error
                        .store(block::error::code::BLK_STS_IOERR.into(), ordering::Relaxed);

                    if self.bad_blocks_once {
                        self.bad_blocks.set_good(range.clone())?;
                    }

                    if self.bad_blocks_partial_io {
                        let block_size_sectors = bytes_to_sectors(self.block_size_bytes) as u64;
                        range.start = align_down(range.start, block_size_sectors);
                        if start < range.start {
                            *sectors = (range.start - start) as u32;
                        }
                    } else {
                        *sectors = 0;
                    }
                }
            };
        }
        Ok(())
    }

    fn end_request(rq: Owned<mq::Request<Self>>) {
        let status = rq.data_ref().error.load(ordering::Relaxed);
        rq.data_ref().error.store(0, ordering::Relaxed);

        // TODO: Use correct error code
        match status {
            0 => rq.end_ok(),
            _ => rq.end(bindings::BLK_STS_IOERR),
        }
    }

    fn complete_request(&self, rq: Owned<mq::Request<Self>>) {
        match self.irq_mode {
            IRQMode::None => Self::end_request(rq),
            IRQMode::Soft => mq::Request::complete(rq.into()),
            IRQMode::Timer => {
                OwnableRefCounted::into_shared(rq)
                    .start(self.completion_time)
                    .dismiss();
            }
        }
    }

    #[inline(always)]
    fn queue_rq_internal(
        hw_data: Pin<&SpinLock<HwQueueContext>>,
        this: ArcBorrow<'_, Self>,
        rq: Owned<mq::IdleRequest<Self>>,
        _is_last: bool,
    ) -> Result<(), QueueRequestError> {
        #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
        if rq.queue_data().requeue_inject.should_fail(1) {
            if rq
                .queue_data()
                .requeue_selector
                .fetch_add(1, ordering::Relaxed)
                & 1
                == 0
            {
                return Err(QueueRequestError {
                    request: rq,
                    error: BLK_STS_DEV_RESOURCE,
                });
            } else {
                rq.requeue(true);
                return Ok(());
            }
        }

        if this.bandwidth_limit != 0 {
            if !this.bandwidth_timer.active() {
                drop(this.bandwidth_timer_handle.lock().take());
                let arc: Arc<_> = this.into();
                *this.bandwidth_timer_handle.lock() =
                    Some(arc.start(Self::BANDWIDTH_TIMER_INTERVAL));
            }

            if this
                .bandwidth_bytes
                .fetch_add(rq.bytes() as u64, ordering::Relaxed)
                + (rq.bytes() as u64)
                > this.bandwidth_limit
            {
                rq.queue().stop_hw_queues();
                if this.bandwidth_bytes.load(ordering::Relaxed) <= this.bandwidth_limit {
                    rq.queue().start_stopped_hw_queues_async();
                }

                return Err(QueueRequestError {
                    request: rq,
                    error: kernel::block::error::code::BLK_STS_DEV_RESOURCE,
                });
            }
        }

        let mut rq = rq.start();

        #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
        if rq.queue_data().timeout_inject.should_fail(1) {
            rq.data_ref().fake_timeout.store(1, ordering::Relaxed);
            return Ok(());
        }

        if rq.command() == mq::Command::Flush {
            if this.memory_backed {
                this.storage.flush(&hw_data);
            }
            this.complete_request(rq);

            return Ok(());
        }

        let status = (|| -> Result {
            #[cfg(CONFIG_BLK_DEV_ZONED)]
            if this.zoned.enabled {
                this.handle_zoned_command(&hw_data, &mut rq)?;
            } else {
                this.handle_regular_command(&hw_data, &mut rq)?;
            }
            #[cfg(not(CONFIG_BLK_DEV_ZONED))]
            this.handle_regular_command(&hw_data, &mut rq)?;

            Ok(())
        })();

        if let Err(_) = status {
            // Do not overwrite existing error.
            rq.data_ref().error.cmpxchg(
                0,
                kernel::block::error::code::BLK_STS_IOERR.into(),
                ordering::Relaxed,
            );
        }

        if rq.is_poll() {
            // NOTE: We lack the ability to insert `Owned<Request>` into a
            // `kernel::list::List`, so we use a `RingBuffer` instead. The
            // drawback of this is that we have to allocate the space for the
            // ring buffer during drive initialization, and we have to hold the
            // lock protecting the list until we have processed all the requests
            // in the list. Change to a linked list when the kernel gets this
            // ability.

            // NOTE: We are processing requests during submit rather than during
            // poll. This is different from C driver. C driver does processing
            // during poll.

            hw_data
                .lock()
                .poll_queue
                .push_head(rq)
                .expect("Buffer is sized to hold all in flight requests");
        } else {
            this.complete_request(rq);
        }

        Ok(())
    }
}

struct QueueRequestError {
    request: Owned<IdleRequest<NullBlkDevice>>,
    error: BlkError,
}

impl From<QueueRequestError> for BlkError {
    fn from(value: QueueRequestError) -> Self {
        kernel::block::error::code::BLK_STS_IOERR
    }
}

impl_has_hr_timer! {
    impl HasHrTimer<Self> for NullBlkDevice {
        mode: hrtimer::RelativeHardMode<kernel::time::Monotonic>,
        field: self.bandwidth_timer,
    }
}

impl HrTimerCallback for NullBlkDevice {
    type Pointer<'a> = Arc<Self>;

    fn run(
        this: ArcBorrow<'_, Self>,
        mut context: HrTimerCallbackContext<'_, Self>,
    ) -> HrTimerRestart {
        if this.bandwidth_bytes.load(ordering::Relaxed) == 0 {
            return HrTimerRestart::NoRestart;
        }

        this.disk.as_ref().map(|disk| {
            disk.try_access()
                .map(|disk| disk.queue().start_stopped_hw_queues_async())
        });

        this.bandwidth_bytes.store(0, ordering::Relaxed);

        context.forward_now(Self::BANDWIDTH_TIMER_INTERVAL);
        HrTimerRestart::Restart
    }
}

struct HwQueueContext {
    page: Option<KBox<disk_storage::NullBlockPage>>,
    preload: xarray::XArrayPreloadBuffer,
    poll_queue: kernel::ringbuffer::RingBuffer<Owned<mq::Request<NullBlkDevice>>>,
}

#[pin_data]
struct Pdu {
    #[pin]
    timer: HrTimer<Self>,
    error: Atomic<u32>,
    fake_timeout: Atomic<u32>,
}

impl HrTimerCallback for Pdu {
    type Pointer<'a> = ARef<mq::Request<NullBlkDevice>>;

    fn run(this: Self::Pointer<'_>, _context: HrTimerCallbackContext<'_, Self>) -> HrTimerRestart {
        OwnableRefCounted::try_from_shared(this)
            .map_err(|_e| kernel::error::code::EIO)
            .expect("Failed to complete request")
            .end_ok();
        HrTimerRestart::NoRestart
    }
}

kernel::impl_has_hr_timer! {
    impl HasHrTimer<Self> for Pdu {
        mode: kernel::time::hrtimer::RelativeMode<kernel::time::Monotonic>,
        field: self.timer,
    }
}

struct NullBlkTagsetData {
    queue_depth: u32,
    queue_config: Arc<Mutex<QueueConfig>>,
    #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
    init_hctx_inject: Arc<FaultConfig>,
}

#[vtable]
impl Operations for NullBlkDevice {
    type QueueData = Arc<Self>;
    type RequestData = Pdu;
    type TagSetData = KBox<NullBlkTagsetData>;
    type HwData = Pin<KBox<SpinLock<HwQueueContext>>>;

    fn new_request_data() -> impl PinInit<Self::RequestData> {
        pin_init!(Pdu {
            timer <- HrTimer::new(),
            error: Atomic::new(0),
            fake_timeout: Atomic::new(0),
        })
    }

    fn queue_rq(
        hw_data: Pin<&SpinLock<HwQueueContext>>,
        this: ArcBorrow<'_, Self>,
        rq: Owned<mq::IdleRequest<Self>>,
        is_last: bool,
    ) -> BlkResult {
        Ok(Self::queue_rq_internal(hw_data, this, rq, is_last)?)
    }

    fn queue_rqs(
        hw_data: Pin<&SpinLock<HwQueueContext>>,
        this: ArcBorrow<'_, Self>,
        requests: &mut RequestList<Self>,
    ) {
        let mut requeue = RequestList::new();
        while let Some(request) = requests.pop() {
            match Self::queue_rq_internal(hw_data, this, request, false) {
                Err(QueueRequestError { request, error }) => {
                    requeue.push_tail(request);
                }
                Ok(()) => (),
            }
        }

        drop(core::mem::replace(requests, requeue));
    }

    fn commit_rqs(_hw_data: Pin<&SpinLock<HwQueueContext>>, _queue_data: ArcBorrow<'_, Self>) {}

    fn poll(
        hw_data: Pin<&SpinLock<HwQueueContext>>,
        _this: ArcBorrow<'_, Self>,
        batch: &mut IoCompletionBatch<Self>,
    ) -> Result<bool> {
        let mut guard = hw_data.lock();
        let mut completed = false;

        while let Some(rq) = guard.poll_queue.pop_tail() {
            let status = rq.data_ref().error.load(ordering::Relaxed);
            rq.data_ref().error.store(0, ordering::Relaxed);

            if let Err(rq) = batch.add_request(rq, status != 0) {
                Self::end_request(rq);
            }

            completed = true;
        }

        Ok(completed)
    }

    fn init_hctx(tagset_data: &NullBlkTagsetData, _hctx_idx: u32) -> Result<Self::HwData> {
        #[cfg(CONFIG_BLK_DEV_RUST_NULL_FAULT_INJECTION)]
        if tagset_data.init_hctx_inject.should_fail(1) {
            return Err(EFAULT);
        }

        KBox::pin_init(
            new_spinlock!(HwQueueContext {
                page: None,
                preload: xarray::XArrayPreloadBuffer::new(2)?,
                poll_queue: kernel::ringbuffer::RingBuffer::new(
                    tagset_data.queue_depth.try_into()?
                )?,
            }),
            GFP_KERNEL,
        )
    }

    fn complete(rq: ARef<mq::Request<Self>>) {
        Self::end_request(
            OwnableRefCounted::try_from_shared(rq)
                .map_err(|_e| kernel::error::code::EIO)
                .expect("Failed to complete request"),
        )
    }

    #[cfg(CONFIG_BLK_DEV_ZONED)]
    fn report_zones(
        disk: &GenDiskRef<Self>,
        sector: u64,
        nr_zones: u32,
        callback: impl Fn(*mut bindings::blk_zone, u32) -> Result,
    ) -> Result<u32> {
        Self::report_zones_internal(disk, sector, nr_zones, callback)
    }

    fn map_queues(tag_set: Pin<&mut TagSet<Self>>) {
        let queue_config = tag_set.data().queue_config.lock();
        let mut submit_queue_count = queue_config.submit_queues;
        let mut poll_queue_count = queue_config.poll_queues;
        drop(queue_config);

        if tag_set.hw_queue_count() != submit_queue_count + poll_queue_count {
            pr_warn!(
                "tag set has unexpected hardware queue count: {}\n",
                tag_set.hw_queue_count()
            );
            submit_queue_count = 1;
            poll_queue_count = 0;
        }

        let mut offset = 0;
        tag_set
            .update_maps(|mut qmap| {
                use mq::QueueType::*;
                let queue_count = match qmap.kind() {
                    Default => submit_queue_count,
                    Read => 0,
                    Poll => poll_queue_count,
                };
                qmap.set_queue_count(queue_count);
                qmap.set_offset(offset);
                offset += queue_count;
                qmap.map_queues();
            })
            .unwrap()
    }

    fn request_timeout(tag_set: &TagSet<Self>, qid: u32, tag: u32) -> RequestTimeoutStatus {
        if let Some(request) = tag_set.tag_to_rq(qid, tag) {
            pr_info!("Request timed out\n");
            // Only fail requests that are faking timeouts. Requests that time
            // out due to memory pressure will be completed normally.
            if request.data_ref().fake_timeout.load(ordering::Relaxed) != 0 {
                request.data_ref().error.store(
                    block::error::code::BLK_STS_TIMEOUT.into(),
                    ordering::Relaxed,
                );
                request.data_ref().fake_timeout.store(0, ordering::Relaxed);

                if let Ok(request) = OwnableRefCounted::try_from_shared(request) {
                    Self::end_request(request);
                    return RequestTimeoutStatus::Completed;
                }
                // TODO: pr_warn_once!
                pr_warn!("Timed out request could not be completed\n");
            }
        } else {
            pr_warn!("Timed out request referenced in timeout handler\n");
        }
        RequestTimeoutStatus::RetryLater
    }
}

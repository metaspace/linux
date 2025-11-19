// SPDX-License-Identifier: GPL-2.0

//! This is a Rust implementation of the C null block driver.

mod configfs;
mod disk_storage;

use configfs::IRQMode;
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
        error::BlkResult,
        mq::{
            self,
            gen_disk::{
                self,
                GenDisk,
                GenDiskRef, //
            },
            Operations,
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
    pr_info,
    prelude::*,
    revocable::Revocable,
    str::CString,
    sync::{
        aref::ARef,
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
        OwnableRefCounted,
        Owned, //
    },
    xarray::XArraySheaf, //
};
use pin_init::PinInit;

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
            description:
            "Set the rotational feature for the device (0 for false, 1 for true). Default: 0",
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
                description:
            "Time in ns to complete a request in hardware. Default: 10,000ns",
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
            description:
        "Use per-node allocation for hardware context queues, 0-false, 1-true. Default: 0-false",
        },
        home_node: i32 {
            default: -1,
            description: "Home node for the device. Default: -1 (no node)",
        },
        discard: u8 {
            default: 0,
            description:
            "Support discard operations (requires memory-backed null_blk device). Default: false",
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
    },
}

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

                let block_size = *module_parameters::bs.value();
                let disk = NullBlkDevice::new(NullBlkOptions {
                    name: &name,
                    block_size,
                    rotational: *module_parameters::rotational.value() != 0,
                    capacity_mib: *module_parameters::gb.value() * 1024,
                    irq_mode: (*module_parameters::irqmode.value()).try_into()?,
                    completion_time: Delta::from_nanos(completion_time),
                    memory_backed: *module_parameters::memory_backed.value() != 0,
                    submit_queues,
                    home_node: *module_parameters::home_node.value(),
                    discard: *module_parameters::discard.value() != 0,
                    no_sched: *module_parameters::no_sched.value() != 0,
                    bad_blocks: Arc::pin_init(BadBlocks::new(false), GFP_KERNEL)?,
                    bad_blocks_once: false,
                    bad_blocks_partial_io: false,
                    storage: Arc::pin_init(DiskStorage::new(0, block_size as usize), GFP_KERNEL)?,
                    bandwidth_limit: u64::from(*module_parameters::mbps.value()) * 2u64.pow(20),
                    blocking: *module_parameters::blocking.value() != 0,
                    shared_tags: *module_parameters::shared_tags.value() != 0,
                    hw_queue_depth: *module_parameters::hw_queue_depth.value(),
                })?;
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

struct NullBlkOptions<'a> {
    name: &'a CStr,
    block_size: u32,
    rotational: bool,
    capacity_mib: u64,
    irq_mode: IRQMode,
    completion_time: Delta,
    memory_backed: bool,
    submit_queues: u32,
    home_node: i32,
    discard: bool,
    no_sched: bool,
    bad_blocks: Arc<BadBlocks>,
    bad_blocks_once: bool,
    bad_blocks_partial_io: bool,
    storage: Arc<DiskStorage>,
    bandwidth_limit: u64,
    blocking: bool,
    shared_tags: bool,
    hw_queue_depth: u32,
}

static SHARED_TAG_SET: SetOnce<Arc<TagSet<NullBlkDevice>>> = SetOnce::new();

#[pin_data]
struct NullBlkDevice {
    storage: Arc<DiskStorage>,
    irq_mode: IRQMode,
    completion_time: Delta,
    memory_backed: bool,
    block_size: usize,
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
}

impl NullBlkDevice {
    const BANDWIDTH_TIMER_INTERVAL: Delta = Delta::from_millis(20);

    fn new(options: NullBlkOptions<'_>) -> Result<Arc<GenDisk<Self>>> {
        let NullBlkOptions {
            name,
            block_size,
            rotational,
            capacity_mib,
            irq_mode,
            completion_time,
            memory_backed,
            submit_queues,
            home_node,
            discard,
            no_sched,
            bad_blocks,
            bad_blocks_once,
            bad_blocks_partial_io,
            storage,
            bandwidth_limit,
            blocking,
            shared_tags,
            hw_queue_depth,
        } = options;

        let mut flags = mq::tag_set::Flags::default();

        // TODO: lim.features |= BLK_FEAT_WRITE_CACHE;
        // if (dev->fua)
        // 	lim.features |= BLK_FEAT_FUA;
        if blocking || memory_backed {
            flags |= mq::tag_set::Flag::Blocking;
        }

        if no_sched {
            flags |= mq::tag_set::Flag::NoDefaultScheduler;
        }

        if home_node > kernel::num_online_nodes().try_into()? {
            return Err(code::EINVAL);
        }

        let tagset_ctor = || -> Result<Arc<_>> {
            Arc::pin_init(
                TagSet::new(submit_queues, (), hw_queue_depth, 1, home_node, flags),
                GFP_KERNEL,
            )
        };

        let tagset = if shared_tags {
            SHARED_TAG_SET.as_ref_or_populate_with(tagset_ctor)?.clone()
        } else {
            tagset_ctor()?
        };

        let queue_data = Arc::try_pin_init(
            try_pin_init!(Self {
                storage,
                irq_mode,
                completion_time,
                memory_backed,
                block_size: block_size as usize,
                bad_blocks,
                bad_blocks_once,
                bad_blocks_partial_io,
                bandwidth_limit: bandwidth_limit / 50,
                bandwidth_timer <- HrTimer::new(),
                bandwidth_bytes: Atomic::new(0),
                bandwidth_timer_handle <- new_spinlock!(None),
                disk: SetOnce::new(),
            }),
            GFP_KERNEL,
        )?;

        let mut builder = gen_disk::GenDiskBuilder::new()
            .capacity_sectors(capacity_mib << (20 - block::SECTOR_SHIFT))
            .logical_block_size(block_size)?
            .physical_block_size(block_size)?
            .rotational(rotational);

        if memory_backed && discard {
            builder = builder
                // Max IO size is u32::MAX bytes
                .max_hw_discard_sectors(ffi::c_uint::MAX >> block::SECTOR_SHIFT);
        }

        let disk = builder.build(fmt!("{}", name.to_str()?), tagset, queue_data)?;
        let queue_data: ArcBorrow<'_, Self> = disk.queue_data();
        queue_data.disk.populate(disk.get_ref());
        Ok(disk)
    }

    fn sheaf_size() -> usize {
        2 * ((usize::BITS as usize / bindings::XA_CHUNK_SHIFT)
            + if (usize::BITS as usize % bindings::XA_CHUNK_SHIFT) == 0 {
                0
            } else {
                1
            })
    }

    fn preload<'b, 'c>(
        tree_guard: &'b mut SpinLockGuard<'c, Pin<KBox<TreeContainer>>>,
        hw_data_guard: &'b mut SpinLockGuard<'c, HwQueueContext>,
        block_size: usize,
    ) -> Result {
        if hw_data_guard.page.is_none() {
            hw_data_guard.page =
                Some(tree_guard.do_unlocked(|| {
                    hw_data_guard.do_unlocked(|| NullBlockPage::new(block_size))
                })?);
        }

        Ok(())
    }

    #[inline(always)]
    fn write<'a, 'b, 'c>(
        &'a self,
        tree_guard: &'b mut SpinLockGuard<'c, Pin<KBox<TreeContainer>>>,
        hw_data_guard: &'b mut SpinLockGuard<'c, HwQueueContext>,
        mut sector: u64,
        mut segment: Segment<'_>,
    ) -> Result {
        let mut sheaf: Option<XArraySheaf<'_>> = None;

        while !segment.is_empty() {
            Self::preload(tree_guard, hw_data_guard, self.block_size)?;

            match &mut sheaf {
                Some(sheaf) => {
                    tree_guard.do_unlocked(|| {
                        hw_data_guard.do_unlocked(|| sheaf.refill(GFP_KERNEL, Self::sheaf_size()))
                    })?;
                }
                None => {
                    let _ = sheaf.insert(
                        kernel::xarray::xarray_kmem_cache()
                            .sheaf(Self::sheaf_size(), GFP_NOWAIT)
                            .or(tree_guard.do_unlocked(|| {
                                hw_data_guard.do_unlocked(|| -> Result<_> {
                                    kernel::xarray::xarray_kmem_cache()
                                        .sheaf(Self::sheaf_size(), GFP_KERNEL)
                                })
                            }))?,
                    );
                }
            }

            let mut access = self.storage.access(tree_guard, hw_data_guard, sheaf);

            let page = access.get_write_page(sector)?;
            page.set_occupied(sector);
            let page_offset = (sector & u64::from(block::SECTOR_MASK)) << block::SECTOR_SHIFT;

            sector += segment.copy_to_page(page.page_mut().get_pin_mut(), page_offset as usize)
                as u64
                >> block::SECTOR_SHIFT;

            sheaf = access.sheaf;
        }

        if let Some(sheaf) = sheaf {
            tree_guard.do_unlocked(|| {
                hw_data_guard.do_unlocked(|| {
                    sheaf.return_refill(GFP_KERNEL);
                })
            });
        }

        Ok(())
    }

    #[inline(always)]
    fn read<'a, 'b, 'c>(
        &'a self,
        tree_guard: &'b mut SpinLockGuard<'c, Pin<KBox<TreeContainer>>>,
        hw_data_guard: &'b mut SpinLockGuard<'c, HwQueueContext>,
        mut sector: u64,
        mut segment: Segment<'_>,
    ) -> Result {
        let access = self.storage.access(tree_guard, hw_data_guard, None);

        while !segment.is_empty() {
            let page = access.get_read_page(sector);

            match page {
                Some(page) => {
                    let page_offset =
                        (sector & u64::from(block::SECTOR_MASK)) << block::SECTOR_SHIFT;
                    sector += segment.copy_from_page(page.page(), page_offset as usize) as u64
                        >> block::SECTOR_SHIFT;
                }
                None => sector += segment.zero_page() as u64 >> block::SECTOR_SHIFT,
            }
        }

        Ok(())
    }

    fn discard(
        &self,
        hw_data: &Pin<&SpinLock<HwQueueContext>>,
        mut sector: u64,
        sectors: u32,
    ) -> Result {
        let mut tree_guard = self.storage.lock();
        let mut hw_data_guard = hw_data.lock();

        let mut access = self
            .storage
            .access(&mut tree_guard, &mut hw_data_guard, None);

        let mut remaining_bytes = (sectors as usize) << SECTOR_SHIFT;

        while remaining_bytes > 0 {
            access.free_sector(sector);
            let processed = remaining_bytes.min(self.block_size);
            sector += (processed >> SECTOR_SHIFT) as u64;
            remaining_bytes -= processed;
        }

        Ok(())
    }

    #[inline(never)]
    fn transfer(
        &self,
        hw_data: &Pin<&SpinLock<HwQueueContext>>,
        rq: &mut Owned<mq::Request<Self>>,
        sectors: u32,
    ) -> Result {
        let mut sector = rq.sector();
        let end_sector = sector + <u32 as Into<u64>>::into(sectors);
        let command = rq.command();

        // TODO: Use `PerCpu` to get rid of this lock
        let mut hw_data_guard = hw_data.lock();
        let mut tree_guard = self.storage.lock();

        for bio in rq.bio_iter_mut() {
            let segment_iter = bio.segment_iter();
            for segment in segment_iter {
                // Length might be limited by bad blocks.
                let length = segment
                    .len()
                    .min((end_sector - sector) as u32 >> SECTOR_SHIFT);
                match command {
                    bindings::req_op_REQ_OP_WRITE => {
                        self.write(&mut tree_guard, &mut hw_data_guard, sector, segment)?
                    }
                    bindings::req_op_REQ_OP_READ => {
                        self.read(&mut tree_guard, &mut hw_data_guard, sector, segment)?
                    }
                    _ => (),
                }
                sector += u64::from(length) >> SECTOR_SHIFT;

                if sector >= end_sector {
                    return Ok(());
                }
            }
        }

        Ok(())
    }

    fn handle_bad_blocks(&self, rq: &mut Owned<mq::Request<Self>>, sectors: &mut u32) -> Result {
        if self.bad_blocks.enabled() {
            let start = rq.sector();
            let end = start + u64::from(*sectors);
            match self.bad_blocks.check(start..end) {
                badblocks::BlockStatus::None => {}
                badblocks::BlockStatus::Acknowledged(mut range)
                | badblocks::BlockStatus::Unacknowledged(mut range) => {
                    rq.data_ref().error.store(1, ordering::Relaxed);

                    if self.bad_blocks_once {
                        self.bad_blocks.set_good(range.clone())?;
                    }

                    if self.bad_blocks_partial_io {
                        let block_size_sectors = (self.block_size >> SECTOR_SHIFT) as u64;
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

        match status {
            0 => rq.end_ok(),
            _ => rq.end(bindings::BLK_STS_IOERR),
        }
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
}

#[pin_data]
struct Pdu {
    #[pin]
    timer: HrTimer<Self>,
    error: Atomic<u32>,
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

fn is_power_of_two<T>(value: T) -> bool
where
    T: core::ops::Sub<T, Output = T>,
    T: core::ops::BitAnd<Output = T>,
    T: core::cmp::PartialOrd<T>,
    T: Copy,
    T: From<u8>,
{
    (value > 0u8.into()) && (value & (value - 1u8.into())) == 0u8.into()
}

fn align_down<T>(value: T, to: T) -> T
where
    T: core::ops::Sub<T, Output = T>,
    T: core::ops::Not<Output = T>,
    T: core::ops::BitAnd<Output = T>,
    T: core::cmp::PartialOrd<T>,
    T: Copy,
    T: From<u8>,
{
    debug_assert!(is_power_of_two(to));
    value & !(to - 1u8.into())
}

#[vtable]
impl Operations for NullBlkDevice {
    type QueueData = Arc<Self>;
    type RequestData = Pdu;
    type TagSetData = ();
    type HwData = Pin<KBox<SpinLock<HwQueueContext>>>;

    fn new_request_data() -> impl PinInit<Self::RequestData> {
        pin_init!(Pdu {
            timer <- HrTimer::new(),
            error: Atomic::new(0),
        })
    }

    #[inline(always)]
    fn queue_rq(
        hw_data: Pin<&SpinLock<HwQueueContext>>,
        this: ArcBorrow<'_, Self>,
        rq: Owned<mq::IdleRequest<Self>>,
        _is_last: bool,
    ) -> BlkResult {
        let mut sectors = rq.sectors();

        if this.bandwidth_limit != 0 {
            if !this.bandwidth_timer.active() {
                drop(this.bandwidth_timer_handle.lock().take());
                let arc: Arc<_> = this.into();
                *this.bandwidth_timer_handle.lock() =
                    Some(arc.start(Self::BANDWIDTH_TIMER_INTERVAL));
            }

            if this
                .bandwidth_bytes
                .fetch_add(u64::from(rq.bytes()), ordering::Relaxed)
                + u64::from(rq.bytes())
                > this.bandwidth_limit
            {
                rq.queue().stop_hw_queues();
                if this.bandwidth_bytes.load(ordering::Relaxed) <= this.bandwidth_limit {
                    rq.queue().start_stopped_hw_queues_async();
                }

                return Err(kernel::block::error::code::BLK_STS_DEV_RESOURCE);
            }
        }

        let mut rq = rq.start();

        use core::ops::Deref;
        Self::handle_bad_blocks(this.deref(), &mut rq, &mut sectors)?;

        if this.memory_backed {
            if rq.command() == bindings::req_op_REQ_OP_DISCARD {
                this.discard(&hw_data, rq.sector(), sectors)?;
            } else {
                this.transfer(&hw_data, &mut rq, sectors)?;
            }
        }

        match this.irq_mode {
            IRQMode::None => Self::end_request(rq),
            IRQMode::Soft => mq::Request::complete(rq.into()),
            IRQMode::Timer => {
                OwnableRefCounted::into_shared(rq)
                    .start(this.completion_time)
                    .dismiss();
            }
        }
        Ok(())
    }

    fn commit_rqs(_hw_data: Pin<&SpinLock<HwQueueContext>>, _queue_data: ArcBorrow<'_, Self>) {}

    fn init_hctx(_tagset_data: (), _hctx_idx: u32) -> Result<Self::HwData> {
        KBox::pin_init(new_spinlock!(HwQueueContext { page: None }), GFP_KERNEL)
    }

    fn complete(rq: ARef<mq::Request<Self>>) {
        Self::end_request(
            OwnableRefCounted::try_from_shared(rq)
                .map_err(|_e| kernel::error::code::EIO)
                .expect("Failed to complete request"),
        )
    }
}

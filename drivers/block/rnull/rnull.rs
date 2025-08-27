// SPDX-License-Identifier: GPL-2.0

//! This is a Rust implementation of the C null block driver.

mod configfs;
mod disk_storage;

use configfs::IRQMode;
use core::option::Option::Some;
use disk_storage::DiskStorage;
use disk_storage::NullBlockPage;
use disk_storage::TreeContainer;
use kernel::{
    bindings,
    block::{
        self,
        badblocks::{self, BadBlocks},
        bio::Segment,
        mq::{
            self,
            gen_disk::{self, GenDisk},
            Operations, TagSet,
        },
        SECTOR_SHIFT,
    },
    error::{code, Result},
    ffi, new_mutex, new_spinlock,
    prelude::*,
    str::CString,
    sync::{
        atomic::{ordering, Atomic},
        Arc, Mutex, SpinLock, SpinLockGuard,
    },
    time::{
        hrtimer::{HrTimerCallback, HrTimerPointer, HrTimerRestart},
        Delta,
    },
    types::{ARef, BorrowIterator, OwnableRefCounted, Owned},
    xarray::{self},
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
    },
}

#[pin_data]
struct NullBlkModule {
    #[pin]
    configfs_subsystem: kernel::configfs::Subsystem<configfs::Config>,
    #[pin]
    param_disks: Mutex<KVec<GenDisk<NullBlkDevice>>>,
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
                let disk = NullBlkDevice::new(
                    &name,
                    block_size,
                    *module_parameters::rotational.value() != 0,
                    *module_parameters::gb.value() * 1024,
                    (*module_parameters::irqmode.value()).try_into()?,
                    Delta::from_nanos(completion_time),
                    *module_parameters::memory_backed.value() != 0,
                    submit_queues,
                    *module_parameters::home_node.value(),
                    *module_parameters::discard.value() != 0,
                    *module_parameters::no_sched.value() != 0,
                    Arc::pin_init(BadBlocks::new(false), GFP_KERNEL)?,
                    false,
                    false,
                    Arc::pin_init(DiskStorage::new(0, block_size as usize), GFP_KERNEL)?,
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
}

impl NullBlkDevice {
    // TODO: Change to "attach"
    fn new(
        name: &CStr,
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
    ) -> Result<GenDisk<Self>> {
        let mut flags = mq::Flags::default();

        // TODO: lim.features |= BLK_FEAT_WRITE_CACHE;
        // if (dev->fua)
        // 	lim.features |= BLK_FEAT_FUA;
        if memory_backed {
            flags |= mq::Flags::BLOCKING;
        }

        if no_sched {
            flags |= mq::Flags::NO_DEFAULT_SCHEDULER;
        }

        if home_node > kernel::num_online_nodes().try_into()? {
            return Err(code::EINVAL);
        }

        let tagset = Arc::pin_init(
            TagSet::new(submit_queues, (), 256, 1, home_node, flags),
            GFP_KERNEL,
        )?;

        let queue_data = Box::try_pin_init(
            try_pin_init!(Self {
                storage,
                irq_mode,
                completion_time,
                memory_backed,
                block_size: block_size as usize,
                bad_blocks,
                bad_blocks_once,
                bad_blocks_partial_io,
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

        builder.build(fmt!("{}", name.to_str()?), tagset, queue_data)
    }

    fn preload<'b, 'c>(
        tree_guard: &'b mut SpinLockGuard<'c, Pin<KBox<TreeContainer>>>,
        hw_data_guard: &'b mut SpinLockGuard<'c, HwQueueContext>,
        block_size: usize,
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
        mut tree_guard: &'b mut SpinLockGuard<'c, Pin<KBox<TreeContainer>>>,
        mut hw_data_guard: &'b mut SpinLockGuard<'c, HwQueueContext>,
        mut sector: u64,
        mut segment: Segment<'_>,
    ) -> Result {
        while !segment.is_empty() {
            Self::preload(&mut tree_guard, &mut hw_data_guard, self.block_size)?;

            let mut access = self.storage.access(&mut tree_guard, &mut hw_data_guard);
            let page = access.get_write_page(sector)?;
            page.set_occupied(sector);
            let page_offset = (sector & block::SECTOR_MASK as u64) << block::SECTOR_SHIFT;
            sector += segment.copy_to_page(page.page_mut().get_pin_mut(), page_offset as usize)
                as u64
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

        let mut access = self.storage.access(&mut tree_guard, &mut hw_data_guard);

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
            let mut segment_iter = bio.segment_iter();
            while let Some(segment) = segment_iter.next() {
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
                sector += length as u64 >> SECTOR_SHIFT;

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
            let end = start + *sectors as u64;
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

struct HwQueueContext {
    page: Option<KBox<disk_storage::NullBlockPage>>,
    preload: xarray::XArrayPreloadBuffer,
}

#[pin_data]
struct Pdu {
    #[pin]
    timer: kernel::time::hrtimer::HrTimer<Self>,
    error: Atomic<u32>,
}

impl HrTimerCallback for Pdu {
    type Pointer<'a> = ARef<mq::Request<NullBlkDevice>>;

    fn run(this: Self::Pointer<'_>) -> HrTimerRestart {
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
    type QueueData = Pin<KBox<Self>>;
    type RequestData = Pdu;
    type TagSetData = ();
    type HwData = Pin<KBox<SpinLock<HwQueueContext>>>;

    fn new_request_data() -> impl PinInit<Self::RequestData> {
        pin_init!(Pdu {
            timer <- kernel::time::hrtimer::HrTimer::new(),
            error: Atomic::new(0),
        })
    }

    #[inline(always)]
    fn queue_rq(
        hw_data: Pin<&SpinLock<HwQueueContext>>,
        this: Pin<&Self>,
        mut rq: Owned<mq::Request<Self>>,
        _is_last: bool,
    ) -> Result {
        let mut sectors = rq.sectors();

        Self::handle_bad_blocks(this.get_ref(), &mut rq, &mut sectors)?;

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

    fn commit_rqs(_hw_data: Pin<&SpinLock<HwQueueContext>>, _queue_data: Pin<&Self>) {}

    fn init_hctx(_tagset_data: (), _hctx_idx: u32) -> Result<Self::HwData> {
        KBox::pin_init(
            new_spinlock!(HwQueueContext {
                page: None,
                preload: xarray::XArrayPreloadBuffer::new(2)?
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
}

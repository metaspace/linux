// SPDX-License-Identifier: GPL-2.0

//! This is a Rust implementation of the C null block driver.

mod configfs;

use configfs::IRQMode;
use core::ops::Deref;
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
        SECTOR_MASK, SECTOR_SHIFT,
    },
    error::{code, Result},
    ffi, new_mutex, new_spinlock,
    page::{Page, PAGE_SIZE},
    prelude::*,
    str::CString,
    sync::{
        atomic::{ordering, Atomic},
        Arc, Mutex, SpinLock,
    },
    time::{
        hrtimer::{HrTimerCallback, HrTimerPointer, HrTimerRestart},
        Delta,
    },
    types::{ARef, BorrowIterator, OwnableRefCounted, Owned},
    xarray::{self, XArray},
    CacheAligned,
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

                let disk = NullBlkDevice::new(
                    &name,
                    *module_parameters::bs.value(),
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
                    false,
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

struct NullBlkDevice;

impl NullBlkDevice {
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
        outer_lock: bool,
    ) -> Result<GenDisk<Self>> {
        let mut flags = mq::Flags::default();

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
            try_pin_init!(
                QueueData {
                    tree <- TreeContainer::new(),
                    irq_mode,
                    completion_time,
                    memory_backed,
                    block_size: block_size as usize,
                    outer_lock,
                    bad_blocks,
                    bad_blocks_once,
                    bad_blocks_partial_io,
                }
            ),
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

    #[inline(always)]
    fn write(
        tree: &mut xarray::Guard<'_, TreeNode>,
        mut sector: u64,
        mut segment: Segment<'_>,
    ) -> Result {
        while !segment.is_empty() {
            let page_idx = sector >> block::PAGE_SECTORS_SHIFT;

            let page = if let Some(page) = tree.get_mut(page_idx as usize) {
                page
            } else {
                let page = tree.do_unlocked(|| NullBlockPage::new())?;
                tree.store(page_idx as usize, page, GFP_NOIO)?;
                tree.get_mut(page_idx as usize).unwrap()
            };

            page.set_occupied(sector);
            let page_offset = (sector & block::SECTOR_MASK as u64) << block::SECTOR_SHIFT;
            sector += segment.copy_to_page(page.page.get_pin_mut(), page_offset as usize) as u64
                >> block::SECTOR_SHIFT;
        }
        Ok(())
    }

    #[inline(always)]
    fn read(
        tree: &xarray::Guard<'_, TreeNode>,
        mut sector: u64,
        mut segment: Segment<'_>,
    ) -> Result {
        while !segment.is_empty() {
            let idx = sector >> block::PAGE_SECTORS_SHIFT;

            if let Some(page) = tree.get(idx as usize) {
                let page_offset = (sector & block::SECTOR_MASK as u64) << block::SECTOR_SHIFT;
                sector += segment.copy_from_page(&page.page, page_offset as usize) as u64
                    >> block::SECTOR_SHIFT;
            } else {
                sector += segment.zero_page() as u64 >> block::SECTOR_SHIFT;
            }
        }

        Ok(())
    }

    fn discard(
        tree: &mut xarray::Guard<'_, TreeNode>,
        mut sector: u64,
        sectors: u32,
        block_size: usize,
    ) -> Result {
        let mut remaining_bytes = (sectors as usize) << SECTOR_SHIFT;

        while remaining_bytes > 0 {
            let page_idx = sector >> block::PAGE_SECTORS_SHIFT;
            let mut remove = false;
            // TODO: XArray location handle
            if let Some(page) = tree.get_mut(page_idx as usize) {
                page.set_free(sector);
                if page.is_empty() {
                    remove = true;
                }
            }

            if remove {
                drop(tree.remove(page_idx as usize))
            }

            let processed = remaining_bytes.min(block_size);
            sector += (processed >> SECTOR_SHIFT) as u64;
            remaining_bytes -= processed;
        }

        Ok(())
    }

    #[inline(never)]
    fn transfer(
        rq: &mut Owned<mq::Request<Self>>,
        tree: &mut xarray::Guard<'_, TreeNode>,
        sectors: u32,
    ) -> Result {
        let mut sector = rq.sector();
        let end_sector = sector + <u32 as Into<u64>>::into(sectors);
        let command = rq.command();

        for bio in rq.bio_iter_mut() {
            let mut segment_iter = bio.segment_iter();
            while let Some(segment) = segment_iter.next() {
                // Length might be limited by bad blocks.
                let length = segment
                    .len()
                    .min((sector - end_sector) as u32 >> SECTOR_SHIFT);
                match command {
                    bindings::req_op_REQ_OP_WRITE => Self::write(tree, sector, segment)?,
                    bindings::req_op_REQ_OP_READ => Self::read(tree, sector, segment)?,
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

    fn handle_bad_blocks(
        rq: &mut Owned<mq::Request<Self>>,
        queue_data: &QueueData,
        sectors: &mut u32,
    ) -> Result {
        if queue_data.bad_blocks.enabled() {
            let start = rq.sector();
            let end = start + *sectors as u64;
            match queue_data.bad_blocks.check(start..end) {
                badblocks::BlockStatus::None => {}
                badblocks::BlockStatus::Acknowledged(mut range)
                | badblocks::BlockStatus::Unacknowledged(mut range) => {
                    rq.data_ref().error.store(1, ordering::Relaxed);

                    if queue_data.bad_blocks_once {
                        queue_data.bad_blocks.set_good(range.clone())?;
                    }

                    if queue_data.bad_blocks_partial_io {
                        let block_size_sectors = (queue_data.block_size >> SECTOR_SHIFT) as u64;
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

const _CHEKC_STATUS_WIDTH: () = build_assert!((PAGE_SIZE >> SECTOR_SHIFT) <= 64);

struct NullBlockPage {
    page: Owned<Page>,
    status: u64,
}

impl NullBlockPage {
    fn new() -> Result<KBox<Self>> {
        Ok(KBox::new(
            Self {
                page: Page::alloc_page(GFP_NOIO | __GFP_ZERO)?,
                status: 0,
            },
            GFP_NOIO,
        )?)
    }

    fn set_occupied(&mut self, sector: u64) {
        let idx = sector & SECTOR_MASK as u64;
        self.status |= 1 << idx;
    }

    fn set_free(&mut self, sector: u64) {
        let idx = sector & SECTOR_MASK as u64;
        self.status &= !(1 << idx);
    }

    fn is_empty(&self) -> bool {
        self.status == 0
    }
}

type TreeNode = KBox<NullBlockPage>;
type Tree = XArray<TreeNode>;

#[pin_data]
struct TreeContainer {
    // `XArray` is safe to use without a lock, as it applies internal locking.
    // However, there are two reasons to use an external lock: a) cache line
    // contention and b) we don't want to take the lock for each page we
    // process.
    //
    // A: The `XArray` lock (xa_lock) is located on the same cache line as the
    // xarray data pointer (xa_head). The effect of this arrangement is that
    // under heavy contention, we often get a cache miss when we try to follow
    // the data pointer after acquiring the lock. We would rather have consumers
    // spinning on another lock, so we do not get a miss on xa_head. This issue
    // can potentially be fixed by padding the C `struct xarray`.
    //
    // B: The current `XArray` Rust API requires that we take the `xa_lock` for
    // each `XArray` operation. This is very inefficient when the lock is
    // contended and we have many operations to perform. Eventually we should
    // update the `XArray` API to allow multiple tree operations under a single
    // lock acquisition. For now, serialize tree access with an external lock.
    #[pin]
    tree: CacheAligned<Tree>,
    #[pin]
    lock: CacheAligned<SpinLock<()>>,
}

impl TreeContainer {
    fn new() -> impl PinInit<Self> {
        pin_init!(TreeContainer {
            tree <- CacheAligned::new_initializer(XArray::new(kernel::xarray::AllocKind::Alloc)),
            lock <- CacheAligned::new_initializer(new_spinlock!((), "rnullb:mem")),
        })
    }
}

#[pin_data]
struct QueueData {
    #[pin]
    tree: TreeContainer,
    irq_mode: IRQMode,
    completion_time: Delta,
    memory_backed: bool,
    block_size: usize,
    outer_lock: bool,
    bad_blocks: Arc<BadBlocks>,
    bad_blocks_once: bool,
    bad_blocks_partial_io: bool,
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
    type QueueData = Pin<KBox<QueueData>>;
    type RequestData = Pdu;
    type TagSetData = ();

    fn new_request_data() -> impl PinInit<Self::RequestData> {
        pin_init!(Pdu {
            timer <- kernel::time::hrtimer::HrTimer::new(),
            error: Atomic::new(0),
        })
    }

    #[inline(always)]
    fn queue_rq(
        queue_data: Pin<&QueueData>,
        mut rq: Owned<mq::Request<Self>>,
        _is_last: bool,
    ) -> Result {
        let mut sectors = rq.sectors();

        Self::handle_bad_blocks(&mut rq, queue_data.get_ref(), &mut sectors)?;

        if queue_data.memory_backed {
            let outer_guard = if queue_data.outer_lock {
                Some(queue_data.tree.lock.lock())
            } else {
                None
            };

            let tree = queue_data.tree.tree.deref();
            let mut guard = tree.lock();

            if rq.command() == bindings::req_op_REQ_OP_DISCARD {
                Self::discard(&mut guard, rq.sector(), sectors, queue_data.block_size)?;
            } else {
                Self::transfer(&mut rq, &mut guard, sectors)?;
            }

            drop(guard);
            drop(outer_guard);
        }

        match queue_data.irq_mode {
            IRQMode::None => Self::end_request(rq),
            IRQMode::Soft => mq::Request::complete(rq.into()),
            IRQMode::Timer => {
                OwnableRefCounted::into_shared(rq)
                    .start(queue_data.completion_time)
                    .dismiss();
            }
        }
        Ok(())
    }

    fn commit_rqs(_queue_data: Pin<&QueueData>) {}

    fn complete(rq: ARef<mq::Request<Self>>) {
        Self::end_request(
            OwnableRefCounted::try_from_shared(rq)
                .map_err(|_e| kernel::error::code::EIO)
                .expect("Failed to complete request"),
        )
    }
}

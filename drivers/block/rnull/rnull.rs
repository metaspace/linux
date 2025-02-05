// SPDX-License-Identifier: GPL-2.0

//! This is a Rust implementation of the C null block driver.

mod configfs;

use configfs::IRQMode;
use core::ops::Deref;
use kernel::{
    alloc::{flags, KVec},
    bindings,
    block::{
        self,
        bio::Segment,
        mq::{
            self,
            gen_disk::{self, GenDisk},
            Operations, TagSet,
        },
    },
    error::Result,
    new_mutex, new_spinlock,
    page::Page,
    pr_info,
    prelude::*,
    str::CString,
    sync::{Arc, Mutex, SpinLock},
    time::{
        hrtimer::{HrTimerCallback, HrTimerPointer, HrTimerRestart},
        Ktime,
    },
    types::{ARef, BorrowIterator, OwnableRefCounted, Owned},
    xarray::XArray,
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
            let completion_time: i64 = (*module_parameters::completion_nsec.get()).try_into()?;
            for i in 0..(*module_parameters::nr_devices.get()) {
                let name = CString::try_from_fmt(fmt!("rnullb{}", i))?;
                let disk = NullBlkDevice::new(
                    &name,
                    *module_parameters::bs.get(),
                    *module_parameters::rotational.get() != 0,
                    *module_parameters::gb.get() * 1024,
                    (*module_parameters::irqmode.get()).try_into()?,
                    Ktime::from_nanos(completion_time),
                    *module_parameters::memory_backed.get() != 0,
                )?;
                disks.push(disk, flags::GFP_KERNEL)?;
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
        completion_time: Ktime,
        memory_backed: bool,
    ) -> Result<GenDisk<Self>> {
        let tagset = Arc::pin_init(TagSet::new(1, 256, 1), flags::GFP_KERNEL)?;

        let queue_data = Box::pin_init(
            pin_init!(
            QueueData {
                tree <- TreeContainer::new(),
                irq_mode,
                completion_time,
                memory_backed,
            }),
            flags::GFP_KERNEL,
        )?;

        gen_disk::GenDiskBuilder::new()
            .capacity_sectors(capacity_mib << (20 - block::SECTOR_SHIFT))
            .logical_block_size(block_size)?
            .physical_block_size(block_size)?
            .rotational(rotational)
            .build(fmt!("{}", name.to_str()?), tagset, queue_data)
    }

    #[inline(always)]
    fn write(tree: TreeRef<'_>, mut sector: usize, mut segment: Segment<'_>) -> Result {
        let mut guard = tree.lock();

        while !segment.is_empty() {
            let page_idx = sector >> block::PAGE_SECTORS_SHIFT;

            let page = if let Some(page) = guard.get_mut(page_idx) {
                page
            } else {
                guard.store(
                    page_idx,
                    Page::alloc_page(flags::GFP_NOIO | flags::__GFP_ZERO)?,
                    flags::GFP_KERNEL,
                )?;
                guard.get_mut(page_idx).unwrap()
            };

            let page_offset = (sector & block::SECTOR_MASK as usize) << block::SECTOR_SHIFT;
            sector += segment.copy_to_page(page, page_offset) >> block::SECTOR_SHIFT;
        }
        Ok(())
    }

    #[inline(always)]
    fn read(tree: TreeRef<'_>, mut sector: usize, mut segment: Segment<'_>) -> Result {
        let guard = tree.lock();

        while !segment.is_empty() {
            let idx = sector >> block::PAGE_SECTORS_SHIFT;

            if let Some(page) = guard.get(idx) {
                let page_offset = (sector & block::SECTOR_MASK as usize) << block::SECTOR_SHIFT;
                sector += segment.copy_from_page(page, page_offset) >> block::SECTOR_SHIFT;
            } else {
                sector += segment.zero_page() >> block::SECTOR_SHIFT;
            }
        }

        Ok(())
    }

    #[inline(never)]
    fn transfer(
        command: bindings::req_op,
        tree: TreeRef<'_>,
        sector: usize,
        segment: Segment<'_>,
    ) -> Result {
        match command {
            bindings::req_op_REQ_OP_WRITE => Self::write(tree, sector, segment)?,
            bindings::req_op_REQ_OP_READ => Self::read(tree, sector, segment)?,
            _ => (),
        }
        Ok(())
    }
}

type Tree = XArray<Owned<Page>>;
type TreeRef<'a> = &'a Tree;

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
    completion_time: Ktime,
    memory_backed: bool,
}

#[pin_data]
struct Pdu {
    #[pin]
    timer: kernel::time::hrtimer::HrTimer<Self>,
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
    impl HasHrTimer<Self> for Pdu { self.timer }
}

#[vtable]
impl Operations for NullBlkDevice {
    type QueueData = Pin<KBox<QueueData>>;
    type RequestData = Pdu;

    fn new_request_data() -> impl PinInit<Self::RequestData> {
        pin_init!(Pdu {
            timer <- kernel::time::hrtimer::HrTimer::new(kernel::time::hrtimer::HrTimerMode::Relative, kernel::time::ClockId::Monotonic),
        })
    }

    #[inline(always)]
    fn queue_rq(
        queue_data: Pin<&QueueData>,
        mut rq: Owned<mq::Request<Self>>,
        _is_last: bool,
    ) -> Result {
        if queue_data.memory_backed {
            let guard = queue_data.tree.lock.lock();
            let tree = queue_data.tree.tree.deref();
            let command = rq.command();
            let mut sector = rq.sector();

            for bio in rq.bio_iter_mut() {
                let mut segment_iter = bio.segment_iter();
                while let Some(segment) = segment_iter.next() {
                    let length = segment.len();
                    Self::transfer(command, tree, sector, segment)?;
                    sector += length as usize >> block::SECTOR_SHIFT;
                }
            }

            drop(guard);
        }

        match queue_data.irq_mode {
            IRQMode::None => rq.end_ok(),
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
        OwnableRefCounted::try_from_shared(rq)
            .map_err(|_e| kernel::error::code::EIO)
            .expect("Failed to complete request")
            .end_ok();
    }
}

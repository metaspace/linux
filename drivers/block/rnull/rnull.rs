// SPDX-License-Identifier: GPL-2.0

//! This is a Rust implementation of the C null block driver.

mod configfs;

use configfs::IRQMode;
use kernel::{
    bindings,
    block::{
        self,
        badblocks::{self, BadBlocks},
        bio::Segment,
        mq::{
            self,
            gen_disk::{
                self,
                GenDisk, //
            },
            Operations,
            TagSet, //
        },
        SECTOR_MASK, SECTOR_SHIFT,
    },
    error::{
        code,
        Result, //
    },
    ffi,
    new_mutex,
    new_xarray,
    page::{
        SafePage,
        PAGE_SIZE, //
    },
    pr_info,
    prelude::*,
    str::CString,
    sync::{
        aref::ARef,
        atomic::{
            ordering,
            Atomic, //
        },
        Arc,
        Mutex, //
    },
    time::{
        hrtimer::{
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
    xarray::XArray, //
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
                let disk = NullBlkDevice::new(NullBlkOptions {
                    name: &name,
                    block_size: *module_parameters::bs.value(),
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
}
struct NullBlkDevice;

impl NullBlkDevice {
    fn new(options: NullBlkOptions<'_>) -> Result<GenDisk<Self>> {
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
        } = options;

        let mut flags = mq::tag_set::Flags::default();

        if memory_backed {
            flags |= mq::tag_set::Flag::Blocking;
        }

        if no_sched {
            flags |= mq::tag_set::Flag::NoDefaultScheduler;
        }

        if home_node > kernel::num_online_nodes().try_into()? {
            return Err(code::EINVAL);
        }

        let tagset = Arc::pin_init(
            TagSet::new(submit_queues, 256, 1, home_node, flags),
            GFP_KERNEL,
        )?;

        let queue_data = Box::pin_init(
            pin_init!(QueueData {
                tree <- new_xarray!(kernel::xarray::AllocKind::Alloc),
                irq_mode,
                completion_time,
                memory_backed,
                block_size: block_size.into(),
                bad_blocks,
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

    #[inline(always)]
    fn write(tree: &Tree, mut sector: u64, mut segment: Segment<'_>) -> Result {
        while !segment.is_empty() {
            let page = NullBlockPage::new()?;
            let mut tree = tree.lock();

            let page_idx = sector >> block::PAGE_SECTORS_SHIFT;

            let page = if let Some(page) = tree.get_mut(page_idx as usize) {
                page
            } else {
                tree.store(page_idx as usize, page, GFP_NOIO)?;
                tree.get_mut(page_idx as usize).unwrap()
            };

            page.set_occupied(sector);
            let page_offset = (sector & u64::from(block::SECTOR_MASK)) << block::SECTOR_SHIFT;
            sector += segment.copy_to_page(page.page.get_pin_mut(), page_offset as usize) as u64
                >> block::SECTOR_SHIFT;
        }
        Ok(())
    }

    #[inline(always)]
    fn read(tree: &Tree, mut sector: u64, mut segment: Segment<'_>) -> Result {
        let tree = tree.lock();

        while !segment.is_empty() {
            let idx = sector >> block::PAGE_SECTORS_SHIFT;

            if let Some(page) = tree.get(idx as usize) {
                let page_offset = (sector & u64::from(block::SECTOR_MASK)) << block::SECTOR_SHIFT;
                sector += segment.copy_from_page(&page.page, page_offset as usize) as u64
                    >> block::SECTOR_SHIFT;
            } else {
                sector += segment.zero_page() as u64 >> block::SECTOR_SHIFT;
            }
        }

        Ok(())
    }

    fn discard(tree: &Tree, mut sector: u64, sectors: u64, block_size: u64) -> Result {
        let mut remaining_bytes = sectors << SECTOR_SHIFT;
        let mut tree = tree.lock();

        while remaining_bytes > 0 {
            let page_idx = sector >> block::PAGE_SECTORS_SHIFT;
            let mut remove = false;
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
            sector += processed >> SECTOR_SHIFT;
            remaining_bytes -= processed;
        }

        Ok(())
    }

    #[inline(never)]
    fn transfer(
        command: bindings::req_op,
        tree: &Tree,
        sector: u64,
        segment: Segment<'_>,
    ) -> Result {
        match command {
            bindings::req_op_REQ_OP_WRITE => Self::write(tree, sector, segment)?,
            bindings::req_op_REQ_OP_READ => Self::read(tree, sector, segment)?,
            _ => (),
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
    page: Owned<SafePage>,
    status: u64,
}

impl NullBlockPage {
    fn new() -> Result<KBox<Self>> {
        Ok(KBox::new(
            Self {
                page: SafePage::alloc_page(GFP_NOIO | __GFP_ZERO)?,
                status: 0,
            },
            GFP_NOIO,
        )?)
    }

    fn set_occupied(&mut self, sector: u64) {
        let idx = sector & u64::from(SECTOR_MASK);
        self.status |= 1 << idx;
    }

    fn set_free(&mut self, sector: u64) {
        let idx = sector & u64::from(SECTOR_MASK);
        self.status &= !(1 << idx);
    }

    fn is_empty(&self) -> bool {
        self.status == 0
    }
}

type TreeNode = KBox<NullBlockPage>;
type Tree = XArray<TreeNode>;

#[pin_data]
struct QueueData {
    #[pin]
    tree: Tree,
    irq_mode: IRQMode,
    completion_time: Delta,
    memory_backed: bool,
    block_size: u64,
    bad_blocks: Arc<BadBlocks>,
}

#[pin_data]
struct Pdu {
    #[pin]
    timer: kernel::time::hrtimer::HrTimer<Self>,
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

#[vtable]
impl Operations for NullBlkDevice {
    type QueueData = Pin<KBox<QueueData>>;
    type RequestData = Pdu;

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
        if queue_data.bad_blocks.enabled() {
            let start = rq.sector();
            let end = start + u64::from(rq.sectors());
            if !matches!(
                queue_data.bad_blocks.check(start..end),
                badblocks::BlockStatus::None
            ) {
                rq.data_ref().error.store(1, ordering::Relaxed);
            }
        }

        // TODO: Skip IO if bad block.

        if queue_data.memory_backed {
            let tree = &queue_data.tree;
            let command = rq.command();
            let mut sector = rq.sector();

            if command == bindings::req_op_REQ_OP_DISCARD {
                Self::discard(tree, sector, rq.sectors().into(), queue_data.block_size)?;
            } else {
                for bio in rq.bio_iter_mut() {
                    let segment_iter = bio.segment_iter();
                    for segment in segment_iter {
                        let length = segment.len();
                        Self::transfer(command, tree, sector, segment)?;
                        sector += u64::from(length) >> block::SECTOR_SHIFT;
                    }
                }
            }
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

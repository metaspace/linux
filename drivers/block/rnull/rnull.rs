// SPDX-License-Identifier: GPL-2.0

//! This is a Rust implementation of the C null block driver.

mod configfs;

use configfs::IRQMode;
use kernel::{
    block::{
        self,
        mq::{
            self,
            gen_disk::{
                self,
                GenDisk, //
            },
            Operations,
            TagSet, //
        },
    },
    error::Result,
    new_mutex,
    pr_info,
    prelude::*,
    str::CString,
    sync::{
        aref::ARef,
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
    }, //
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
            description:  "Time in ns to complete a request in hardware. Default: 10,000ns",
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

                let disk = NullBlkDevice::new(
                    &name,
                    *module_parameters::bs.value(),
                    *module_parameters::rotational.value() != 0,
                    *module_parameters::gb.value() * 1024,
                    (*module_parameters::irqmode.value()).try_into()?,
                    Delta::from_nanos(completion_time),
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
    ) -> Result<GenDisk<Self>> {
        let tagset = Arc::pin_init(
            TagSet::new(1, 256, 1, mq::tag_set::Flags::default()),
            GFP_KERNEL,
        )?;

        let queue_data = Box::new(
            QueueData {
                irq_mode,
                completion_time,
            },
            GFP_KERNEL,
        )?;

        gen_disk::GenDiskBuilder::new()
            .capacity_sectors(capacity_mib << (20 - block::SECTOR_SHIFT))
            .logical_block_size(block_size)?
            .physical_block_size(block_size)?
            .rotational(rotational)
            .build(fmt!("{}", name.to_str()?), tagset, queue_data)
    }
}

struct QueueData {
    irq_mode: IRQMode,
    completion_time: Delta,
}

#[pin_data]
struct Pdu {
    #[pin]
    timer: kernel::time::hrtimer::HrTimer<Self>,
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
    type QueueData = KBox<QueueData>;
    type RequestData = Pdu;

    fn new_request_data() -> impl PinInit<Self::RequestData> {
        pin_init!(Pdu {
            timer <- kernel::time::hrtimer::HrTimer::new(),
        })
    }

    #[inline(always)]
    fn queue_rq(queue_data: &QueueData, rq: Owned<mq::Request<Self>>, _is_last: bool) -> Result {
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

    fn commit_rqs(_queue_data: &QueueData) {}

    fn complete(rq: ARef<mq::Request<Self>>) {
        OwnableRefCounted::try_from_shared(rq)
            .map_err(|_e| kernel::error::code::EIO)
            .expect("Failed to complete request")
            .end_ok();
    }
}

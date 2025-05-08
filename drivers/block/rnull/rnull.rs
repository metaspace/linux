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
            description:  "IRQ completion handler. 0-none, 1-softirq",
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
            for i in 0..(*module_parameters::nr_devices.value()) {
                let name = CString::try_from_fmt(fmt!("rnullb{}", i))?;

                let disk = NullBlkDevice::new(
                    &name,
                    *module_parameters::bs.value(),
                    *module_parameters::rotational.value() != 0,
                    *module_parameters::gb.value() * 1024,
                    (*module_parameters::irqmode.value()).try_into()?,
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
    ) -> Result<GenDisk<Self>> {
        let tagset = Arc::pin_init(TagSet::new(1, 256, 1), GFP_KERNEL)?;

        let queue_data = Box::new(QueueData { irq_mode }, GFP_KERNEL)?;

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
}

#[vtable]
impl Operations for NullBlkDevice {
    type QueueData = KBox<QueueData>;

    #[inline(always)]
    fn queue_rq(queue_data: &QueueData, rq: Owned<mq::Request<Self>>, _is_last: bool) -> Result {
        match queue_data.irq_mode {
            IRQMode::None => rq.end_ok(),
            IRQMode::Soft => mq::Request::complete(rq.into()),
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
